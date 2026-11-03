//! The embedded command supervisor: a host routes one of its own command lines to
//! [`run`], and axo execs that command to put each job in its own cgroup v2 directory
//! and kill that directory if axo dies. Sync std + rustix; no tokio.
//!
//! - `guard <job-dir>` stays in axo's cgroup. It prints `ready`, then watches axo (pidfd)
//!   and its control pipe (stdin). axo dying or the pipe closing kills and removes the
//!   job directory; `release` means axo already removed it.
//! - `enter <job-dir> <program> [args…]` reads the environment from stdin, moves itself
//!   into the job directory and execs the command. The environment travels over stdin so
//!   nothing in it (`LD_PRELOAD`, …) runs before the process is inside its cgroup.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno;
use rustix::process::{PidfdFlags, getppid, pidfd_open};

/// The control message axo sends once it removed the job directory itself.
pub(crate) const RELEASE: &[u8] = b"release\n";

/// Where the cgroup v2 hierarchy is mounted.
pub(crate) const CGROUP_MOUNT: &str = "/sys/fs/cgroup";

/// Cleanup rounds before the guardian gives up on a directory that stays busy.
const CLEANUP_ROUNDS: u32 = 50;

/// Runs one supervisor invocation and returns the exit code the host's `main` must
/// return. `args` are the arguments after the host's own routing prefix: `guard <job-dir>`
/// or `enter <job-dir> <program> [args…]`. Call it first in `main`, before starting any
/// runtime, thread or host setup, and only for processes axo spawned directly (an
/// `exec`-only wrapper is fine; a forking wrapper breaks the guard's parent check).
pub fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let args: Vec<OsString> = args.into_iter().collect();
    match (args.first().and_then(|mode| mode.to_str()), args.get(1)) {
        (Some("guard"), Some(dir)) if args.len() == 2 => guard(Path::new(dir)),
        (Some("enter"), Some(dir)) if args.len() >= 3 => {
            enter(Path::new(dir), &args[2], &args[3..])
        }
        _ => {
            eprintln!("axo supervisor: usage: guard <job-dir> | enter <job-dir> <program> [args…]");
            ExitCode::from(2)
        }
    }
}

fn guard(dir: &Path) -> ExitCode {
    // Checking the parent again after `pidfd_open` closes the race where axo died (and we
    // were reparented) between `getppid` and `pidfd_open`.
    let pidfd = getppid().and_then(|parent| {
        pidfd_open(parent, PidfdFlags::empty())
            .ok()
            .filter(|_| getppid() == Some(parent))
    });
    let Some(pidfd) = pidfd else {
        cleanup(dir);
        return ExitCode::FAILURE;
    };
    let mut stdout = io::stdout().lock();
    if stdout
        .write_all(b"ready\n")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        cleanup(dir);
        return ExitCode::FAILURE;
    }
    drop(stdout);

    let stdin = io::stdin();
    let mut message = Vec::new();
    loop {
        let mut fds = [
            PollFd::new(&stdin, PollFlags::IN),
            PollFd::new(&pidfd, PollFlags::IN),
        ];
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(_) => return cleanup(dir),
        }
        // Readable pidfd: axo exited.
        if !fds[1].revents().is_empty() {
            return cleanup(dir);
        }
        if fds[0].revents().is_empty() {
            continue;
        }
        let mut chunk = [0u8; 64];
        match rustix::io::read(&stdin, &mut chunk[..]) {
            Err(Errno::INTR | Errno::AGAIN) => continue,
            // EOF or error: axo's handle was dropped (task, runtime or process gone).
            Ok(0) | Err(_) => return cleanup(dir),
            Ok(n) => message.extend_from_slice(&chunk[..n]),
        }
        if message.contains(&b'\n') || message.len() >= RELEASE.len() {
            // Anything but `release` is a protocol error: fail closed.
            return if message == RELEASE {
                ExitCode::SUCCESS
            } else {
                cleanup(dir)
            };
        }
    }
}

/// Kills everything in `dir` and removes it, retrying while the kernel reports it busy.
fn cleanup(dir: &Path) -> ExitCode {
    for _ in 0..CLEANUP_ROUNDS {
        if let Err(err) = kill(dir)
            && err.kind() == io::ErrorKind::NotFound
        {
            return ExitCode::SUCCESS;
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while populated(dir) && Instant::now() < deadline {
            sleep(Duration::from_millis(10));
        }
        match fs::remove_dir(dir) {
            Ok(()) => return ExitCode::SUCCESS,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return ExitCode::SUCCESS,
            Err(err) if err.kind() == io::ErrorKind::ResourceBusy => {}
            Err(_) => return ExitCode::FAILURE,
        }
    }
    ExitCode::FAILURE
}

fn enter(dir: &Path, program: &OsStr, args: &[OsString]) -> ExitCode {
    let mut input = Vec::new();
    if let Err(err) = io::stdin().lock().read_to_end(&mut input) {
        eprintln!("axo supervisor: cannot read the environment: {err}");
        return ExitCode::from(125);
    }
    let env = input.split(|&byte| byte == 0).filter_map(|record| {
        let eq = record
            .iter()
            .position(|&byte| byte == b'=')
            .filter(|&eq| eq > 0)?;
        Some((
            OsStr::from_bytes(&record[..eq]),
            OsStr::from_bytes(&record[eq + 1..]),
        ))
    });

    if let Err(err) = fs::write(dir.join("cgroup.procs"), "0") {
        eprintln!("axo supervisor: cannot enter {}: {err}", dir.display());
        return ExitCode::from(125);
    }
    let inside = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|contents| cgroup_dir(&contents))
        .is_some_and(|own| own == dir);
    if !inside {
        eprintln!("axo supervisor: not in {}", dir.display());
        return ExitCode::from(125);
    }

    let err = Command::new(program)
        .args(args)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .exec();
    eprintln!("axo supervisor: cannot run {}: {err}", program.display());
    ExitCode::from(if err.kind() == io::ErrorKind::NotFound {
        127
    } else {
        126
    })
}

/// The cgroup v2 directory named by a `/proc/<pid>/cgroup` file's `0::<path>` line.
pub(crate) fn cgroup_dir(proc_cgroup: &str) -> Option<PathBuf> {
    let path = proc_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?;
    Some(Path::new(CGROUP_MOUNT).join(path.trim_start_matches('/')))
}

/// Writes `1` to `dir/cgroup.kill`, SIGKILLing every process in the cgroup.
pub(crate) fn kill(dir: &Path) -> io::Result<()> {
    fs::write(dir.join("cgroup.kill"), "1")
}

/// Whether `dir/cgroup.events` reports `populated 1`; false when unreadable.
pub(crate) fn populated(dir: &Path) -> bool {
    fs::read_to_string(dir.join("cgroup.events"))
        .is_ok_and(|events| events.lines().any(|line| line == "populated 1"))
}
