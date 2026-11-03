//! Running a command on the host: each command runs in its own container, a process
//! group or a cgroup v2 job directory depending on the host-chosen [`Supervision`]
//! ([`ProcessSupervisor`]), with kill escalation and output capture.

use std::ffi::{OsStr, OsString};
use std::fmt::Display;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use rustix::io::Errno;
use rustix::process::{
    Pid, PidfdFlags, Signal, WaitOptions, kill_process_group, pidfd_open, pidfd_send_signal,
    test_kill_process_group, waitpgid,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{OnceCell, mpsc};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use crate::exec::EnvMap;
use crate::exec::supervisor::{self, CGROUP_MOUNT, RELEASE};

/// How long a job's processes get between SIGTERM and SIGKILL.
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

/// How axo runs local commands (`bash`, dev-shell capture, host tools using
/// `SessionTools::supervisor`). Chosen by the embedding host; not a settings key.
#[derive(Debug, Clone, Default)]
pub enum Supervision {
    /// Each command leads its own process group; stopping or dropping it signals the
    /// group. A best-effort `sh` guard kills the group if the host dies. Descendants
    /// that leave the group (`setsid`, double fork) are not tracked.
    #[default]
    ProcessGroup,
    /// Each command gets its own cgroup v2 job directory, watched by a supervisor
    /// process the host provides: axo runs `program args… guard <dir>` and
    /// `program args… enter <dir> <command…>`, and the host must route those to
    /// [`crate::exec::supervisor::run`]. Fails closed when unavailable.
    Cgroup {
        /// The executable; a bare name is resolved on axo's own PATH at spawn.
        program: PathBuf,
        /// Arguments before axo's (`guard …` / `enter …`), e.g. the host's route word.
        args: Vec<OsString>,
        /// Delegated cgroup v2 directory for job cgroups; `None` uses axo's own cgroup.
        cgroup_root: Option<PathBuf>,
    },
}

/// One command: `program` runs with exactly `env` in `cwd`.
#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: EnvMap,
}

/// Runs every local command in its own container, as selected by [`Supervision`]:
/// a process group, or a cgroup v2 job directory watched by a host-provided
/// `guard` process, where whichever of axo and the guardian survives the other kills
/// the job. Cgroup mode has no fallback: without a usable cgroup root, commands fail.
pub struct ProcessSupervisor {
    supervision: Supervision,
    /// The cgroup root, checked on the first cgroup-mode command and then cached;
    /// failures are deterministic too.
    ready: OnceCell<Result<PathBuf, String>>,
}

impl ProcessSupervisor {
    /// Checks nothing yet: hosts that never run commands keep working without a
    /// delegated cgroup.
    pub fn new(supervision: Supervision) -> Arc<Self> {
        Arc::new(Self {
            supervision,
            ready: OnceCell::new(),
        })
    }

    /// Starts `spec` in a fresh container. Cancelling the returned future at any point
    /// leaves nothing behind.
    pub async fn spawn(&self, spec: ExecSpec) -> Result<Process, String> {
        match &self.supervision {
            Supervision::ProcessGroup => spawn_group(spec),
            Supervision::Cgroup {
                program,
                args,
                cgroup_root,
            } => {
                let root = self
                    .ready
                    .get_or_init(|| async { probe(cgroup_root.as_deref()) })
                    .await
                    .as_ref()
                    .map_err(Clone::clone)?;
                spawn_cgroup(program, args, root, spec).await
            }
        }
    }

    /// Runs to completion like `Command::output`, then kills anything left in the
    /// container and closes it. Reaching `deadline` kills the job and returns
    /// [`OutputError::TimedOut`].
    pub async fn output(
        &self,
        spec: ExecSpec,
        deadline: Option<Instant>,
    ) -> Result<Output, OutputError> {
        let program = spec.program.clone();
        let mut process = self.spawn(spec).await.map_err(OutputError::Failed)?;
        let (stdout, stderr) = (process.stdout.take(), process.stderr.take());
        let run = async {
            let (status, stdout, stderr) =
                tokio::join!(process.wait(), read_all(stdout), read_all(stderr));
            status.map(|status| Output {
                status,
                stdout,
                stderr,
            })
        };
        let result = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, run).await.ok(),
            None => Some(run.await),
        };
        process.container().kill();
        process.close().await;
        match result {
            None => Err(OutputError::TimedOut),
            Some(Ok(output)) => Ok(output),
            Some(Err(err)) => Err(OutputError::Failed(format!(
                "Failed to wait for {}: {err}",
                program.display()
            ))),
        }
    }
}

/// Why [`ProcessSupervisor::output`] returned no output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputError {
    /// `deadline` passed; the job was killed.
    TimedOut,
    /// Spawning or waiting failed; the message says why.
    Failed(String),
}

impl Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutputError::TimedOut => f.write_str("timed out"),
            OutputError::Failed(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for OutputError {}

/// Resolves the cgroup root and checks that job cgroups can be created under it.
fn probe(cgroup_root: Option<&Path>) -> Result<PathBuf, String> {
    let root = match cgroup_root {
        Some(root) => root.to_owned(),
        None => std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|contents| supervisor::cgroup_dir(&contents))
            .ok_or_else(|| {
                unavailable(
                    Path::new(CGROUP_MOUNT),
                    "/proc/self/cgroup names no cgroup v2 path",
                )
            })?,
    };
    let root = std::fs::canonicalize(&root).map_err(|err| unavailable(&root, err))?;
    if !root.starts_with(CGROUP_MOUNT) {
        return Err(unavailable(
            &root,
            "it is not inside the cgroup v2 hierarchy at /sys/fs/cgroup",
        ));
    }
    let probe = root.join(format!("axo-probe-{}", Uuid::now_v7()));
    std::fs::create_dir(&probe).map_err(|err| unavailable(&root, err))?;
    let has_kill = probe.join("cgroup.kill").try_exists();
    let removed = std::fs::remove_dir(&probe);
    match has_kill {
        Ok(true) => {}
        Ok(false) => {
            return Err(unavailable(
                &root,
                "it has no cgroup.kill (cgroup v2 on Linux 5.14+ required)",
            ));
        }
        Err(err) => return Err(unavailable(&root, err)),
    }
    removed.map_err(|err| unavailable(&root, err))?;
    Ok(root)
}

/// Starts `spec` as the leader of a new process group. Nothing is awaited before the
/// `Process` owns the group, so cancellation cannot leak anything.
fn spawn_group(spec: ExecSpec) -> Result<Process, String> {
    let failed = |err: String| format!("Failed to start {}: {err}", spec.program.display());
    let mut root = Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|err| failed(err.to_string()))?;
    let Some(pgid) = root
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
    else {
        let _ = root.start_kill();
        return Err(failed("spawned process has no pid".to_owned()));
    };
    // `sh -c 'read _; kill -KILL -- -<pgid>'` with its stdin piped from axo. If axo dies
    // (even `kill -9`) the kernel closes the pipe, `read` hits EOF and the guard kills
    // the group. PDEATHSIG cannot do this: bash forks every non-final command of a
    // script and the death signal is cleared across fork.
    // ponytail: a pgid freed and reused between the group emptying and close() disarming the guard could be killed if the host dies in that window; cgroup mode has no such window
    let guard = Command::new("sh")
        .args(["-c", r#"read _; kill -KILL -- -"$0""#])
        .arg(pgid.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .inspect_err(|err| tracing::warn!(%err, "cannot spawn the process-group guard"))
        .ok()
        .and_then(|mut child| {
            let stdin = child.stdin.take()?;
            Some((child, stdin))
        });
    Ok(Process {
        stdout: root.stdout.take(),
        stderr: root.stderr.take(),
        root,
        watch: Watch::Group(guard),
        container: Container(Arc::new(Scope::Group(pgid))),
        closed: false,
    })
}

/// Starts `spec` inside a fresh job cgroup under `root`, watched by a guard process
/// run through the host's `program args…`.
async fn spawn_cgroup(
    program: &Path,
    args: &[OsString],
    root: &Path,
    spec: ExecSpec,
) -> Result<Process, String> {
    let dir = root.join(format!("axo-job-{}", Uuid::now_v7()));
    std::fs::create_dir(&dir).map_err(|err| unavailable(root, err))?;
    // Until `Process` owns the directory, failing or cancelled paths remove it here.
    let mut empty = EmptyDir(Some(&dir));

    // No `kill_on_drop`: dropping the guardian's stdin is what makes it clean up.
    let mut guardian = Command::new(program)
        .args(args)
        .arg("guard")
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|err| {
            format!(
                "Command supervisor {} failed to start: {err}",
                program.display()
            )
        })?;
    let control = guardian.stdin.take();
    let announce = guardian.stdout.take();
    let line = tokio::time::timeout(Duration::from_secs(5), async {
        let mut line = String::new();
        BufReader::new(announce?).read_line(&mut line).await.ok()?;
        Some(line)
    })
    .await;
    let Some(control) = control.filter(|_| matches!(&line, Ok(Some(line)) if line == "ready\n"))
    else {
        let _ = guardian.start_kill();
        let _ = guardian.wait().await;
        return Err(format!(
            "Command supervisor {} did not report ready; the host must route these arguments to axo::exec::supervisor::run",
            program.display()
        ));
    };

    // The supervisor inherits axo's own (trusted) environment, which a host
    // executable may need to start at all. The command environment travels over
    // stdin and is applied only after cgroup entry, at `exec`.
    let root = Command::new(program)
        .args(args)
        .arg("enter")
        .arg(&dir)
        .arg(&spec.program)
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        // Before it reads its environment to EOF the root is outside the cgroup, so
        // only a direct kill reaches it.
        .kill_on_drop(true)
        .spawn();
    let mut root = match root {
        Ok(root) => root,
        Err(err) => {
            drop(empty);
            let mut control = control;
            let _ = control.write_all(RELEASE).await;
            drop(control);
            let _ = guardian.wait().await;
            return Err(format!("Failed to start {}: {err}", spec.program.display()));
        }
    };

    // Declared before `process`: a cancelled write drops `process` first, SIGKILLing
    // the root while it still waits for EOF, so a partial environment never runs.
    let mut env_pipe = root.stdin.take();
    empty.0 = None;
    drop(empty);
    let process = Process {
        stdout: root.stdout.take(),
        stderr: root.stderr.take(),
        root,
        watch: Watch::Guardian {
            child: guardian,
            exited: false,
            control: Some(control),
        },
        container: Container(Arc::new(Scope::Cgroup(dir))),
        closed: false,
    };
    if let Some(pipe) = env_pipe.as_mut() {
        // A failed write means the root already died; `wait` reports its status.
        let _ = pipe.write_all(&env_records(&spec.env)).await;
    }
    drop(env_pipe);
    Ok(process)
}

fn unavailable(root: &Path, err: impl Display) -> String {
    format!(
        "Command supervision unavailable: cannot create a job cgroup under {}: {err}. Run the host in a delegated cgroup (systemd: Delegate=yes), set cgroup_root in Supervision::Cgroup, or use Supervision::ProcessGroup.",
        root.display()
    )
}

/// `KEY=VALUE\0` records of raw `OsStr` bytes, as the supervisor's `enter` reads them.
fn env_records(env: &EnvMap) -> Vec<u8> {
    let mut records = Vec::new();
    for (key, value) in env {
        records.extend_from_slice(key.as_bytes());
        records.push(b'=');
        records.extend_from_slice(value.as_bytes());
        records.push(0);
    }
    records
}

async fn read_all(reader: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut reader) = reader {
        let _ = reader.read_to_end(&mut buf).await;
    }
    buf
}

/// Removes a still-empty job directory on drop unless disarmed.
struct EmptyDir<'a>(Option<&'a Path>);

impl Drop for EmptyDir<'_> {
    fn drop(&mut self) {
        if let Some(dir) = self.0 {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// A command running in its own container. Dropping it before [`Process::close`]
/// finishes kills everything in the container; in cgroup mode the guardian then
/// removes the directory.
pub struct Process {
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    root: Child,
    watch: Watch,
    container: Container,
    closed: bool,
}

/// The process watching over a [`Process`] on the host's behalf.
enum Watch {
    /// The cgroup guardian (`guard <dir>`).
    Guardian {
        child: Child,
        exited: bool,
        /// The guardian's stdin: `release` after a clean close, EOF (drop) otherwise.
        control: Option<ChildStdin>,
    },
    /// The best-effort process-group guard with the stdin that, once closed, fires it;
    /// `None` when it could not be spawned.
    Group(Option<(Child, ChildStdin)>),
}

impl Watch {
    /// Stands the watcher down after a clean close: the guardian is told to release
    /// and reaped; the group guard is SIGKILLed and reaped while its stdin is still
    /// open, so it never reaches its `kill` (a SIGKILLed process runs no further user
    /// code) and leaves no zombie.
    async fn release(&mut self) {
        match self {
            Watch::Guardian { child, control, .. } => {
                if let Some(mut control) = control.take() {
                    let _ = control.write_all(RELEASE).await;
                }
                let _ = child.wait().await;
            }
            Watch::Group(guard) => {
                if let Some((mut child, stdin)) = guard.take() {
                    let _ = child.kill().await;
                    drop(stdin);
                }
            }
        }
    }
}

impl Process {
    /// Takes the root's piped stdout and stderr and forwards their chunks, interleaved
    /// as they arrive, into one channel that closes when both pipes reach EOF. Dropping
    /// the `JoinSet` stops the readers.
    pub fn capture_output(&mut self) -> (mpsc::Receiver<Vec<u8>>, JoinSet<()>) {
        capture(self.stdout.take(), self.stderr.take())
    }

    /// Waits for the root process. In cgroup mode a guardian that dies first takes the
    /// job with it. Cancel-safe.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        tokio::select! {
            status = self.root.wait() => status,
            _ = guardian_lost(&mut self.watch, &self.container) => {
                self.root.wait().await
            }
        }
    }

    /// Resolves once the cgroup guardian has died, after killing the job: without its
    /// watcher the job must not keep running unsupervised. Never resolves while the
    /// guardian lives, nor again once its death was handled, and never in process-group
    /// mode, whose guard is best effort. Cancel-safe.
    pub async fn guardian_lost(&mut self) {
        guardian_lost(&mut self.watch, &self.container).await;
    }

    pub fn container(&self) -> Container {
        self.container.clone()
    }

    /// Whether processes besides the reaped root remain in the container; call only
    /// after `wait` returned.
    pub(crate) fn has_leftovers(&self) -> bool {
        if let Scope::Group(pgid) = &*self.container.0 {
            // Reaps the group's zombies that are our own children, which happens when
            // the host is PID 1 or a child subreaper; otherwise ECHILD ends this at once.
            while let Ok(Some(_)) = waitpgid(*pgid, WaitOptions::NOHANG) {}
        }
        self.container.populated()
    }

    /// Waits for the root, then for every leftover to exit. Cancel-safe.
    pub(crate) async fn wait_empty(&mut self) {
        let _ = self.wait().await;
        // ponytail: polling, cgroup.events inotify if many long-lived jobs
        while self.has_leftovers() {
            tokio::select! {
                _ = sleep(Duration::from_millis(250)) => {}
                _ = self.guardian_lost() => {}
            }
        }
    }

    /// Waits for the root and every leftover to exit, removes the job directory (cgroup
    /// mode) and stands the watcher down. Callers that want leftovers dead call
    /// `container().kill()` first. Cancelling it falls back to the drop path.
    pub async fn close(mut self) {
        self.wait_empty().await;
        if let Scope::Cgroup(dir) = &*self.container.0 {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match std::fs::remove_dir(dir) {
                    Ok(()) => break,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => break,
                    Err(err)
                        if err.kind() == io::ErrorKind::ResourceBusy
                            && Instant::now() < deadline =>
                    {
                        self.container.kill();
                        sleep(Duration::from_millis(20)).await;
                    }
                    // Still there: dropping `self` kills it and the guardian's EOF
                    // cleanup keeps retrying.
                    Err(_) => return,
                }
            }
        }
        self.watch.release().await;
        self.closed = true;
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Cgroup mode: dropping `control` (EOF) also makes the guardian kill and remove
        // the directory, and `kill_on_drop` covers a root that has not entered it yet.
        // Group mode: the guard's stdin EOF kills the same, already-dying group, which
        // is harmless; tokio's orphan reaper collects the guard.
        if !self.closed {
            self.container.kill();
        }
    }
}

fn capture(
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
) -> (mpsc::Receiver<Vec<u8>>, JoinSet<()>) {
    let (tx, rx) = mpsc::channel(64);
    let mut readers = JoinSet::new();
    if let Some(stdout) = stdout {
        readers.spawn(pump(stdout, tx.clone()));
    }
    if let Some(stderr) = stderr {
        readers.spawn(pump(stderr, tx));
    }
    (rx, readers)
}

/// One command's container: its process group or its job cgroup directory.
#[derive(Debug, Clone)]
pub struct Container(Arc<Scope>);

#[derive(Debug)]
enum Scope {
    Cgroup(PathBuf),
    Group(Pid),
}

impl Container {
    /// SIGKILLs everything in the container (`cgroup.kill` or a group kill); errors are
    /// ignored.
    pub fn kill(&self) {
        match &*self.0 {
            Scope::Cgroup(dir) => {
                let _ = supervisor::kill(dir);
            }
            Scope::Group(pgid) => {
                let _ = kill_process_group(*pgid, Signal::KILL);
            }
        }
    }

    /// Whether any process is left in the container; false when unreadable. Group
    /// members that refuse a signal (EPERM) still count as present.
    pub fn populated(&self) -> bool {
        match &*self.0 {
            Scope::Cgroup(dir) => supervisor::populated(dir),
            Scope::Group(pgid) => !matches!(test_kill_process_group(*pgid), Err(Errno::SRCH)),
        }
    }

    /// SIGTERMs every member, gives them up to [`KILL_GRACE`] to exit, then kills the
    /// rest. In cgroup mode a pidfd pins each member's identity: a pid reused after
    /// `cgroup.procs` was read makes the send fail with ESRCH instead of hitting another
    /// process. In group mode the caller must keep reaping the leader meanwhile: an
    /// unreaped zombie still counts as a member.
    pub async fn terminate(&self) {
        match &*self.0 {
            Scope::Cgroup(dir) => {
                let procs = std::fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default();
                for pid in procs
                    .lines()
                    .filter_map(|line| Pid::from_raw(line.parse().ok()?))
                {
                    let Ok(pidfd) = pidfd_open(pid, PidfdFlags::empty()) else {
                        continue;
                    };
                    if self.contains(pid) {
                        // ESRCH and EPERM are covered by the `cgroup.kill` below.
                        let _ = pidfd_send_signal(&pidfd, Signal::TERM);
                    }
                }
            }
            Scope::Group(pgid) => {
                if kill_process_group(*pgid, Signal::TERM) == Err(Errno::SRCH) {
                    return;
                }
            }
        }
        let deadline = Instant::now() + KILL_GRACE;
        while self.populated() && Instant::now() < deadline {
            sleep(Duration::from_millis(20)).await;
        }
        self.kill();
    }

    /// Whether `pid` is in this job cgroup; cgroup mode only.
    fn contains(&self, pid: Pid) -> bool {
        let Scope::Cgroup(dir) = &*self.0 else {
            return false;
        };
        std::fs::read_to_string(format!("/proc/{}/cgroup", pid.as_raw_nonzero()))
            .ok()
            .and_then(|contents| supervisor::cgroup_dir(&contents))
            .is_some_and(|found| found == *dir)
    }
}

/// Resolves once a cgroup guardian has died, after killing the job; pending forever in
/// process-group mode or once the death was handled.
async fn guardian_lost(watch: &mut Watch, container: &Container) {
    let Watch::Guardian { child, exited, .. } = watch else {
        return std::future::pending().await;
    };
    if *exited {
        return std::future::pending().await;
    }
    let _ = child.wait().await;
    *exited = true;
    container.kill();
}
