//! The runtime provider seam: a host-registered provider logs in, refreshes and rewrites
//! requests on all three wire APIs; registration replaces and restores built-in providers;
//! `reload` keeps registrations; owned provider layers stack over runtime registrations and
//! reserve unavailable providers without running them.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use async_trait::async_trait;
use axo_ai::testing::{fixture_model, http_header_values, serve_http};
use axo_ai::{
    ApiKind, AssistantEvent, Credential, CredentialSource, CredentialStore, DeviceCode, Error,
    FileCredentialStore, LoginInteraction, LoginPrompt, LoginSelect, Message, Model, Models,
    ModelsConfig, ModelsOptions, Provider, ProviderLogin, ProviderOrigin, ProviderRequest,
    RequestRewrite, ServiceTier, SimpleStreamOptions, Streamer, SystemMessage, TextBlock,
    Transcript, is_retryable_error, now_ms,
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Answers `count` connections in turn with a 400 and returns each request's head and body.
fn server(count: usize) -> (String, JoinHandle<Vec<(String, Value)>>) {
    let body = r#"{"error":{"message":"stop"}}"#;
    let response = format!(
        "HTTP/1.1 400 Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    serve_http(std::iter::repeat_n(response, count))
}

fn assert_server_error(id: &str, event: AssistantEvent) {
    assert!(
        matches!(&event, AssistantEvent::Error { message, .. }
            if message.failure.as_ref().and_then(|f| f.status) == Some(400)
                && message.error_message.as_deref().is_some_and(|s| s.contains("stop"))),
        "{id}: expected fixture HTTP 400 (stop), got {event:?}"
    );
}

fn model(provider: &str, id: &str, api: ApiKind, base_url: &str) -> Model {
    fixture_model(provider, id, id, api, base_url.into(), 200_000)
}

fn provider(id: &str, models: Vec<Model>) -> Provider {
    Provider {
        id: id.into(),
        models,
        env: Vec::new(),
        requires_key: false,
        oauth_only: false,
        login: None,
        rewrite: None,
    }
}

async fn load(dir: &tempfile::TempDir) -> (Models, Arc<FileCredentialStore>) {
    let store = Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));
    let models = Models::load(ModelsOptions {
        cache_dir: None,
        extended_context: false,
        config: ModelsConfig::default(),
        credentials: store.clone(),
        http_client: None,
    })
    .await
    .unwrap();
    (models, store)
}

/// Streams one request and returns its terminal event.
async fn stream(models: &Models, model: &Model) -> AssistantEvent {
    stream_events(models, model).await.pop().unwrap()
}

async fn stream_events(models: &Models, model: &Model) -> Vec<AssistantEvent> {
    let transcript = Transcript::new(vec![Message::System(SystemMessage {
        content: vec![TextBlock {
            text: "test".into(),
        }],
        ..Default::default()
    })]);
    Streamer::stream(
        models,
        model,
        transcript,
        SimpleStreamOptions::default(),
        CancellationToken::new(),
    )
    .collect()
    .await
}

fn oauth(access: &str, refresh: &str, expires: i64, tag: &str, base_url: &str) -> Credential {
    Credential::Oauth {
        access: access.into(),
        refresh: refresh.into(),
        expires,
        headers: BTreeMap::from([("x-scripted".to_owned(), tag.to_owned())]),
        base_url: Some(base_url.into()),
        extra: Default::default(),
    }
}

struct ScriptedLogin {
    base_url: String,
    refreshes: AtomicUsize,
}

#[async_trait]
impl ProviderLogin for ScriptedLogin {
    async fn login(&self, interaction: &dyn LoginInteraction) -> Result<Credential, Error> {
        interaction
            .open_url("https://login.test/authorize", None)
            .await;
        interaction.progress("waiting").await;
        let code = interaction
            .prompt(&LoginPrompt {
                message: "code".into(),
                placeholder: None,
                secret: false,
            })
            .await?;
        // Inside the 5-minute refresh margin: the first request refreshes.
        Ok(oauth(
            "access-0",
            &code,
            now_ms() + 60_000,
            "0",
            &self.base_url,
        ))
    }

    async fn refresh(&self, _credential: Credential) -> Result<Credential, Error> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(oauth(
            "access-1",
            "refresh-1",
            now_ms() + 3_600_000,
            "1",
            &self.base_url,
        ))
    }
}

struct ScriptedRewrite;

#[async_trait]
impl RequestRewrite for ScriptedRewrite {
    async fn rewrite(
        &self,
        _model: &Model,
        mut request: ProviderRequest,
    ) -> Result<ProviderRequest, Error> {
        request
            .headers
            .insert("x-original-url", request.url.parse().unwrap());
        let mut url = url::Url::parse(&request.url).unwrap();
        url.set_path("/rewritten");
        url.set_query(None);
        request.url = url.into();
        request
            .headers
            .insert("x-rewritten", "yes".parse().unwrap());
        request.body["rewritten"] = json!(true);
        Ok(request)
    }
}

#[tokio::test]
async fn rewrite_runs_once_across_http_retry() {
    struct CountingRewrite(AtomicUsize);

    #[async_trait]
    impl RequestRewrite for CountingRewrite {
        async fn rewrite(
            &self,
            model: &Model,
            request: ProviderRequest,
        ) -> Result<ProviderRequest, Error> {
            self.0.fetch_add(1, Ordering::SeqCst);
            ScriptedRewrite.rewrite(model, request).await
        }
    }

    let stop = r#"{"error":{"message":"stop"}}"#;
    let (url, server) = serve_http([
        "HTTP/1.1 429 Too Many Requests\r\nretry-after-ms: 0\r\nx-should-retry: False\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{stop}", stop.len()),
    ].into_iter());
    let dir = tempfile::tempdir().unwrap();
    let (models, _) = load(&dir).await;
    let model = model("scripted", "only", ApiKind::OpenAiCompletions, &url);
    let rewrite = Arc::new(CountingRewrite(AtomicUsize::new(0)));
    models
        .register(
            "host",
            Provider {
                rewrite: Some(rewrite.clone()),
                ..provider("scripted", vec![model.clone()])
            },
        )
        .unwrap();

    assert_server_error("scripted/only", stream(&models, &model).await);
    assert_eq!(rewrite.0.load(Ordering::SeqCst), 1);
    let requests = server.join().unwrap();
    let [(first, body), (second, retried)] = requests.as_slice() else {
        panic!("expected two captures, got {requests:?}");
    };
    assert!(first.starts_with("POST /rewritten "));
    assert_eq!(first.lines().next(), second.lines().next());
    for head in [first, second] {
        assert_eq!(http_header_values(head, "x-rewritten").next(), Some("yes"));
    }
    assert_eq!(body["rewritten"], true);
    assert_eq!(body, retried);
}

#[derive(Default)]
struct ScriptedInteraction {
    events: Mutex<Vec<String>>,
    prompt_gate: Option<(Notify, Notify)>,
}

#[async_trait]
impl LoginInteraction for ScriptedInteraction {
    async fn open_url(&self, url: &str, _instructions: Option<&str>) {
        self.events.lock().unwrap().push(format!("url {url}"));
    }

    async fn device_code(&self, code: &DeviceCode) {
        self.events
            .lock()
            .unwrap()
            .push(format!("code {}", code.user_code));
    }

    async fn prompt(&self, _prompt: &LoginPrompt) -> Result<String, Error> {
        if let Some((entered, resume)) = &self.prompt_gate {
            entered.notify_one();
            resume.notified().await;
        }
        Ok("pasted-code".into())
    }

    async fn select(&self, select: &LoginSelect) -> Result<String, Error> {
        Ok(select.options[0].id.clone())
    }

    async fn progress(&self, message: &str) {
        self.events
            .lock()
            .unwrap()
            .push(format!("progress {message}"));
    }
}

#[tokio::test]
async fn scripted_provider_logs_in_refreshes_and_rewrites() {
    let (url, server) = server(3);
    let dir = tempfile::tempdir().unwrap();
    let (models, store) = load(&dir).await;
    let login = Arc::new(ScriptedLogin {
        base_url: url.clone(),
        refreshes: AtomicUsize::new(0),
    });
    let apis = [
        ("anthropic", ApiKind::AnthropicMessages),
        ("completions", ApiKind::OpenAiCompletions),
        ("codex", ApiKind::OpenAiCodexResponses),
    ];
    models
        .register(
            "host",
            Provider {
                requires_key: true,
                login: Some(login.clone()),
                rewrite: Some(Arc::new(ScriptedRewrite)),
                ..provider(
                    "scripted",
                    apis.iter()
                        .map(|(id, api)| model("scripted", id, *api, "http://127.0.0.1:9"))
                        .collect(),
                )
            },
        )
        .unwrap();
    let slot = models.credential_slot("scripted").unwrap();

    // 1. The registration is visible.
    let info = models.providers();
    assert!(info.iter().any(|info| info.id == "scripted" && info.login));
    assert!(models.find("scripted/anthropic").is_some());

    // 2. Login runs through the interaction and stores the credential.
    let interaction = ScriptedInteraction::default();
    models.login("scripted", &interaction).await.unwrap();
    assert_eq!(
        *interaction.events.lock().unwrap(),
        ["url https://login.test/authorize", "progress waiting"]
    );
    assert!(matches!(
        store.get(&slot).await.unwrap(),
        Some(Credential::Oauth { access, refresh, .. }) if access == "access-0" && refresh == "pasted-code"
    ));

    // 3. Every wire API refreshes once, applies the stored auth, and sends the rewrite.
    for (id, _) in apis {
        let model = models.find(&format!("scripted/{id}")).unwrap();
        assert_server_error(id, stream(&models, &model).await);
    }
    let requests = server.join().unwrap();
    for ((head, body), (id, api)) in requests.iter().zip(apis) {
        assert!(head.starts_with("POST /rewritten "), "{id}: {head}");
        assert_eq!(
            http_header_values(head, "x-rewritten").next(),
            Some("yes"),
            "{id}"
        );
        assert_eq!(
            http_header_values(head, "x-scripted").next(),
            Some("1"),
            "{id}"
        );
        assert!(
            http_header_values(head, "x-original-url")
                .next()
                .is_some_and(|original| original.starts_with(&url)),
            "{id}: {head}"
        );
        assert_eq!(body["rewritten"], true, "{id}");
        assert_eq!(body["model"], id, "{id}");
        if api == ApiKind::AnthropicMessages {
            assert_eq!(
                http_header_values(head, "x-api-key").next(),
                Some("access-1")
            );
        } else {
            assert_eq!(
                http_header_values(head, "authorization").next(),
                Some("Bearer access-1"),
                "{id}"
            );
        }
        if api == ApiKind::OpenAiCodexResponses {
            assert_eq!(http_header_values(head, "chatgpt-account-id").next(), None);
        }
    }
    assert_eq!(login.refreshes.load(Ordering::SeqCst), 1);

    // 4. Unregister removes the provider and its models, even for callers holding an old model.
    let stale = models.find("scripted/anthropic").unwrap();
    assert!(models.unregister("scripted"));
    assert!(models.find("scripted/anthropic").is_none());
    assert!(!models.providers().iter().any(|info| info.id == "scripted"));
    assert!(matches!(
        store.get(&slot).await.unwrap(),
        Some(Credential::Oauth { access, .. }) if access == "access-1"
    ));
    let events = stream_events(&models, &stale).await;
    assert!(
        matches!(
            events.as_slice(),
            [AssistantEvent::Error { message, .. }]
                if message.error_message.as_deref() == Some("Unknown model: scripted/anthropic")
        ),
        "unexpected events: {events:?}"
    );

    // 5. A registration replaces a built-in provider of the same id; unregister restores it.
    let codex = |models: &Models| {
        models
            .providers()
            .into_iter()
            .find(|info| info.id == "openai-codex")
            .unwrap()
    };
    models
        .register(
            "host",
            provider(
                "openai-codex",
                vec![model(
                    "openai-codex",
                    "only",
                    ApiKind::OpenAiCodexResponses,
                    "http://127.0.0.1:9",
                )],
            ),
        )
        .unwrap();
    assert!(models.find("openai-codex/gpt-6-sol").is_none());
    assert!(models.find("openai-codex/only").is_some());
    assert!(!codex(&models).login);
    assert!(models.unregister("openai-codex"));
    assert!(models.find("openai-codex/gpt-6-sol").is_some());
    assert!(models.find("openai-codex/only").is_none());
    assert!(codex(&models).login);
}

#[tokio::test]
async fn config_base_url_beats_the_credential() {
    let (url, server) = server(1);
    let dir = tempfile::tempdir().unwrap();
    let (models, store) = load(&dir).await;
    let credential_url = "http://127.0.0.1:9";
    let credential = oauth("access", "refresh", i64::MAX, "1", credential_url);
    store
        .set("loaded/scripted", credential.clone())
        .await
        .unwrap();
    let config = ModelsConfig::parse_jsonc(
        &json!({"providers": {"scripted": {
            "api": "openai-completions",
            "baseUrl": url,
            "models": [{"id": "only"}]
        }}})
        .to_string(),
    )
    .unwrap();
    models.reload(config, false).await.unwrap();
    let model = models.find("scripted/only").unwrap();
    assert_eq!(models.auth(&model).await.unwrap().base_url, None);
    assert_server_error("scripted/only", stream(&models, &model).await);
    let requests = server.join().unwrap();
    assert_eq!(
        http_header_values(&requests[0].0, "authorization").next(),
        Some("Bearer access")
    );

    models.reload(ModelsConfig::default(), false).await.unwrap();
    models
        .register("host", provider("scripted", vec![model.clone()]))
        .unwrap();
    assert_eq!(models.auth(&model).await.unwrap().base_url, None);
    let slot = models.credential_slot("scripted").unwrap();
    store.set(&slot, credential).await.unwrap();
    assert_eq!(
        models.auth(&model).await.unwrap().base_url.as_deref(),
        Some(credential_url)
    );
}

enum FailingRewrite {
    Auth,
    Unavailable,
}

#[async_trait]
impl RequestRewrite for FailingRewrite {
    async fn rewrite(
        &self,
        _model: &Model,
        _request: ProviderRequest,
    ) -> Result<ProviderRequest, Error> {
        Err(match self {
            Self::Auth => Error::Auth("extension scripted is unavailable".into()),
            Self::Unavailable => Error::ProviderUnavailable {
                provider: "scripted".into(),
                reason: "rewrite refused".into(),
            },
        })
    }
}

#[tokio::test]
async fn failed_rewrite_ends_the_stream_without_sending() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let (models, _) = load(&dir).await;
    let model = model("scripted", "only", ApiKind::OpenAiCompletions, &url);
    for rewrite in [FailingRewrite::Auth, FailingRewrite::Unavailable] {
        let typed = matches!(rewrite, FailingRewrite::Unavailable);
        models
            .register(
                "host",
                Provider {
                    rewrite: Some(Arc::new(rewrite)),
                    ..provider("scripted", vec![model.clone()])
                },
            )
            .unwrap();

        let event = stream(&models, &model).await;
        if typed {
            assert_unavailable(event, "rewrite refused");
        } else {
            let AssistantEvent::Error { message, .. } = event else {
                panic!("not an error: {event:?}");
            };
            assert!(
                message
                    .error_message
                    .as_deref()
                    .unwrap()
                    .contains("extension scripted is unavailable")
            );
            assert!(message.failure.is_none());
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[tokio::test]
async fn reload_keeps_registrations_and_survives_bad_config() {
    let (url, server) = server(1);
    let dir = tempfile::tempdir().unwrap();
    let (models, _) = load(&dir).await;
    models
        .register(
            "host",
            provider(
                "scripted",
                vec![model("scripted", "only", ApiKind::OpenAiCompletions, &url)],
            ),
        )
        .unwrap();
    let before = models.find("scripted/only").unwrap();

    let config = ModelsConfig::parse_jsonc(
        r#"{"providers":{"custom":{
            "api":"openai-completions","baseUrl":"http://127.0.0.1:9","models":[{"id":"one"}]
        }}}"#,
    )
    .unwrap();
    models.reload(config, false).await.unwrap();
    assert!(models.find("custom/one").is_some());
    assert!(models.find("scripted/only").is_some());
    assert_server_error("scripted/only", stream(&models, &before).await);
    let requests = server.join().unwrap();
    assert_eq!(requests[0].1["model"], "only");

    let broken =
        ModelsConfig::parse_jsonc(r#"{"providers":{"broken":{"models":[{"id":"x"}]}}}"#).unwrap();
    assert!(matches!(
        models.reload(broken, false).await,
        Err(Error::Config(message)) if message.contains(r#"has no "api""#)
    ));
    assert!(models.find("custom/one").is_some());
    assert!(models.find("scripted/only").is_some());
}

/// Asserts a terminal, non-retryable `provider_unavailable` failure naming `reason`.
fn assert_unavailable(event: AssistantEvent, reason: &str) {
    let AssistantEvent::Error { message, .. } = event else {
        panic!("not an error: {event:?}");
    };
    assert!(message.error_message.as_deref().unwrap().contains(reason));
    let failure = message.failure.as_ref().unwrap();
    assert_eq!(failure.code.as_deref(), Some("provider_unavailable"));
    assert_eq!(failure.should_retry, Some(false));
    // The reason matches the transient patterns ("timed out"), yet is never retried.
    assert!(!is_retryable_error(&message));
}

#[tokio::test]
async fn provider_layers_stack_over_native_registrations_and_reveal_them() {
    let dir = tempfile::tempdir().unwrap();
    let (models, _) = load(&dir).await;
    let native = model(
        "lab",
        "native",
        ApiKind::OpenAiCompletions,
        "http://127.0.0.1:9",
    );
    models
        .register("host", provider("lab", vec![native]))
        .unwrap();
    const BAD_FACTORS: [f64; 4] = [0.0, -1.0, f64::NAN, f64::INFINITY];
    const DIAGNOSTIC: &str = "serviceTiers: multipliers must be positive numbers";
    let native = models.find("lab/native").unwrap();
    for factor in BAD_FACTORS {
        let mut bad = native.clone();
        bad.service_tiers.insert(ServiceTier::Fast, factor);
        assert!(
            matches!(models.register("host", provider("lab", vec![bad])),
            Err(Error::Config(message)) if message == DIAGNOSTIC)
        );
        assert_eq!(models.find("lab/native").as_ref(), Some(&native));
    }
    let a_model = model("lab", "a", ApiKind::OpenAiCompletions, "http://127.0.0.1:9");
    let b_model = model("lab", "b", ApiKind::AnthropicMessages, "http://127.0.0.1:9");

    let a = models.provider_layer();
    assert_eq!(a.hidden("lab"), Some(ProviderOrigin::Registered));
    assert_eq!(a.hidden("nothing"), None);
    a.replace(vec![(
        "a".into(),
        provider("lab", vec![a_model.clone()]),
        None,
    )])
    .unwrap();
    assert_eq!(models.find("lab/a"), Some(a_model.clone()));
    assert!(models.find("lab/native").is_none());
    assert_eq!(
        models
            .providers()
            .iter()
            .find(|info| info.id == "lab")
            .unwrap()
            .origin,
        ProviderOrigin::Layer
    );
    assert_eq!(
        models
            .providers()
            .iter()
            .find(|info| info.id == "openai-codex")
            .unwrap()
            .origin,
        ProviderOrigin::Loaded
    );

    let b = models.provider_layer();
    assert_eq!(b.hidden("lab"), Some(ProviderOrigin::Layer));
    let before = models.all();
    let unchanged = before
        .iter()
        .find(|model| model.provider == "openai-codex" && model.id == "gpt-6-sol")
        .unwrap();
    b.replace(vec![(
        "b".into(),
        provider("lab", vec![b_model.clone()]),
        None,
    )])
    .unwrap();
    assert_eq!(models.find("lab/b"), Some(b_model.clone()));
    assert!(models.find("lab/a").is_none());
    let after = models.all();
    let retained = after
        .iter()
        .find(|model| model.provider == unchanged.provider && model.id == unchanged.id)
        .unwrap();
    assert!(Arc::ptr_eq(unchanged, retained));
    assert!(before.iter().any(|m| m.provider == "lab" && m.id == "a"));
    assert!(!before.iter().any(|m| m.provider == "lab" && m.id == "b"));
    assert_eq!(
        models
            .providers()
            .iter()
            .filter(|info| info.id == "lab")
            .count(),
        1
    );

    // Every invalid batch leaves the previous layer intact, even if its first entry is valid.
    let stray = model(
        "other",
        "x",
        ApiKind::OpenAiCompletions,
        "http://127.0.0.1:9",
    );
    assert!(matches!(
        b.replace(vec![
            ("a".into(), provider("fresh", Vec::new()), None),
            ("b".into(), provider("lab", vec![stray]), None),
        ]),
        Err(Error::Config(_))
    ));
    assert!(matches!(
        b.replace(vec![
            ("a".into(), provider("fresh", Vec::new()), None),
            ("b".into(), provider("fresh", Vec::new()), None),
        ]),
        Err(Error::Config(message)) if message.contains("duplicate provider id")
    ));
    assert!(matches!(
        b.replace(vec![
            ("a".into(), provider("fresh", Vec::new()), None),
            (String::new(), provider("lab", Vec::new()), None),
        ]),
        Err(Error::Config(message)) if message == "credential owner must not be empty"
    ));
    assert!(matches!(
        models.register("", provider("fresh", Vec::new())),
        Err(Error::Config(message)) if message == "credential owner must not be empty"
    ));
    let mut mispriced = b_model.clone();
    mispriced.service_tiers.insert(ServiceTier::Standard, 2.0);
    let invalid = || provider("lab", vec![mispriced.clone()]);
    for result in [
        models.register("host", invalid()),
        b.replace(vec![
            ("a".into(), provider("fresh", Vec::new()), None),
            ("b".into(), invalid(), None),
        ]),
    ] {
        assert!(matches!(
            result,
            Err(Error::Config(message)) if message == r#"serviceTiers: "standard" has no multiplier"#
        ));
    }
    for factor in BAD_FACTORS {
        let mut bad = b_model.clone();
        bad.service_tiers.insert(ServiceTier::Fast, factor);
        assert!(matches!(b.replace(vec![
            ("a".into(), provider("fresh", Vec::new()), None),
            ("b".into(), provider("lab", vec![bad]), None),
        ]), Err(Error::Config(message)) if message == DIAGNOSTIC));
        assert_eq!(models.find("lab/b").as_ref(), Some(&b_model));
        assert!(!models.providers().iter().any(|info| info.id == "fresh"));
    }
    assert_eq!(models.find("lab/b"), Some(b_model));
    assert!(!models.providers().iter().any(|info| info.id == "fresh"));

    // The host changes its native registration and reloads while it is hidden.
    let changed = model(
        "lab",
        "changed",
        ApiKind::OpenAiCompletions,
        "http://127.0.0.1:9",
    );
    models
        .register("host", provider("lab", vec![changed.clone()]))
        .unwrap();
    models.reload(ModelsConfig::default(), false).await.unwrap();
    assert!(models.find("lab/changed").is_none());
    assert!(models.find("lab/b").is_some());

    // An empty replacement removes the newer layer's providers; dropping the older layer
    // reveals exactly the current native registration.
    b.replace(Vec::new()).unwrap();
    assert_eq!(models.find("lab/a"), Some(a_model));
    let emptied = models.all();
    b.replace(Vec::new()).unwrap();
    assert!(Arc::ptr_eq(&emptied, &models.all()));
    drop(a);
    assert_eq!(models.find("lab/changed"), Some(changed));
    assert_eq!(
        models
            .providers()
            .iter()
            .find(|info| info.id == "lab")
            .unwrap()
            .origin,
        ProviderOrigin::Registered
    );
    assert!(models.find("lab/a").is_none());
    assert!(models.find("lab/native").is_none());
    drop(b);
    assert!(models.find("lab/changed").is_some());
}

struct UnreachableLogin;

#[async_trait]
impl ProviderLogin for UnreachableLogin {
    async fn login(&self, _interaction: &dyn LoginInteraction) -> Result<Credential, Error> {
        panic!("an unavailable provider must not log in");
    }

    async fn refresh(&self, _credential: Credential) -> Result<Credential, Error> {
        panic!("an unavailable provider must not refresh");
    }
}

#[tokio::test]
async fn unavailable_layer_provider_stays_selectable_and_fails_before_auth() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let (models, _) = load(&dir).await;
    let lab = model("lab", "only", ApiKind::OpenAiCompletions, &url);
    let reason = "extension lab-provider timed out (group personal)";
    let layer = models.provider_layer();
    // Requires a key and has none: the unavailable check comes first.
    layer
        .replace(vec![(
            "extension/lab".into(),
            Provider {
                requires_key: true,
                login: Some(Arc::new(UnreachableLogin)),
                rewrite: Some(Arc::new(FailingRewrite::Auth)),
                ..provider("lab", vec![lab.clone()])
            },
            Some(reason.into()),
        )])
        .unwrap();

    assert_eq!(
        models.credential_slot("lab").unwrap(),
        "layer/extension/lab/lab"
    );
    assert_eq!(models.find("lab/only"), Some(lab.clone()));
    assert!(matches!(
        models.auth(&lab).await,
        Err(Error::ProviderUnavailable { provider, reason: r }) if provider == "lab" && r == reason
    ));
    assert!(matches!(
        models.login("lab", &ScriptedInteraction::default()).await,
        Err(Error::ProviderUnavailable { .. })
    ));
    assert_unavailable(stream(&models, &lab).await, reason);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    assert!(!layer.set_unavailable("other", None));
    assert!(layer.set_unavailable("lab", None));
    assert!(matches!(
        models.auth(&lab).await,
        Err(Error::NoApiKey { provider }) if provider == "lab"
    ));
}

struct InterruptedRefresh;

#[async_trait]
impl ProviderLogin for InterruptedRefresh {
    async fn login(&self, _interaction: &dyn LoginInteraction) -> Result<Credential, Error> {
        unreachable!()
    }

    async fn refresh(&self, _credential: Credential) -> Result<Credential, Error> {
        Err(Error::ProviderUnavailable {
            provider: "lab".into(),
            reason: "extension lab-provider stopped".into(),
        })
    }
}

#[tokio::test]
async fn interrupted_refresh_keeps_its_typed_error_through_the_store() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let (models, store) = load(&dir).await;
    let lab = model("lab", "only", ApiKind::OpenAiCompletions, &url);
    let layer = models.provider_layer();
    layer
        .replace(vec![(
            "extension/lab".into(),
            Provider {
                requires_key: true,
                login: Some(Arc::new(InterruptedRefresh)),
                ..provider("lab", vec![lab.clone()])
            },
            None,
        )])
        .unwrap();
    let expiring = oauth("access-0", "refresh-0", now_ms() + 60_000, "0", &url);
    let slot = models.credential_slot("lab").unwrap();
    store.set(&slot, expiring.clone()).await.unwrap();

    assert!(matches!(
        models.auth(&lab).await,
        Err(Error::ProviderUnavailable { .. })
    ));
    assert_unavailable(
        stream(&models, &lab).await,
        "extension lab-provider stopped",
    );
    assert_eq!(store.get(&slot).await.unwrap(), Some(expiring));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn credential_slots_follow_the_origin_not_the_winner() {
    let dir = tempfile::tempdir().unwrap();
    let (models, store) = load(&dir).await;
    let models = Arc::new(models);
    let config = ModelsConfig::parse_jsonc(
        r#"{"providers":{"lab":{"api":"openai-completions","baseUrl":"http://127.0.0.1:9","models":[{"id":"only"}]}}}"#,
    )
    .unwrap();
    models.reload(config, false).await.unwrap();
    let model = models.find("lab/only").unwrap();
    let login = Arc::new(ScriptedLogin {
        base_url: model.base_url.clone(),
        refreshes: AtomicUsize::new(0),
    });
    let entry = || Provider {
        login: Some(login.clone()),
        ..provider("lab", vec![model.clone()])
    };
    let key = |value: &str| Credential::ApiKey { key: value.into() };
    let fresh_slot = async |expected: &str| {
        let slot = models.credential_slot("lab").unwrap();
        assert_eq!(slot, expected);
        assert_eq!(
            models.credential_source("lab").await.unwrap(),
            CredentialSource::None
        );
        slot
    };

    // Bare pre-cutover credentials must neither authenticate a new principal nor be erased.
    store.set("lab", key("ambiguous")).await.unwrap();
    let loaded = fresh_slot("loaded/lab").await;
    let expired = oauth(
        "loaded-secret",
        "loaded-refresh",
        0,
        "loaded",
        &model.base_url,
    );
    store.set(&loaded, expired.clone()).await.unwrap();
    models.register("host", entry()).unwrap();
    let native = fresh_slot("registered/host/lab").await;
    store.set(&native, key("native-secret")).await.unwrap();

    let a = models.provider_layer();
    a.replace(vec![("ext/a".into(), entry(), None)]).unwrap();
    let alpha = fresh_slot("layer/ext/a/lab").await;
    store.set(&alpha, key("alpha-secret")).await.unwrap();
    let b = models.provider_layer();
    b.replace(vec![("ext/b".into(), entry(), None)]).unwrap();
    let beta = fresh_slot("layer/ext/b/lab").await;
    models
        .login("lab", &ScriptedInteraction::default())
        .await
        .unwrap();
    assert_eq!(
        models.auth(&model).await.unwrap().api_key.as_deref(),
        Some("access-1")
    );
    assert_eq!(login.refreshes.load(Ordering::SeqCst), 1);
    assert!(matches!(
        store.get(&beta).await.unwrap(),
        Some(Credential::Oauth { access, refresh, .. }) if access == "access-1" && refresh == "refresh-1"
    ));
    assert_eq!(store.get(&alpha).await.unwrap(), Some(key("alpha-secret")));
    assert_eq!(
        store.get(&native).await.unwrap(),
        Some(key("native-secret"))
    );
    assert_eq!(store.get(&loaded).await.unwrap(), Some(expired.clone()));

    drop(b);
    assert_eq!(
        models.auth(&model).await.unwrap().api_key.as_deref(),
        Some("alpha-secret")
    );
    drop(a);
    assert_eq!(
        models.auth(&model).await.unwrap().api_key.as_deref(),
        Some("native-secret")
    );

    // Swap owners while the user is responding. Completing the login still belongs to host.
    let interaction = ScriptedInteraction {
        prompt_gate: Some((Notify::new(), Notify::new())),
        ..Default::default()
    };
    let (entered, resume) = interaction.prompt_gate.as_ref().unwrap();
    let (result, other) = tokio::join!(models.login("lab", &interaction), async {
        entered.notified().await;
        models.register("other", entry()).unwrap();
        let other = fresh_slot("registered/other/lab").await;
        resume.notify_one();
        other
    });
    result.unwrap();
    assert_eq!(store.get(&other).await.unwrap(), None);
    assert!(matches!(
        store.get(&native).await.unwrap(),
        Some(Credential::Oauth { access, refresh, .. }) if access == "access-0" && refresh == "pasted-code"
    ));
    assert!(models.unregister("lab"));
    assert_eq!(models.credential_slot("lab").unwrap(), loaded);
    assert_eq!(
        models.credential_source("lab").await.unwrap(),
        CredentialSource::Oauth
    );
    assert_eq!(store.get(&loaded).await.unwrap(), Some(expired));
    assert_eq!(store.get("lab").await.unwrap(), Some(key("ambiguous")));
}

struct ApiKeyLogin;

#[async_trait]
impl ProviderLogin for ApiKeyLogin {
    async fn login(&self, _interaction: &dyn LoginInteraction) -> Result<Credential, Error> {
        Ok(Credential::ApiKey {
            key: "from-login".into(),
        })
    }

    async fn refresh(&self, _credential: Credential) -> Result<Credential, Error> {
        unreachable!()
    }
}

#[tokio::test]
async fn claude_subscription_policy_follows_winning_provider_not_wire_api() {
    let dir = tempfile::tempdir().unwrap();
    let (models, store) = load(&dir).await;
    let (url, server) = server(1);
    let lab = model("lab", "only", ApiKind::AnthropicMessages, &url);
    let interaction = ScriptedInteraction::default();
    let registration = || Provider {
        requires_key: true,
        ..provider("lab", vec![lab.clone()])
    };
    models
        .register(
            "host",
            Provider {
                login: Some(Arc::new(ApiKeyLogin)),
                ..registration()
            },
        )
        .unwrap();
    assert!(models.set_api_key("lab", " \t".into()).await.is_err());
    models.set_api_key("lab", "from-host".into()).await.unwrap();
    assert_eq!(
        models.auth(&lab).await.unwrap().api_key.as_deref(),
        Some("from-host")
    );
    models.login("lab", &interaction).await.unwrap();
    assert_eq!(
        models.auth(&lab).await.unwrap().api_key.as_deref(),
        Some("from-login")
    );

    let oauth_only = || Provider {
        oauth_only: true,
        ..registration()
    };
    assert!(models.register("invalid", oauth_only()).is_err());
    let layer = models.provider_layer();
    let replace = |login: Arc<dyn ProviderLogin>| {
        layer
            .replace(vec![(
                "ext/lab".into(),
                Provider {
                    login: Some(login),
                    ..oauth_only()
                },
                None,
            )])
            .unwrap();
    };
    replace(Arc::new(ApiKeyLogin));
    let slot = models.credential_slot("lab").unwrap();
    assert!(
        models
            .providers()
            .iter()
            .any(|info| info.id == "lab" && info.oauth_only)
    );
    assert!(matches!(
        models.set_api_key("lab", "forbidden".into()).await,
        Err(Error::LoginRequired { .. })
    ));
    assert!(models.login("lab", &interaction).await.is_err());
    assert_eq!(store.get(&slot).await.unwrap(), None);

    store
        .set(
            &slot,
            Credential::ApiKey {
                key: "stale".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        models.auth(&lab).await,
        Err(Error::LoginRequired { .. })
    ));
    replace(Arc::new(ScriptedLogin {
        base_url: url,
        refreshes: AtomicUsize::new(0),
    }));
    models.login("lab", &interaction).await.unwrap();
    assert_eq!(
        models.credential_source("lab").await.unwrap(),
        CredentialSource::Oauth
    );
    assert_server_error("lab", stream(&models, &lab).await);
    let (head, body) = server.join().unwrap().pop().unwrap();
    assert_eq!(
        http_header_values(&head, "x-api-key").next(),
        Some("access-1")
    );
    assert_eq!(http_header_values(&head, "x-scripted").next(), Some("1"));
    assert_eq!(http_header_values(&head, "authorization").next(), None);
    assert_eq!(body["system"][0]["text"], "test");
}

#[tokio::test]
async fn keyless_codex_rewrite_reaches_http() {
    let (url, server) = server(1);
    let model = model("lab", "only", ApiKind::OpenAiCodexResponses, &url);
    let dir = tempfile::tempdir().unwrap();
    let (models, store) = load(&dir).await;
    models
        .register(
            "host",
            Provider {
                rewrite: Some(Arc::new(ScriptedRewrite)),
                ..provider("lab", vec![model.clone()])
            },
        )
        .unwrap();
    let AssistantEvent::Error { message, .. } = stream(&models, &model).await else {
        panic!("expected the test server's error");
    };
    assert!(message.error_message.as_deref().unwrap().contains("stop"));
    let requests = server.join().unwrap();
    let head = &requests[0].0;
    assert!(head.starts_with("POST /rewritten "), "{head}");
    assert_eq!(http_header_values(head, "x-rewritten").next(), Some("yes"));
    assert_eq!(http_header_values(head, "authorization").next(), None);
    assert_eq!(http_header_values(head, "chatgpt-account-id").next(), None);

    // A required but missing/empty key must fail at setup, before even invoking the rewrite.
    models
        .register(
            "host",
            Provider {
                requires_key: true,
                rewrite: Some(Arc::new(FailingRewrite::Auth)),
                ..provider("lab", vec![model.clone()])
            },
        )
        .unwrap();
    let slot = models.credential_slot("lab").unwrap();
    for credential in [None, Some(Credential::ApiKey { key: String::new() })] {
        if let Some(credential) = credential {
            store.set(&slot, credential).await.unwrap();
        }
        let events = stream_events(&models, &model).await;
        assert!(
            matches!(
                events.as_slice(),
                [AssistantEvent::Error { message, .. }]
                    if message.error_message.as_deref() == Some("No API key for provider: lab")
            ),
            "{events:?}"
        );
    }
}
