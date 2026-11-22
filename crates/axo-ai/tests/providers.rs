//! The runtime provider seam: a host-registered provider logs in, refreshes and rewrites
//! requests on all three wire APIs; registration replaces and restores built-in providers;
//! `reload` keeps registrations; owned provider layers stack over runtime registrations and
//! reserve unavailable providers without running them.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use async_trait::async_trait;
use axo_ai::testing::read_http_request_parts;
use axo_ai::{
    ApiKind, AssistantEvent, Compat, Cost, Credential, CredentialStore, DeviceCode, Error,
    FileCredentialStore, InputKind, LoginInteraction, LoginPrompt, LoginSelect, Message, Model,
    Models, ModelsConfig, ModelsOptions, Provider, ProviderLogin, ProviderOrigin, ProviderRequest,
    RequestRewrite, SimpleStreamOptions, Streamer, SystemMessage, TextBlock, Transcript,
    is_retryable_error, now_ms,
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Answers `count` connections in turn with a 400 and returns each request's head and body.
fn server(count: usize) -> (String, JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let body = r#"{"error":{"message":"stop"}}"#;
    let response = format!(
        "HTTP/1.1 400 Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let thread = std::thread::spawn(move || {
        (0..count)
            .map(|_| {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request_parts(&mut stream);
                stream.write_all(response.as_bytes()).unwrap();
                request
            })
            .collect()
    });
    (url, thread)
}

fn model(provider: &str, id: &str, api: ApiKind, base_url: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api,
        provider: provider.into(),
        base_url: base_url.into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: Cost::default(),
        service_tiers: Default::default(),
        context_window: 200_000,
        max_tokens: 4096,
        compat: Compat::default(),
    }
}

fn provider(id: &str, models: Vec<Model>) -> Provider {
    Provider {
        id: id.into(),
        models,
        env: Vec::new(),
        requires_key: false,
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

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
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

#[derive(Default)]
struct ScriptedInteraction {
    events: Mutex<Vec<String>>,
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
        .register(Provider {
            requires_key: true,
            login: Some(login.clone()),
            rewrite: Some(Arc::new(ScriptedRewrite)),
            ..provider(
                "scripted",
                apis.iter()
                    .map(|(id, api)| model("scripted", id, *api, "http://127.0.0.1:9"))
                    .collect(),
            )
        })
        .unwrap();

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
        store.get("scripted").await.unwrap(),
        Some(Credential::Oauth { access, refresh, .. }) if access == "access-0" && refresh == "pasted-code"
    ));

    // 3. Every wire API refreshes once, applies the stored auth, and sends the rewrite.
    for (id, _) in apis {
        let model = models.find(&format!("scripted/{id}")).unwrap();
        assert!(matches!(
            stream(&models, &model).await,
            AssistantEvent::Error { .. }
        ));
    }
    let requests = server.join().unwrap();
    for ((head, body), (id, api)) in requests.iter().zip(apis) {
        assert!(head.starts_with("POST /rewritten "), "{id}: {head}");
        assert_eq!(header(head, "x-rewritten"), Some("yes"), "{id}");
        assert_eq!(header(head, "x-scripted"), Some("1"), "{id}");
        assert!(
            header(head, "x-original-url").is_some_and(|original| original.starts_with(&url)),
            "{id}: {head}"
        );
        assert_eq!(body["rewritten"], true, "{id}");
        assert_eq!(body["model"], id, "{id}");
        if api == ApiKind::AnthropicMessages {
            assert_eq!(header(head, "x-api-key"), Some("access-1"));
        } else {
            assert_eq!(
                header(head, "authorization"),
                Some("Bearer access-1"),
                "{id}"
            );
        }
        if api == ApiKind::OpenAiCodexResponses {
            assert_eq!(header(head, "chatgpt-account-id"), None);
        }
    }
    assert_eq!(login.refreshes.load(Ordering::SeqCst), 1);

    // 4. Unregister removes the provider and its models, even for callers holding an old model.
    let stale = models.find("scripted/anthropic").unwrap();
    assert!(models.unregister("scripted"));
    assert!(models.find("scripted/anthropic").is_none());
    assert!(!models.providers().iter().any(|info| info.id == "scripted"));
    assert!(matches!(
        store.get("scripted").await.unwrap(),
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
        .register(provider(
            "openai-codex",
            vec![model(
                "openai-codex",
                "only",
                ApiKind::OpenAiCodexResponses,
                "http://127.0.0.1:9",
            )],
        ))
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
    store.set("scripted", credential).await.unwrap();
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
    let AssistantEvent::Error { message, .. } = stream(&models, &model).await else {
        panic!("expected the test server's error");
    };
    let error = message.error_message.as_deref();
    assert!(
        error.is_some_and(|error| error.contains("stop")),
        "unexpected provider error: {error:?}"
    );
    let requests = server.join().unwrap();
    assert_eq!(
        header(&requests[0].0, "authorization"),
        Some("Bearer access")
    );

    models.reload(ModelsConfig::default(), false).await.unwrap();
    models
        .register(provider("scripted", vec![model.clone()]))
        .unwrap();
    assert_eq!(
        models.auth(&model).await.unwrap().base_url.as_deref(),
        Some(credential_url)
    );
}

struct FailingRewrite;

#[async_trait]
impl RequestRewrite for FailingRewrite {
    async fn rewrite(
        &self,
        _model: &Model,
        _request: ProviderRequest,
    ) -> Result<ProviderRequest, Error> {
        Err(Error::Auth("extension scripted is unavailable".into()))
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
    models
        .register(Provider {
            rewrite: Some(Arc::new(FailingRewrite)),
            ..provider("scripted", vec![model.clone()])
        })
        .unwrap();

    match stream(&models, &model).await {
        AssistantEvent::Error { message, .. } => assert!(
            message
                .error_message
                .as_deref()
                .unwrap()
                .contains("extension scripted is unavailable")
        ),
        event => panic!("not an error: {event:?}"),
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn reload_keeps_registrations_and_survives_bad_config() {
    let (url, server) = server(1);
    let dir = tempfile::tempdir().unwrap();
    let (models, _) = load(&dir).await;
    models
        .register(provider(
            "scripted",
            vec![model("scripted", "only", ApiKind::OpenAiCompletions, &url)],
        ))
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
    assert!(matches!(
        stream(&models, &before).await,
        AssistantEvent::Error { .. }
    ));
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
    let models = Arc::new(load(&dir).await.0);
    let native = model(
        "lab",
        "native",
        ApiKind::OpenAiCompletions,
        "http://127.0.0.1:9",
    );
    models.register(provider("lab", vec![native])).unwrap();
    let a_model = model("lab", "a", ApiKind::OpenAiCompletions, "http://127.0.0.1:9");
    let b_model = model("lab", "b", ApiKind::AnthropicMessages, "http://127.0.0.1:9");

    let a = models.provider_layer();
    assert_eq!(a.hidden("lab"), Some(ProviderOrigin::Registered));
    assert_eq!(a.hidden("nothing"), None);
    a.replace(vec![(provider("lab", vec![a_model.clone()]), None)])
        .unwrap();
    assert_eq!(models.find("lab/a"), Some(a_model.clone()));
    assert!(models.find("lab/native").is_none());

    let b = models.provider_layer();
    assert_eq!(b.hidden("lab"), Some(ProviderOrigin::Layer));
    b.replace(vec![(provider("lab", vec![b_model.clone()]), None)])
        .unwrap();
    assert_eq!(models.find("lab/b"), Some(b_model));
    assert!(models.find("lab/a").is_none());
    assert_eq!(
        models
            .providers()
            .iter()
            .filter(|info| info.id == "lab")
            .count(),
        1
    );

    // An invalid replacement changes nothing: wrong model owner, then a repeated id.
    let stray = model(
        "other",
        "x",
        ApiKind::OpenAiCompletions,
        "http://127.0.0.1:9",
    );
    assert!(matches!(
        b.replace(vec![
            (provider("fresh", Vec::new()), None),
            (provider("lab", vec![stray]), None),
        ]),
        Err(Error::Config(_))
    ));
    assert!(matches!(
        b.replace(vec![
            (provider("fresh", Vec::new()), None),
            (provider("fresh", Vec::new()), None),
        ]),
        Err(Error::Config(message)) if message.contains("duplicate provider id")
    ));
    assert!(models.find("lab/b").is_some());
    assert!(!models.providers().iter().any(|info| info.id == "fresh"));

    // The host changes its native registration and reloads while it is hidden.
    let changed = model(
        "lab",
        "changed",
        ApiKind::OpenAiCompletions,
        "http://127.0.0.1:9",
    );
    models
        .register(provider("lab", vec![changed.clone()]))
        .unwrap();
    models.reload(ModelsConfig::default(), false).await.unwrap();
    assert!(models.find("lab/changed").is_none());
    assert!(models.find("lab/b").is_some());

    // An empty replacement removes the newer layer's providers; dropping the older layer
    // reveals exactly the current native registration.
    b.replace(Vec::new()).unwrap();
    assert_eq!(models.find("lab/a"), Some(a_model));
    drop(a);
    assert_eq!(models.find("lab/changed"), Some(changed));
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
    let models = Arc::new(models);
    let lab = model("lab", "only", ApiKind::OpenAiCompletions, &url);
    let reason = "extension lab-provider timed out (group personal)";
    let layer = models.provider_layer();
    // Requires a key and has none: the unavailable check comes first.
    layer
        .replace(vec![(
            Provider {
                requires_key: true,
                login: Some(Arc::new(UnreachableLogin)),
                rewrite: Some(Arc::new(FailingRewrite)),
                ..provider("lab", vec![lab.clone()])
            },
            Some(reason.into()),
        )])
        .unwrap();

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
    let models = Arc::new(models);
    let lab = model("lab", "only", ApiKind::OpenAiCompletions, &url);
    let layer = models.provider_layer();
    layer
        .replace(vec![(
            Provider {
                requires_key: true,
                login: Some(Arc::new(InterruptedRefresh)),
                ..provider("lab", vec![lab.clone()])
            },
            None,
        )])
        .unwrap();
    let expiring = oauth("access-0", "refresh-0", now_ms() + 60_000, "0", &url);
    store.set("lab", expiring.clone()).await.unwrap();

    assert!(matches!(
        models.auth(&lab).await,
        Err(Error::ProviderUnavailable { .. })
    ));
    assert_unavailable(
        stream(&models, &lab).await,
        "extension lab-provider stopped",
    );
    assert_eq!(store.get("lab").await.unwrap(), Some(expiring));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
