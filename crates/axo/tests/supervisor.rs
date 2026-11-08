//! Native job supervision, exercised by a reference host embedding: this test binary routes
//! its own `axo-supervisor` command line to `axo::exec::supervisor::run`, so cgroup mode runs
//! with the test executable as guard and entry process. Process-group cases always run;
//! cgroup cases need `AXO_TEST_CGROUP=1` inside a delegated cgroup.

mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use axo::StaticEnv;
use axo::exec::host::{ExecSpec, ProcessSupervisor, Supervision, find_on_path};
use common::dead;
use tokio::io::{AsyncBufReadExt, BufReader};

const ROUTE: &str = "axo-supervisor";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    if args.next().is_some_and(|first| first == ROUTE) {
        return axo::exec::supervisor::run(args);
    }
    // Any other arguments are libtest flags, which a harness-less test ignores.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async {
        unusable_root_fails_closed().await;
        println!("supervisor: unusable_root_fails_closed ... ok");
        dropping_a_group_process_kills_its_group().await;
        println!("supervisor: dropping_a_group_process_kills_its_group ... ok");
        if std::env::var_os("AXO_TEST_CGROUP").is_some() {
            dropping_a_process_kills_detached_descendants().await;
            println!("supervisor: dropping_a_process_kills_detached_descendants ... ok");
        } else {
            println!(
                "supervisor: cgroup cases skipped (set AXO_TEST_CGROUP=1 inside a delegated cgroup to run them)"
            );
        }
        // Last: becoming a child subreaper changes how every later orphan is reaped.
        closing_a_group_process_reaps_adopted_leftovers().await;
        println!("supervisor: closing_a_group_process_reaps_adopted_leftovers ... ok");
    });
    ExitCode::SUCCESS
}

fn bash(script: &str, cwd: &Path) -> ExecSpec {
    let env = StaticEnv::host().0;
    ExecSpec {
        program: find_on_path("bash", &env, Path::new("/")).expect("bash on PATH"),
        args: vec!["-c".into(), script.into()],
        cwd: cwd.to_path_buf(),
        env: (*env).clone(),
    }
}

/// Cgroup supervision with this test binary as the host-provided supervisor.
fn cgroup() -> Supervision {
    Supervision::Cgroup {
        program: std::env::current_exe().unwrap(),
        args: vec![OsString::from(ROUTE)],
        cgroup_root: None,
    }
}

/// The cgroup v2 directory `pid` lives in.
fn cgroup_dir(pid: u32) -> PathBuf {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .expect("cgroup v2 entry");
    Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'))
}

/// Reads the pid the spawned script echoes on its first stdout line.
async fn first_pid(process: &mut axo::exec::host::Process) -> u32 {
    let mut line = String::new();
    BufReader::new(process.stdout.take().unwrap())
        .read_line(&mut line)
        .await
        .unwrap();
    line.trim().parse().unwrap()
}

async fn dropping_a_process_kills_detached_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let supervisor = ProcessSupervisor::new(cgroup());
    let mut process = supervisor
        .spawn(bash("setsid sleep 300 & echo $!", dir.path()))
        .await
        .unwrap();
    let pid = first_pid(&mut process).await;
    let job_dir = cgroup_dir(pid);
    assert!(job_dir.is_dir(), "{job_dir:?} is the job directory");

    drop(process);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !(dead(pid) && !job_dir.exists()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "pid {pid} dead: {}, {job_dir:?} exists: {}",
            dead(pid),
            job_dir.exists()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn dropping_a_group_process_kills_its_group() {
    let dir = tempfile::tempdir().unwrap();
    let supervisor = ProcessSupervisor::new(Supervision::ProcessGroup);
    let mut process = supervisor
        .spawn(bash("sleep 300 & echo $!", dir.path()))
        .await
        .unwrap();
    let pid = first_pid(&mut process).await;
    assert!(!dead(pid), "pid {pid} runs before the drop");

    drop(process);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !dead(pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "pid {pid} survived its group's kill"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// As PID 1 or a child subreaper, the host adopts a job's orphans; their zombies stay in
/// the process group until reaped, so closing must reap them rather than wait forever.
async fn closing_a_group_process_reaps_adopted_leftovers() {
    rustix::process::set_child_subreaper(Some(rustix::process::getpid()))
        .expect("become a child subreaper");
    let dir = tempfile::tempdir().unwrap();
    let supervisor = ProcessSupervisor::new(Supervision::ProcessGroup);
    let process = supervisor
        .spawn(bash("sleep 0.3 & exit 0", dir.path()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), process.close())
        .await
        .expect("close reaps the adopted leftover and returns");
}

async fn unusable_root_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker");
    let supervisor = ProcessSupervisor::new(Supervision::Cgroup {
        program: std::env::current_exe().unwrap(),
        args: vec![OsString::from(ROUTE)],
        cgroup_root: Some("/proc".into()),
    });
    let error = supervisor
        .spawn(bash("touch marker", dir.path()))
        .await
        .err()
        .expect("spawn must fail");
    assert!(
        error.starts_with("Command supervision unavailable"),
        "{error}"
    );
    assert!(!marker.exists());
}
