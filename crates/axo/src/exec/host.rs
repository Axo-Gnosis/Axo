//! Running a command on the host: process group, kill escalation, output capture.

use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::exec::EnvMap;

/// How long a group gets between SIGTERM and SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(1);

/// The first executable file named `name` on the `PATH` of `env`; relative entries
/// resolve against `cwd`.
pub fn find_on_path(name: &str, env: &EnvMap, cwd: &Path) -> Option<PathBuf> {
    let path = env.get(OsStr::new("PATH"))?;
    std::env::split_paths(path)
        .map(|dir| cwd.join(dir).join(name))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// A child running as the leader of its own process group. Dropping it while armed
/// SIGKILLs the whole group, so an abandoned call never leaves processes behind.
pub struct ProcessGroup {
    pub child: Child,
    pub pgid: Pid,
    armed: bool,
    /// Kills the group if axo dies; `None` when it could not be spawned.
    guard: Option<Guard>,
}

/// `sh -c 'read _; kill -KILL -- -<pgid>'` with its stdin piped from axo. If axo dies
/// (even `kill -9`) the kernel closes the pipe, `read` hits EOF and the guard kills the
/// group. PDEATHSIG cannot do this: bash forks every non-final command of a script and
/// the death signal is cleared across fork, so `sleep 20; echo` would outlive axo.
struct Guard {
    child: Child,
    /// Held open for the call; closing it fires the guard.
    _stdin: ChildStdin,
}

impl Guard {
    fn spawn(pgid: Pid) -> io::Result<Self> {
        let mut child = Command::new("sh")
            .args(["-c", r#"read _; kill -KILL -- -"$0""#])
            .arg(pgid.as_raw_nonzero().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("guard has no stdin"))?;
        Ok(Self {
            child,
            _stdin: stdin,
        })
    }

    /// SIGKILLs the guard before its pipe closes, so it never reaches its `kill`
    /// (a SIGKILLed process runs no further user code). Dropping the tokio `Child`
    /// leaves the zombie to tokio's orphan reaper.
    fn disarm(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.try_wait();
    }
}

impl ProcessGroup {
    /// Spawns `command` as the leader of a new process group, plus its guard (which
    /// stays in axo's own process group).
    pub fn spawn(mut command: Command) -> io::Result<Self> {
        command.process_group(0);
        let child = command.spawn()?;
        let pgid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::other("spawned process has no pid"))?;
        let guard = Guard::spawn(pgid)
            .inspect_err(|err| tracing::warn!(%err, "cannot spawn the process-group guard"))
            .ok();
        Ok(Self {
            child,
            pgid,
            armed: true,
            guard,
        })
    }

    /// Leaves the group alone on drop (the command finished; background jobs it started
    /// keep running, as in pi).
    pub fn release(&mut self) {
        self.armed = false;
        if let Some(guard) = self.guard.take() {
            guard.disarm();
        }
    }

    /// Takes the child's piped stdout and stderr and forwards their chunks, interleaved
    /// as they arrive, into one channel that closes when both pipes reach EOF. Dropping
    /// the `JoinSet` stops the readers.
    pub fn capture_output(&mut self) -> (mpsc::Receiver<Vec<u8>>, JoinSet<()>) {
        let (tx, rx) = mpsc::channel(64);
        let mut readers = JoinSet::new();
        if let Some(stdout) = self.child.stdout.take() {
            readers.spawn(pump(stdout, tx.clone()));
        }
        if let Some(stderr) = self.child.stderr.take() {
            readers.spawn(pump(stderr, tx));
        }
        (rx, readers)
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            guard.disarm();
        }
        if self.armed {
            let _ = kill_process_group(self.pgid, Signal::KILL);
        }
    }
}

async fn pump(mut reader: impl AsyncRead + Unpin + Send + 'static, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0; 64 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Stops a process group: SIGTERM, up to [`KILL_GRACE`] for every member to exit, then
/// SIGKILL. The caller must keep reaping the group leader meanwhile (an unreaped zombie
/// still counts as a member), so run this in its own task.
pub async fn terminate_group(pgid: Pid) {
    if kill_process_group(pgid, Signal::TERM).is_err() {
        return;
    }
    let deadline = Instant::now() + KILL_GRACE;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if test_kill_process_group(pgid).is_err() {
            return;
        }
    }
    let _ = kill_process_group(pgid, Signal::KILL);
}
