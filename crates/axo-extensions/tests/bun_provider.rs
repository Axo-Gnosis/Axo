//! Real Bun acceptance: a Rust host loads `tests/fixtures/lab-provider` (and renamed or
//! broken copies) through the public `Axo` + `Extensions` APIs against a loopback lab server,
//! `tests/fixtures/lab-session` into scripted real sessions, and `tests/fixtures/lab-deps`
//! with its npm dependency installed by axo.
//!
//! Needs `AXO_TEST_BUN=/absolute/path/to/bun`: unset prints an explicit skip, set but
//! unusable fails. Process-group cases always run; `AXO_TEST_CGROUP=1` (inside a delegated
//! cgroup v2) reruns them under `Supervision::Cgroup` with this binary as supervisor, routed
//! through its `axo-supervisor` command line. Harness-less: libtest flags are ignored.

#[path = "bun_provider/host.rs"]
mod host;
#[path = "bun_provider/install.rs"]
mod install;
#[path = "bun_provider/lifecycle.rs"]
mod lifecycle;
#[path = "bun_provider/oauth.rs"]
mod oauth;
#[path = "bun_provider/server.rs"]
mod server;
#[path = "bun_provider/sessions.rs"]
mod sessions;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use axo::{ProcessSupervisor, Supervision, exec::host::ExecSpec};

const ROUTE: &str = "axo-supervisor";
/// Wall-clock bound for the whole binary, so a hang fails instead of stalling CI.
const LIMIT: Duration = Duration::from_secs(900);

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|first| first == ROUTE) {
        return axo::exec::supervisor::run(args);
    }
    let Some(bun) = std::env::var_os("AXO_TEST_BUN") else {
        println!(
            "bun_provider: SKIPPED, no real-process check ran (set AXO_TEST_BUN=/absolute/path/to/bun)"
        );
        return ExitCode::SUCCESS;
    };
    std::thread::spawn(|| {
        std::thread::sleep(LIMIT);
        eprintln!("bun_provider: timed out after {} s", LIMIT.as_secs());
        std::process::exit(1);
    });
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let bun = match runtime.block_on(usable(PathBuf::from(bun))) {
        Ok(bun) => bun,
        Err(problem) => {
            eprintln!("bun_provider: AXO_TEST_BUN is set but unusable: {problem}");
            return ExitCode::FAILURE;
        }
    };
    let mut modes = vec![("group", Supervision::ProcessGroup)];
    if std::env::var_os("AXO_TEST_CGROUP").is_some() {
        modes.push((
            "cgroup",
            Supervision::Cgroup {
                program: std::env::current_exe().unwrap(),
                args: vec![OsString::from(ROUTE)],
                cgroup_root: None,
            },
        ));
    }
    runtime.block_on(async {
        for (name, mode) in &modes {
            oauth::run(&bun, mode.clone()).await;
            println!("bun_provider: oauth ({name}) ... ok");
            lifecycle::run(&bun, mode.clone()).await;
            println!("bun_provider: lifecycle ({name}) ... ok");
            sessions::run(&bun, mode.clone()).await;
            println!("bun_provider: sessions ({name}) ... ok");
            install::run(&bun, mode.clone()).await;
            println!("bun_provider: install ({name}) ... ok");
        }
    });
    drop(runtime);
    if modes.len() == 1 {
        println!(
            "bun_provider: cgroup cases skipped (set AXO_TEST_CGROUP=1 inside a delegated cgroup to run them)"
        );
    }
    ExitCode::SUCCESS
}

/// `bun` must be an absolute path that actually runs `--version`.
async fn usable(bun: PathBuf) -> Result<PathBuf, String> {
    if !bun.is_absolute() {
        return Err(format!("{} is not an absolute path", bun.display()));
    }
    let spec = ExecSpec {
        program: bun.clone(),
        args: vec!["--version".into()],
        cwd: "/".into(),
        env: Default::default(),
        sandbox: None,
    };
    let deadline = tokio::time::Instant::now() + host::WAIT_LIMIT;
    let output = ProcessSupervisor::new(Supervision::ProcessGroup)
        .output(spec, Some(deadline))
        .await
        .map_err(|error| format!("{} --version: {error}", bun.display()))?;
    if !output.status.success() {
        return Err(format!("{} --version: {}", bun.display(), output.status));
    }
    Ok(bun)
}
