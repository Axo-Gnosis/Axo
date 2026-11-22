//! TS-owned OAuth through the public Rust host: cold listing and sessions never start Bun;
//! a typed device-code login stores a short-lived credential; three concurrent requests
//! share one locked refresh (one of them canceled while it waits), and the two survivors are
//! rewritten by TS and parsed by Rust.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use axo::Supervision;
use axo_ai::{
    AssistantBlock, AssistantEvent, Credential, CredentialStore, DeviceCode, Error,
    LoginInteraction, LoginPrompt, LoginSelect, StopReason, now_ms,
};
use axo_extensions::ExtensionPhase;
use tokio_util::sync::CancellationToken;

use crate::host::{Host, LAB, wait_for};
use crate::server::{TEXT_HEAD, TEXT_TAIL, TOOL};

/// axo refreshes OAuth credentials expiring within this margin.
const REFRESH_MARGIN_MS: i64 = 5 * 60_000;

/// What the TS login asked of the host UI, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asked {
    OpenUrl(String),
    DeviceCode(DeviceCode),
    Prompt(LoginPrompt),
    Select(LoginSelect),
    Progress(String),
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Asked>>);

impl Recorder {
    fn push(&self, asked: Asked) {
        self.0.lock().unwrap().push(asked);
    }
}

#[async_trait]
impl LoginInteraction for Recorder {
    async fn open_url(&self, url: &str, _instructions: Option<&str>) {
        self.push(Asked::OpenUrl(url.into()));
    }

    async fn device_code(&self, code: &DeviceCode) {
        self.push(Asked::DeviceCode(code.clone()));
    }

    async fn prompt(&self, prompt: &LoginPrompt) -> Result<String, Error> {
        self.push(Asked::Prompt(prompt.clone()));
        Ok("approved".into())
    }

    async fn select(&self, select: &LoginSelect) -> Result<String, Error> {
        self.push(Asked::Select(select.clone()));
        Err(Error::Auth("the lab login has nothing to select".into()))
    }

    async fn progress(&self, message: &str) {
        self.push(Asked::Progress(message.into()));
    }
}

pub async fn run(bun: &Path, mode: Supervision) {
    let host = Host::new(bun, mode).await;
    host.assert_cold_sessions().await;
    // Everything this host owns lives in its temp directory: no runner generation yet.
    assert_eq!(
        std::fs::read_dir(host.path("runtime")).unwrap().count(),
        0,
        "a cold host materialized a runner"
    );

    login(&host).await;
    // The login started the runner, below the host's own runtime and data directories.
    assert_eq!(host.status(LAB).phase, ExtensionPhase::Ready);
    assert_ne!(
        std::fs::read_dir(host.path("runtime")).unwrap().count(),
        0,
        "the runner generation lives under the host runtime_dir"
    );
    assert!(host.data_dir(LAB).join("runner.pid").exists());

    shared_refresh(&host).await;
    host.close().await;
}

/// Logs `lab` in through `Models::login`, asserting the typed interactions TS drove and the
/// short-lived credential it shaped (`access-1`, expiring within the refresh margin, so the
/// next request refreshes).
pub async fn login(host: &Host) {
    let (devices, tokens) = (host.server.count("/device"), host.server.count("/token"));
    let ui = Recorder::default();
    let before = now_ms();
    host.models.login("lab", &ui).await.unwrap();
    let after = now_ms();

    let asked = ui.0.into_inner().unwrap();
    assert_eq!(
        asked,
        [
            Asked::DeviceCode(DeviceCode {
                user_code: "LAB-CODE".into(),
                verification_url: format!("{}/verify", host.server.url),
                expires_in_seconds: None,
            }),
            Asked::Prompt(LoginPrompt {
                message: "Enter the lab approval".into(),
                placeholder: None,
                secret: false,
            }),
            Asked::Progress("Exchanging lab approval".into()),
        ]
    );
    assert_eq!(host.server.count("/device"), devices + 1);
    assert_eq!(host.server.count("/token"), tokens + 1);
    assert_eq!(host.server.rejected(), Vec::<String>::new());

    match host.store.get("lab").await.unwrap() {
        Some(Credential::Oauth {
            access,
            refresh,
            expires,
            ..
        }) => {
            assert_eq!(
                (access.as_str(), refresh.as_str()),
                ("access-1", "refresh-1")
            );
            assert!(
                (before + 60_000..=after + 60_000).contains(&expires),
                "expires {expires} not ~now+60s"
            );
            assert!(expires < after + REFRESH_MARGIN_MS);
        }
        other => panic!("login stored {other:?}"),
    }
}

/// Three concurrent requests over the short-lived credential: the store's lock admits one
/// refresh, held at the server; a waiter canceled meanwhile ends `Aborted` without
/// disturbing it; on release the other two use the rotated token.
async fn shared_refresh(host: &Host) {
    let server = &host.server;
    assert_eq!(server.count("/refresh"), 0);
    server.hold_refresh();
    let canceled = CancellationToken::new();
    let canceled_done = AtomicBool::new(false);
    let (first, second, waiter, ()) = tokio::join!(
        host.stream(CancellationToken::new()),
        host.stream(CancellationToken::new()),
        async {
            let events = host.stream(canceled.clone()).await;
            canceled_done.store(true, Ordering::SeqCst);
            events
        },
        async {
            wait_for("the shared refresh to reach the server", || {
                server.count("/refresh") == 1
            })
            .await;
            canceled.cancel();
            wait_for("the canceled waiter to end", || {
                canceled_done.load(Ordering::SeqCst)
            })
            .await;
            // Nothing got past the held refresh.
            assert_eq!(server.count("/inference"), 0);
            server.release_refresh();
        },
    );

    match waiter.last() {
        Some(AssistantEvent::Error { reason, .. }) => assert_eq!(*reason, StopReason::Aborted),
        other => panic!("canceled waiter ended with {other:?}"),
    }
    for events in [first, second] {
        assert_parsed(&events);
    }

    assert_eq!(server.count("/refresh"), 1, "exactly one locked refresh");
    assert_eq!(server.count("/inference"), 2);
    assert_eq!(server.rejected(), Vec::<String>::new());
    assert_eq!(server.unexpected(), 0);
    for request in server.requests("/inference") {
        // Rust's token header from the rotated credential, plus TS's rewrite.
        assert_eq!(request.header("authorization"), Some("Bearer access-2"));
        assert_eq!(request.header("x-lab"), Some("yes"));
        assert_eq!(request.body["lab"], true);
        assert!(request.head.starts_with("POST /inference "));
    }

    match host.store.get("lab").await.unwrap() {
        Some(Credential::Oauth {
            access,
            refresh,
            expires,
            ..
        }) => {
            assert_eq!(
                (access.as_str(), refresh.as_str()),
                ("access-2", "refresh-2")
            );
            assert!(expires > now_ms() + REFRESH_MARGIN_MS);
        }
        other => panic!("refresh stored {other:?}"),
    }
}

/// Rust parsed the lab SSE: the streamed text and the tool name as the server sent them.
fn assert_parsed(events: &[AssistantEvent]) {
    let Some(AssistantEvent::Done { reason, message }) = events.last() else {
        panic!("lab request did not finish: {:?}", events.last());
    };
    assert_eq!(*reason, StopReason::ToolUse);
    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, format!("{TEXT_HEAD}{TEXT_TAIL}"));
    let tools: Vec<&str> = message
        .tool_calls()
        .map(|call| call.name.as_str())
        .collect();
    assert_eq!(tools, [TOOL]);
}
