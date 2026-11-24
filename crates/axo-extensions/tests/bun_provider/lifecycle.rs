//! Plan verification 4 and 5: runner deaths at the two kill points, the shared group crash
//! budget, per-member load failures, the heartbeat watchdog, removal and shutdown, each on
//! its own host so one case's failures never count toward another's budget.
//!
//! Every wait is an explicit barrier (fixture control files, server counters, typed status);
//! nothing scrapes logs. Under `Supervision::Cgroup` the host sets `LAB_DETACH=1`, so the
//! fixture's child runs in its own session yet must still be in the runner's job cgroup and
//! die with it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axo::Supervision;
use axo_ai::{AssistantEvent, Credential, CredentialStore, Model, Provider};
use axo_extensions::{ExtensionEvent, ExtensionPhase, Hidden, ProviderCollision, ProviderOrigin};
use tokio_util::sync::CancellationToken;

use crate::common::{alive, model_json};
use crate::host::{Host, LAB, enabled, kill, wait_dead, wait_for};
use crate::oauth;
use crate::server::{NATIVE, assert_reply};

const INFERENCE: &str = "/inference";

pub async fn run(bun: &Path, mode: Supervision) {
    kill_points(bun, mode.clone()).await;
    group_budget(bun, mode.clone()).await;
    load_failures_and_watchdog(bun, mode.clone()).await;
    removal_and_shutdown(bun, mode).await;
}

/// Kill point 1, before `rewrite` answers: one named nonretryable failure, no HTTP, the
/// whole job dies, and the next request after `Ready` succeeds. Kill point 2, after the
/// server started SSE: the Rust-owned stream finishes with its content, no second request.
async fn kill_points(bun: &Path, mode: Supervision) {
    let cgroup = is_cgroup(&mode);
    let host = Host::new(bun, mode).await;
    oauth::login(&host).await;
    assert_reply(&host.stream(CancellationToken::new()).await);

    // Kill point 1: the runner holds inside `rewrite`.
    let ready = host.wait_phase(LAB, ExtensionPhase::Ready).await;
    let (runner, child) = (host.pid(LAB, "runner.pid"), host.pid(LAB, "child.pid"));
    let job = cgroup.then(|| same_job(runner, child));
    let sent = host.server.count(INFERENCE);
    touch(&host.data_dir(LAB).join("hold-rewrite"));
    let (events, ()) = tokio::join!(host.stream(CancellationToken::new()), async {
        host.wait_file(LAB, "rewrite-entered").await;
        kill(runner);
    });
    assert_unavailable(&events, LAB);
    reaped(&[runner, child], job.as_deref()).await;
    host.wait_ready_after(LAB, ready.generation).await;
    assert_eq!(
        host.server.count(INFERENCE),
        sent,
        "an interrupted rewrite sent HTTP or was retried"
    );
    std::fs::remove_file(host.data_dir(LAB).join("rewrite-entered")).unwrap();
    assert_reply(&host.stream(CancellationToken::new()).await);
    assert_eq!(host.server.count(INFERENCE), sent + 1);

    // Kill point 2: the server flushed the first SSE bytes and holds the rest.
    let ready = host.status(LAB);
    let (runner, child) = (host.pid(LAB, "runner.pid"), host.pid(LAB, "child.pid"));
    let job = cgroup.then(|| same_job(runner, child));
    let sent = host.server.count(INFERENCE);
    let started = host.server.sse_started();
    host.server.hold_sse();
    let (events, ()) = tokio::join!(host.stream(CancellationToken::new()), async {
        wait_for("the first SSE bytes", || {
            host.server.sse_started() > started
        })
        .await;
        kill(runner);
        // The bridge saw the death and restarted while the stream was still held.
        host.wait_ready_after(LAB, ready.generation).await;
        host.server.release_sse();
    });
    assert_reply(&events);
    assert_eq!(
        host.server.count(INFERENCE),
        sent + 1,
        "a runner death after prepare repeated the request"
    );
    reaped(&[runner, child], job.as_deref()).await;
    host.close().await;
}

/// Two grouped members share one runner and one budget: the third death in the window
/// disables both with typed events, while a separate placement and a native provider keep
/// serving.
async fn group_budget(bun: &Path, mode: Supervision) {
    const TWO: &str = "lab-two";
    const SOLO: &str = "lab-solo";
    let host = Host::new(bun, mode).await;
    host.install(TWO, LAB);
    host.install(SOLO, LAB);
    register_native(&host, "native");
    host.extensions
        .configure(&[
            enabled(LAB, Some("personal")),
            enabled(TWO, Some("personal")),
            enabled(SOLO, None),
        ])
        .await
        .unwrap();
    for provider in ["lab", TWO, SOLO] {
        grant(&host, provider).await;
        assert_reply(&stream(&host, provider).await);
    }
    assert_eq!(
        host.pid(LAB, "runner.pid"),
        host.pid(TWO, "runner.pid"),
        "group members share one runner"
    );
    let solo = (host.status(SOLO), host.pid(SOLO, "runner.pid"));
    let mut events = host.extensions.subscribe();

    for round in 1..=3 {
        let generation = host.status(LAB).generation;
        let runner = host.pid(LAB, "runner.pid");
        let children = [host.pid(LAB, "child.pid"), host.pid(TWO, "child.pid")];
        kill(runner);
        // Every ordinary descendant of the group's runner dies with it.
        reaped(&[runner, children[0], children[1]], None).await;
        if round < 3 {
            host.wait_ready_after(LAB, generation).await;
            host.wait_ready_after(TWO, generation).await;
        }
    }
    for id in [LAB, TWO] {
        let status = host.wait_phase(id, ExtensionPhase::Disabled).await;
        assert_eq!(status.group.as_deref(), Some("personal"));
        let reason = status.problem.unwrap().message;
        assert!(
            reason.contains("group personal") && reason.contains("3 runner failures"),
            "{reason}"
        );
    }
    let mut disabled = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let ExtensionEvent::StateChanged(status) = event
            && status.phase == ExtensionPhase::Disabled
        {
            assert_eq!(status.group.as_deref(), Some("personal"), "{status:?}");
            disabled.push(status.id);
        }
    }
    disabled.sort();
    disabled.dedup();
    assert_eq!(
        disabled,
        [LAB, TWO],
        "a Disabled event for each group member"
    );

    let sent = host.server.count(INFERENCE);
    assert_unavailable(&stream(&host, "lab").await, LAB);
    assert_unavailable(&stream(&host, TWO).await, TWO);
    assert_eq!(
        host.server.count(INFERENCE),
        sent,
        "a disabled member sent HTTP"
    );
    // The separate placement never restarted and still serves; so does the native.
    assert_eq!((host.status(SOLO), host.pid(SOLO, "runner.pid")), solo);
    assert_reply(&stream(&host, SOLO).await);
    let native = host.server.count(NATIVE);
    assert_reply(&stream(&host, "native").await);
    assert_eq!(host.server.count(NATIVE), native + 1);
    host.close().await;
}

/// A throwing import and a missing dependency disable only their own members (keeping the
/// hidden native provider hidden, fetching nothing); then a synchronously hung JS thread is
/// killed by the heartbeat watchdog and the group recovers without reviving them.
async fn load_failures_and_watchdog(bun: &Path, mode: Supervision) {
    const THROWING: &str = "throwing";
    const MISSING: &str = "missing-dependency";
    let host = Host::new(bun, mode).await;
    host.install(THROWING, THROWING);
    host.install(MISSING, MISSING);
    register_native(&host, THROWING);
    host.extensions
        .configure(&[
            enabled(LAB, Some("mixed")),
            enabled(THROWING, Some("mixed")),
            enabled(MISSING, Some("mixed")),
        ])
        .await
        .unwrap();
    assert!(host.extensions.collisions().contains(&ProviderCollision {
        provider: THROWING.into(),
        winner: THROWING.into(),
        hidden: Hidden::Host(ProviderOrigin::Registered),
    }));
    grant(&host, "lab").await;
    assert_reply(&host.stream(CancellationToken::new()).await);

    let lab = host.status(LAB);
    assert_eq!((lab.phase, lab.problem), (ExtensionPhase::Ready, None));
    for id in [THROWING, MISSING] {
        let status = host.status(id);
        assert_eq!(status.phase, ExtensionPhase::Disabled, "{status:?}");
        let reason = status.problem.unwrap().message;
        assert!(
            reason.contains(id) && reason.contains("failed to load"),
            "{reason}"
        );
        assert!(
            !reason.contains(LAB),
            "{id}'s failure blamed its sibling: {reason}"
        );
    }
    let declared = host.models.find("throwing/m").unwrap();
    assert_ne!(
        declared.base_url,
        native_url(&host),
        "the hidden native was exposed"
    );
    let native = host.server.count(NATIVE);
    assert_unavailable(&stream(&host, THROWING).await, THROWING);
    assert_unavailable(&stream(&host, MISSING).await, MISSING);
    assert_eq!(
        host.server.count(NATIVE),
        native,
        "the hidden native was called"
    );
    let fetched: Vec<String> = host
        .server
        .paths()
        .into_iter()
        .filter(|path| path.starts_with("/registry"))
        .collect();
    assert!(fetched.is_empty(), "Bun tried to install: {fetched:?}");

    // Watchdog: the next rewrite starves the JS event loop.
    let ready = host.status(LAB);
    let (runner, child) = (host.pid(LAB, "runner.pid"), host.pid(LAB, "child.pid"));
    let sent = host.server.count(INFERENCE);
    touch(&host.data_dir(LAB).join("hang"));
    let events = host.stream(CancellationToken::new()).await;
    assert!(host.data_dir(LAB).join("hang-entered").exists());
    assert_unavailable(&events, LAB);
    reaped(&[runner, child], None).await;
    host.wait_ready_after(LAB, ready.generation).await;
    assert_eq!(host.server.count(INFERENCE), sent);
    for id in [THROWING, MISSING] {
        assert_eq!(host.status(id).phase, ExtensionPhase::Disabled);
    }
    assert_reply(&host.stream(CancellationToken::new()).await);
    host.close().await;
}

/// Deliberate removal reveals the exact native registration `lab-provider` hid and reaps
/// its runner; explicit shutdown reaps every remaining job before returning.
async fn removal_and_shutdown(bun: &Path, mode: Supervision) {
    const SOLO: &str = "lab-solo";
    let cgroup = is_cgroup(&mode);
    let host = Host::new(bun, mode).await;
    let native = register_native(&host, "lab");
    host.install(SOLO, LAB);
    let mut events = host.extensions.subscribe();
    host.extensions
        .configure(&[enabled(LAB, None), enabled(SOLO, None)])
        .await
        .unwrap();
    let shadowed = ProviderCollision {
        provider: "lab".into(),
        winner: LAB.into(),
        hidden: Hidden::Host(ProviderOrigin::Registered),
    };
    assert!(host.extensions.collisions().contains(&shadowed));
    let mut announced = false;
    while let Ok(event) = events.try_recv() {
        announced |= event == ExtensionEvent::Shadowed(shadowed.clone());
    }
    assert!(announced, "no Shadowed event for lab");
    assert_ne!(host.models.find("lab/m").unwrap(), native);

    for provider in ["lab", SOLO] {
        grant(&host, provider).await;
        assert_reply(&stream(&host, provider).await);
    }
    let pids = |id: &str| [host.pid(id, "runner.pid"), host.pid(id, "child.pid")];
    let (lab, solo) = (pids(LAB), pids(SOLO));
    let jobs = |pids: [i32; 2]| cgroup.then(|| same_job(pids[0], pids[1]));
    let (lab_job, solo_job) = (jobs(lab), jobs(solo));

    host.extensions
        .configure(&[enabled(SOLO, None)])
        .await
        .unwrap();
    assert_eq!(
        host.models.find("lab/m").unwrap(),
        native,
        "removal revealed something else"
    );
    assert!(!host.extensions.collisions().contains(&shadowed));
    reaped(&lab, lab_job.as_deref()).await;
    let sent = host.server.count(NATIVE);
    assert_reply(&stream(&host, "lab").await);
    assert_eq!(
        host.server.count(NATIVE),
        sent + 1,
        "lab/m did not reach the native"
    );

    host.extensions.shutdown().await;
    assert!(
        !alive(solo[0]),
        "shutdown returned before reaping {SOLO}'s runner"
    );
    reaped(&solo, solo_job.as_deref()).await;
    assert!(host.models.find("lab-solo/m").is_none());
    assert_eq!(host.models.find("lab/m").unwrap(), native);
    host.close().await;
}

fn is_cgroup(mode: &Supervision) -> bool {
    matches!(mode, Supervision::Cgroup { .. })
}

/// Streams `<provider>/m`.
async fn stream(host: &Host, provider: &str) -> Vec<AssistantEvent> {
    host.stream_model(&format!("{provider}/m"), CancellationToken::new())
        .await
}

/// Stores a valid, far-from-expiry credential the lab server accepts, so `provider` needs
/// no refresh.
async fn grant(host: &Host, provider: &str) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let credential = Credential::Oauth {
        access: "access-2".into(),
        refresh: "refresh-2".into(),
        expires: i64::try_from(now.as_millis()).unwrap() + 3_600_000,
        headers: BTreeMap::new(),
        base_url: None,
        extra: serde_json::Map::new(),
    };
    let slot = host.models.credential_slot(provider).unwrap();
    host.store.set(&slot, credential).await.unwrap();
}

fn native_url(host: &Host) -> String {
    format!("{}/native", host.server.url)
}

/// Registers a keyless native runtime provider `<provider>/m` on the server's native path
/// and returns its model.
fn register_native(host: &Host, provider: &str) -> Model {
    let model: Model = serde_json::from_value(model_json(provider, &native_url(host))).unwrap();
    host.models
        .register(
            "host",
            Provider {
                id: provider.into(),
                models: vec![model.clone()],
                env: Vec::new(),
                requires_key: false,
                oauth_only: false,
                login: None,
                rewrite: None,
            },
        )
        .unwrap();
    model
}

fn touch(path: &Path) {
    std::fs::write(path, "").unwrap();
}

/// Exactly one terminal, nonretryable `provider_unavailable` failure naming `needle`.
fn assert_unavailable(events: &[AssistantEvent], needle: &str) {
    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AssistantEvent::Error { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "{events:?}");
    assert!(
        matches!(events.last(), Some(AssistantEvent::Error { .. })),
        "{events:?}"
    );
    let message = errors[0];
    let text = message.error_message.as_deref().unwrap_or_default();
    assert!(text.contains(needle), "{needle:?} not in {text:?}");
    let failure = message.failure.as_ref().expect("no provider failure");
    assert_eq!(
        failure.code.as_deref(),
        Some("provider_unavailable"),
        "{text}"
    );
    assert_eq!(failure.should_retry, Some(false), "{text}");
    assert!(!axo_ai::is_retryable_error(message), "{text}");
}

/// Waits until every PID is gone and, under cgroups, the job directory too.
async fn reaped(pids: &[i32], job: Option<&Path>) {
    for &pid in pids {
        wait_dead(pid).await;
    }
    if let Some(job) = job {
        wait_for(&format!("{} to go away", job.display()), || !job.exists()).await;
    }
}

/// Asserts the detached child left the runner's session yet shares its job cgroup, and
/// returns that job directory.
fn same_job(runner: i32, child: i32) -> PathBuf {
    assert_ne!(
        session(child),
        session(runner),
        "LAB_DETACH child {child} did not setsid"
    );
    let job = cgroup_dir(runner);
    assert_eq!(
        cgroup_dir(child),
        job,
        "child {child} escaped the job cgroup"
    );
    assert!(
        job.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("axo-job-")),
        "{} is not an axo job cgroup",
        job.display()
    );
    job
}

/// The session ID: field 6 of `/proc/<pid>/stat`, the 4th after the `(comm)` field.
fn session(pid: i32) -> i32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let (_, rest) = stat.rsplit_once(')').unwrap();
    rest.split_whitespace().nth(3).unwrap().parse().unwrap()
}

/// The cgroup v2 directory `pid` lives in.
fn cgroup_dir(pid: i32) -> PathBuf {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .expect("cgroup v2 entry");
    Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'))
}
