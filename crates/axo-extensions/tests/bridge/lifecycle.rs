//! Runner lifecycle through the public API, with this test binary as a scripted runner
//! under the real process supervisor (see `fake` in `bridge.rs`).

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use axo_ai::{
    AssistantEvent, Credential, CredentialStore, DeviceCode, Error, LoginInteraction, LoginPrompt,
    LoginSelect, now_ms,
};
use axo_extensions::{BridgeError, ExtensionEvent, ExtensionPhase};

use crate::common::{alive, phase};

use super::{
    Case, Host, Http, assert_unavailable, base_url, enabled, find, header, kill, register_native,
    stream, wait_for, wait_phase, wait_ready_after,
};

pub const CASES: &[Case] = &[
    case!(cold_until_a_callback_is_needed_then_rewrites_reach_http),
    case!(logins_and_refresh_run_through_the_runner),
    case!(crash_counts_once_and_the_third_disables_the_group_until_retry),
    case!(load_rejection_spares_its_sibling_and_retry_reloads_it),
    case!(startup_death_fails_the_waiter_then_restarts),
    case!(connected_runner_without_hello_times_out_and_is_reaped),
    case!(configure_and_shutdown_during_startup_fail_the_waiter),
    case!(removal_and_shutdown_end_a_waiting_login),
    case!(displaced_call_is_canceled_and_its_stale_reply_dropped),
    case!(interrupted_callbacks_send_no_http_and_store_nothing),
    case!(setup_failure_recovers_on_the_next_configure),
    case!(reload_replaces_a_warm_runner),
];

/// A login UI answering prompts with `answer`, or never when `answer` is `None`.
struct Ui {
    answer: Option<&'static str>,
    events: Mutex<Vec<String>>,
}

impl Ui {
    fn new(answer: Option<&'static str>) -> Self {
        Ui {
            answer,
            events: Mutex::default(),
        }
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn push(&self, event: String) {
        self.events.lock().unwrap().push(event);
    }
}

#[async_trait]
impl LoginInteraction for Ui {
    async fn open_url(&self, url: &str, _: Option<&str>) {
        self.push(format!("url {url}"));
    }

    async fn device_code(&self, code: &DeviceCode) {
        self.push(format!("code {}", code.user_code));
    }

    async fn prompt(&self, prompt: &LoginPrompt) -> Result<String, Error> {
        self.push(format!("prompt {}", prompt.message));
        match self.answer {
            Some(answer) => Ok(answer.into()),
            None => std::future::pending().await,
        }
    }

    async fn select(&self, select: &LoginSelect) -> Result<String, Error> {
        Ok(select.options[0].id.clone())
    }

    async fn progress(&self, message: &str) {
        self.push(format!("progress {message}"));
    }
}

async fn request(host: &Host, provider: &str) -> AssistantEvent {
    let model = find(&host.models, provider).unwrap_or_else(|| panic!("{provider} not listed"));
    stream(&host.models, &model).await
}

fn access(credential: Option<Credential>) -> Option<String> {
    match credential? {
        Credential::Oauth { access, .. } => Some(access),
        Credential::ApiKey { .. } => None,
    }
}

fn expiring() -> Credential {
    Credential::Oauth {
        access: "stale".into(),
        refresh: "refresh-0".into(),
        expires: now_ms() + 60_000,
        headers: BTreeMap::new(),
        base_url: None,
        extra: Default::default(),
    }
}

async fn cold_until_a_callback_is_needed_then_rewrites_reach_http() {
    let host = Host::new().await;
    let http = Http::start();
    host.package("root", "lab-provider", &[("lab", &http.url, false, true)]);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[("lab-provider", None)]))
        .await
        .unwrap();
    // Listing never starts the runner.
    assert_eq!(
        phase(&extensions, "lab-provider").phase,
        ExtensionPhase::Cold
    );
    assert!(find(&host.models, "lab").is_some());
    assert!(host.models.all().iter().any(|m| m.provider == "lab"));
    assert!(host.pids().is_empty());
    assert!(!host.path("data").join("lab-provider").exists());

    // Two requests on the cold placement: the second waits for the first start, and both
    // reach HTTP through the one runner.
    tokio::join!(request(&host, "lab"), request(&host, "lab"));
    let requests = http.requests();
    assert_eq!(requests.len(), 2);
    let (head, body) = &requests[0];
    assert!(head.starts_with("POST /rewritten "), "{head}");
    assert_eq!(header(head, "x-rewritten"), Some("yes"));
    assert_eq!(body["rewritten"], true);
    assert_eq!(body["model"], "m");
    assert_eq!(
        phase(&extensions, "lab-provider").phase,
        ExtensionPhase::Ready
    );
    assert!(host.path("data").join("lab-provider").is_dir());

    // The live runner serves later requests.
    request(&host, "lab").await;
    assert_eq!(http.requests().len(), 3);
    let pids = host.pids();
    assert_eq!(pids.len(), 1);
    assert_eq!(
        host.calls("lab-provider"),
        ["activate", "rewrite", "rewrite", "rewrite"]
    );

    extensions.shutdown().await;
    assert!(!alive(pids[0]), "shutdown did not reap the runner");
}

async fn logins_and_refresh_run_through_the_runner() {
    let host = Host::new().await;
    let http = Http::start();
    host.package("root", "alpha", &[("pa", &http.url, true, false)]);
    let beta_http = Http::start();
    host.package(
        "root",
        "beta",
        &[
            ("pa", &beta_http.url, true, false),
            ("pb", &http.url, true, false),
        ],
    );
    let extensions = host.manager();
    let alpha_wins = enabled(&[("beta", Some("g")), ("alpha", Some("g"))]);
    let beta_wins = enabled(&[("alpha", Some("g")), ("beta", Some("g"))]);
    extensions.configure(&alpha_wins).await.unwrap();
    let pa = host.models.credential_slot("pa").unwrap();
    let pb = host.models.credential_slot("pb").unwrap();

    // The first login starts the shared runner and waits for activation.
    let first = Ui::new(Some("first"));
    host.models.login("pa", &first).await.unwrap();
    assert_eq!(
        first.events(),
        [
            "url https://login.test/authorize",
            "progress waiting",
            "prompt code"
        ]
    );
    assert_eq!(
        access(host.store.get(&pa).await.unwrap()).as_deref(),
        Some("access-first")
    );

    // Two concurrent logins on one runner each get their own interactions and answers.
    let (one, two) = (Ui::new(Some("one")), Ui::new(Some("two")));
    let (a, b) = tokio::join!(host.models.login("pa", &one), host.models.login("pb", &two));
    a.unwrap();
    b.unwrap();
    assert_eq!(one.events().len(), 3);
    assert_eq!(two.events().len(), 3);
    assert_eq!(
        access(host.store.get(&pa).await.unwrap()).as_deref(),
        Some("access-one")
    );
    assert_eq!(
        access(host.store.get(&pb).await.unwrap()).as_deref(),
        Some("access-two")
    );
    assert_eq!(host.pids().len(), 1);

    // An expiring credential is refreshed through the runner and stored before the request.
    host.store.set(&pa, expiring()).await.unwrap();
    request(&host, "pa").await;
    assert_eq!(
        access(host.store.get(&pa).await.unwrap()).as_deref(),
        Some("refreshed")
    );
    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        header(&requests[0].0, "authorization"),
        Some("Bearer refreshed")
    );
    assert_eq!(
        host.calls("alpha")
            .iter()
            .filter(|m| *m == "refresh")
            .count(),
        1
    );

    // Reordering claimants in the same layer must not lend alpha's token to beta.
    extensions.configure(&beta_wins).await.unwrap();
    let beta_pa = host.models.credential_slot("pa").unwrap();
    assert_ne!(pa, beta_pa);
    assert_eq!(host.store.get(&beta_pa).await.unwrap(), None);
    request(&host, "pa").await;
    assert!(beta_http.requests().is_empty());
    host.models
        .login("pa", &Ui::new(Some("beta")))
        .await
        .unwrap();
    request(&host, "pa").await;
    let requests = beta_http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        header(&requests[0].0, "authorization"),
        Some("Bearer access-beta")
    );

    extensions.configure(&alpha_wins).await.unwrap();
    assert_eq!(host.models.credential_slot("pa").unwrap(), pa);
    request(&host, "pa").await;
    let requests = http.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        header(&requests[1].0, "authorization"),
        Some("Bearer refreshed")
    );
    extensions.shutdown().await;

    // Even the same extension ID in a different package root is a new principal.
    host.package("root2", "alpha", &[("pa", &http.url, true, false)]);
    let mut options = host.options();
    options.roots.reverse();
    let other = host.manager_with(options);
    other.configure(&alpha_wins).await.unwrap();
    let other_pa = host.models.credential_slot("pa").unwrap();
    assert_ne!(other_pa, pa);
    assert_eq!(host.store.get(&other_pa).await.unwrap(), None);
    other.shutdown().await;
}

async fn crash_counts_once_and_the_third_disables_the_group_until_retry() {
    let host = Host::new().await;
    let http = Http::start();
    let native = Http::start();
    register_native(&host.models, "native", &native.url);
    host.package("root", "alpha", &[("pa", &http.url, false, true)]);
    host.package("root", "beta", &[("pb", &http.url, false, true)]);
    host.package("root", "solo", &[("ps", &http.url, false, true)]);
    let extensions = host.manager();
    let mut events = extensions.subscribe();
    let list = enabled(&[("alpha", Some("g")), ("beta", Some("g")), ("solo", None)]);
    extensions.configure(&list).await.unwrap();

    request(&host, "pa").await;
    assert_eq!(http.requests().len(), 1);
    // SIGKILL ends the process and its connection together: one failure, not two.
    for crash in 1..=2 {
        let died = phase(&extensions, "alpha").generation;
        kill(*host.pids().last().unwrap());
        wait_for("restart", || host.pids().len() == crash + 1).await;
        for id in ["alpha", "beta"] {
            let status = wait_ready_after(&extensions, id, died).await;
            assert_eq!(status.generation, crash as u64 + 1, "{status:?}");
        }
    }
    // The configure in between keeps the placement and its budget.
    extensions.configure(&list).await.unwrap();
    assert_eq!(phase(&extensions, "alpha").phase, ExtensionPhase::Ready);

    kill(*host.pids().last().unwrap());
    for id in ["alpha", "beta"] {
        let status = wait_phase(&extensions, id, ExtensionPhase::Disabled).await;
        let problem = status.problem.unwrap();
        assert_eq!(problem.kind, axo_extensions::ProblemKind::Crash);
        let reason = problem.message;
        assert!(reason.contains("group g"), "{reason}");
    }
    // No third restart.
    assert_eq!(host.pids().len(), 3);
    let mut restarts = 0;
    let mut disabled = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let ExtensionEvent::StateChanged(status) = event {
            match status.phase {
                ExtensionPhase::Restarting if status.id == "alpha" => restarts += 1,
                ExtensionPhase::Disabled => disabled.push(status.id),
                _ => {}
            }
        }
    }
    assert_eq!(restarts, 2);
    assert_eq!(disabled, ["alpha", "beta"]);

    let sent = http.requests().len();
    assert_unavailable(&request(&host, "pa").await, "alpha");
    assert_unavailable(&request(&host, "pb").await, "beta");
    assert_eq!(http.requests().len(), sent);
    // A separate placement and a native provider keep working.
    request(&host, "ps").await;
    assert_eq!(http.requests().len(), sent + 1);
    request(&host, "native").await;
    assert_eq!(native.requests().len(), 1);

    // An unchanged configure does not re-arm it; an explicit retry re-arms the group.
    extensions.configure(&list).await.unwrap();
    assert_eq!(phase(&extensions, "beta").phase, ExtensionPhase::Disabled);
    extensions.retry("alpha").await.unwrap();
    assert_eq!(phase(&extensions, "alpha").phase, ExtensionPhase::Cold);
    assert_eq!(phase(&extensions, "beta").phase, ExtensionPhase::Cold);
    request(&host, "pb").await;
    request(&host, "pa").await;
    assert_eq!(http.requests().len(), sent + 3);
    extensions.shutdown().await;
}

async fn load_rejection_spares_its_sibling_and_retry_reloads_it() {
    let host = Host::new().await;
    let http = Http::start();
    let native = Http::start();
    // The rejected member's provider hides a native one, which must stay hidden.
    register_native(&host.models, "pa", &native.url);
    host.package("root", "alpha", &[("pa", &http.url, false, true)]);
    host.package("root", "beta", &[("pb", &http.url, false, true)]);
    host.script("alpha", "activate", "reject");
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[("alpha", Some("g")), ("beta", Some("g"))]))
        .await
        .unwrap();

    request(&host, "pb").await;
    assert_eq!(http.requests().len(), 1);
    let alpha = phase(&extensions, "alpha");
    assert_eq!(alpha.phase, ExtensionPhase::Disabled, "{alpha:?}");
    let problem = alpha.problem.unwrap();
    assert_eq!(problem.kind, axo_extensions::ProblemKind::Load);
    assert!(problem.message.contains("alpha"));
    let beta = phase(&extensions, "beta");
    assert_eq!(beta.phase, ExtensionPhase::Ready, "{beta:?}");
    assert!(beta.problem.is_none());

    assert_eq!(base_url(&host.models, "pa").unwrap(), http.url);
    assert_unavailable(&request(&host, "pa").await, "alpha");
    assert!(native.requests().is_empty());
    assert_eq!(http.requests().len(), 1);
    // A member rejection is not a crash: the runner is unchanged.
    assert_eq!(host.pids().len(), 1);

    host.script("alpha", "activate", "ok");
    extensions.retry("alpha").await.unwrap();
    request(&host, "pa").await;
    request(&host, "pb").await;
    assert_eq!(http.requests().len(), 3);
    assert_eq!(phase(&extensions, "alpha").phase, ExtensionPhase::Ready);
    assert_eq!(phase(&extensions, "beta").phase, ExtensionPhase::Ready);
    assert!(native.requests().is_empty());
    extensions.shutdown().await;
}

async fn startup_death_fails_the_waiter_then_restarts() {
    let host = Host::new().await;
    let http = Http::start();
    host.package("root", "lab-provider", &[("lab", &http.url, false, true)]);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[("lab-provider", None)]))
        .await
        .unwrap();
    host.script_startup("exit");

    assert_unavailable(&request(&host, "lab").await, "lab-provider");
    assert!(http.requests().is_empty());
    // Generation 1 died in startup; its replacement must come up on its own.
    wait_ready_after(&extensions, "lab-provider", 1).await;
    assert_eq!(host.pids().len(), 2);
    request(&host, "lab").await;
    assert_eq!(http.requests().len(), 1);
    extensions.shutdown().await;
}

async fn connected_runner_without_hello_times_out_and_is_reaped() {
    let host = Host::new().await;
    let http = Http::start();
    host.package("root", "lab-provider", &[("lab", &http.url, false, true)]);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[("lab-provider", None)]))
        .await
        .unwrap();
    host.script_startup("no-hello");

    let first = tokio::time::timeout(std::time::Duration::from_secs(25), request(&host, "lab"))
        .await
        .expect("the runner handshake had no deadline");
    assert_unavailable(&first, "runner did not complete its handshake within 15 s");
    assert!(http.requests().is_empty());
    let pid = std::fs::read_to_string(host.path("ctl/connected-no-hello"))
        .expect("the runner never connected")
        .trim()
        .parse()
        .unwrap();
    assert!(!alive(pid), "the silent runner was not reaped");

    wait_ready_after(&extensions, "lab-provider", 1).await;
    request(&host, "lab").await;
    assert_eq!(http.requests().len(), 1);
    extensions.shutdown().await;
}

async fn configure_and_shutdown_during_startup_fail_the_waiter() {
    let host = Host::new().await;
    let http = Http::start();
    let native = Http::start();
    register_native(&host.models, "pa", &native.url);
    host.package("root", "alpha", &[("pa", &http.url, false, true)]);
    host.script("alpha", "activate", "hang");
    let list = enabled(&[("alpha", None)]);

    for shutdown in [false, true] {
        let extensions = host.manager();
        extensions.configure(&list).await.unwrap();
        let calls = host.calls("alpha").len();
        let pending = request(&host, "pa");
        tokio::pin!(pending);
        tokio::select! {
            _ = &mut pending => panic!("finished while activation hangs"),
            () = host.wait_calls("alpha", calls + 1) => {}
        }
        assert_eq!(phase(&extensions, "alpha").phase, ExtensionPhase::Starting);
        let pid = *host.pids().last().unwrap();
        let (event, ()) = tokio::join!(pending, async {
            if shutdown {
                extensions.shutdown().await;
            } else {
                extensions.configure(&[]).await.unwrap();
            }
        });
        assert_unavailable(&event, "alpha");
        assert!(!alive(pid), "the starting runner was not reaped");
        assert!(http.requests().is_empty());
        assert!(native.requests().is_empty());
        assert_eq!(base_url(&host.models, "pa").unwrap(), native.url);
        extensions.shutdown().await;
    }
}

async fn removal_and_shutdown_end_a_waiting_login() {
    let host = Host::new().await;
    let http = Http::start();
    host.package("root", "alpha", &[("pa", &http.url, true, false)]);
    let list = enabled(&[("alpha", None)]);

    for shutdown in [false, true] {
        let extensions = host.manager();
        extensions.configure(&list).await.unwrap();
        let pa = host.models.credential_slot("pa").unwrap();
        let ui = Ui::new(None);
        let login = host.models.login("pa", &ui);
        tokio::pin!(login);
        tokio::select! {
            _ = &mut login => panic!("login finished without an answer"),
            () = wait_for("prompt", || ui.events().len() == 3) => {}
        }
        let pid = *host.pids().last().unwrap();
        let (result, ()) = tokio::join!(login, async {
            if shutdown {
                extensions.shutdown().await;
            } else {
                extensions.configure(&[]).await.unwrap();
            }
        });
        match result {
            Err(Error::ProviderUnavailable { provider, reason }) => {
                assert_eq!(provider, "pa");
                assert!(reason.contains("alpha"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(host.store.get(&pa).await.unwrap(), None);
        if shutdown {
            assert!(!alive(pid));
        }
        extensions.shutdown().await;
        assert!(!alive(pid));
    }
}

async fn displaced_call_is_canceled_and_its_stale_reply_dropped() {
    let host = Host::new().await;
    let (a, b) = (Http::start(), Http::start());
    host.package(
        "root",
        "alpha",
        &[("shared", &a.url, false, true), ("ka", &a.url, false, true)],
    );
    host.package(
        "root",
        "beta",
        &[("shared", &b.url, false, true), ("kb", &b.url, false, true)],
    );
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[("alpha", Some("g")), ("beta", Some("g"))]))
        .await
        .unwrap();
    request(&host, "kb").await;
    assert_eq!(b.requests().len(), 1);
    let pid = host.pids()[0];

    // beta's rewrite for `shared` is in flight when a reorder hands `shared` to alpha.
    host.script("beta", "rewrite", "late");
    let pending = request(&host, "shared");
    tokio::pin!(pending);
    tokio::select! {
        _ = &mut pending => panic!("finished while the rewrite is held"),
        () = host.wait_calls("beta", 3) => {}
    }
    let (event, ()) = tokio::join!(pending, async {
        extensions
            .configure(&enabled(&[("beta", Some("g")), ("alpha", Some("g"))]))
            .await
            .unwrap();
    });
    assert_unavailable(&event, "beta");
    wait_for("cancel frame", || host.canceled("beta") == ["rewrite"]).await;
    // The runner answers anyway; the stale reply satisfies nothing.
    std::fs::write(host.path("ctl").join("beta").join("release"), "").unwrap();
    host.script("beta", "rewrite", "ok");

    // Reordering a group kept its healthy runner, which still serves beta's other provider.
    assert!(alive(pid));
    assert_eq!(host.pids(), [pid]);
    assert_eq!(phase(&extensions, "beta").phase, ExtensionPhase::Ready);
    request(&host, "kb").await;
    assert_eq!(b.requests().len(), 2, "the displaced request reached HTTP");
    request(&host, "shared").await;
    assert_eq!(a.requests().len(), 1);
    assert_eq!(b.requests().len(), 2);
    assert_eq!(host.pids(), [pid]);
    extensions.shutdown().await;
}

async fn interrupted_callbacks_send_no_http_and_store_nothing() {
    let host = Host::new().await;
    let http = Http::start();
    host.package("root", "lab-provider", &[("lab", &http.url, true, true)]);
    let extensions = host.manager();
    extensions
        .configure(&enabled(&[("lab-provider", None)]))
        .await
        .unwrap();
    let before = expiring();
    let slot = host.models.credential_slot("lab").unwrap();
    host.store.set(&slot, before.clone()).await.unwrap();

    // Runner death during refresh: nothing stored, nothing sent, one named failure.
    host.script("lab-provider", "refresh", "exit");
    assert_unavailable(&request(&host, "lab").await, "lab-provider");
    assert_eq!(host.store.get(&slot).await.unwrap(), Some(before));
    assert!(http.requests().is_empty());
    // Generation 1 died mid-refresh; wait for its replacement.
    wait_ready_after(&extensions, "lab-provider", 1).await;

    // An author's rewrite failure fails that request only; the runner stays.
    host.script("lab-provider", "refresh", "ok");
    host.script("lab-provider", "rewrite", "fail");
    let pids = host.pids();
    let serving = phase(&extensions, "lab-provider").generation;
    let text = super::error_text(&request(&host, "lab").await);
    assert!(
        text.contains("lab-provider") && text.contains("rewrite"),
        "{text}"
    );
    assert_eq!(
        access(host.store.get(&slot).await.unwrap()).as_deref(),
        Some("refreshed")
    );
    assert!(http.requests().is_empty());
    assert_eq!(host.pids(), pids);
    let status = phase(&extensions, "lab-provider");
    assert_eq!(status.phase, ExtensionPhase::Ready);
    assert_eq!(
        status.generation, serving,
        "an author error restarted the runner"
    );

    // Runner death during rewrite: no HTTP and no automatic retry.
    host.script("lab-provider", "rewrite", "exit");
    let died = phase(&extensions, "lab-provider").generation;
    let rewrites = host
        .calls("lab-provider")
        .iter()
        .filter(|m| *m == "rewrite")
        .count();
    assert_unavailable(&request(&host, "lab").await, "lab-provider");
    assert!(http.requests().is_empty());
    assert_eq!(
        host.calls("lab-provider")
            .iter()
            .filter(|m| *m == "rewrite")
            .count(),
        rewrites + 1
    );
    wait_ready_after(&extensions, "lab-provider", died).await;

    // The next independent request after Ready succeeds.
    host.script("lab-provider", "rewrite", "ok");
    request(&host, "lab").await;
    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        header(&requests[0].0, "authorization"),
        Some("Bearer refreshed")
    );
    extensions.shutdown().await;
}

async fn setup_failure_recovers_on_the_next_configure() {
    let host = Host::new().await;
    let http = Http::start();
    host.package("root", "lab-provider", &[("lab", &http.url, false, true)]);
    let extensions = host.manager();
    let list = enabled(&[("lab-provider", None)]);
    extensions.configure(&list).await.unwrap();

    // A runtime_dir that is a file fails setup, not the runner.
    std::fs::remove_dir(host.path("run")).unwrap();
    std::fs::write(host.path("run"), "").unwrap();
    let event = request(&host, "lab").await;
    assert_unavailable(&event, "lab-provider");
    wait_phase(&extensions, "lab-provider", ExtensionPhase::Unavailable).await;

    // Once the host fixes its inputs, an unchanged configure rebuilds the placement.
    std::fs::remove_file(host.path("run")).unwrap();
    std::fs::create_dir(host.path("run")).unwrap();
    extensions.configure(&list).await.unwrap();
    assert_eq!(
        phase(&extensions, "lab-provider").phase,
        ExtensionPhase::Cold
    );
    request(&host, "lab").await;
    assert!(!http.requests().is_empty());
    assert_eq!(
        phase(&extensions, "lab-provider").phase,
        ExtensionPhase::Ready
    );
    extensions.shutdown().await;
}

async fn reload_replaces_a_warm_runner() {
    let host = Host::new().await;
    let http = Http::start();
    host.package("root", "lab-provider", &[("lab", &http.url, false, true)]);
    let extensions = host.manager();
    let list = enabled(&[("lab-provider", None)]);
    extensions.configure(&list).await.unwrap();
    request(&host, "lab").await;
    assert_eq!(
        phase(&extensions, "lab-provider").phase,
        ExtensionPhase::Ready
    );
    assert_eq!(host.pids().len(), 1);

    // An unchanged configure keeps the warm runner.
    extensions.configure(&list).await.unwrap();
    assert_eq!(host.pids().len(), 1);
    assert_eq!(
        phase(&extensions, "lab-provider").phase,
        ExtensionPhase::Ready
    );

    // `reload` replaces it with a cold placement and reaps the old runner.
    extensions.reload("lab-provider").await.unwrap();
    assert_eq!(
        phase(&extensions, "lab-provider").phase,
        ExtensionPhase::Cold
    );
    let first = host.pids()[0];
    wait_for("the reloaded runner to be reaped", || !alive(first)).await;
    request(&host, "lab").await;
    assert_eq!(host.pids().len(), 2);

    assert!(matches!(
        extensions.reload("nope").await,
        Err(BridgeError::UnknownExtension(_))
    ));
    extensions.shutdown().await;
}
