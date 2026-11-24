use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use axo_ai::thinking::supported_levels;
use axo_ai::{
    ApiKind, Error, FileCredentialStore, Models, ModelsConfig, ModelsOptions, Provider,
    ThinkingLevel, TranscriptToolSupport,
};
use serde_json::{Value, json};

/// A loopback HTTP server answering each request path (query ignored) with the status and
/// JSON body set for it, 404 otherwise, and recording every request head.
struct Feed {
    url: String,
    routes: Arc<Mutex<HashMap<String, (u16, String)>>>,
    requests: Arc<Mutex<Vec<String>>>,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl Feed {
    fn start() -> Feed {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut feed = Feed {
            url: format!("http://{addr}"),
            routes: Arc::default(),
            requests: Arc::default(),
            addr,
            stop: Arc::default(),
            server: None,
        };
        let (routes, requests) = (Arc::clone(&feed.routes), Arc::clone(&feed.requests));
        let stop = Arc::clone(&feed.stop);
        feed.server = Some(std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let mut stream = stream.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read(&mut byte).unwrap() == 0 {
                        break;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                let target = head.split(' ').nth(1).unwrap_or_default();
                let path = target.split('?').next().unwrap_or_default();
                let (status, body) = routes
                    .lock()
                    .unwrap()
                    .get(path)
                    .cloned()
                    .unwrap_or((404, "{}".into()));
                requests.lock().unwrap().push(head);
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        }));
        feed
    }

    fn set(&self, path: &str, status: u16, body: &Value) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.into(), (status, body.to_string()));
    }

    fn gets(&self, path: &str) -> usize {
        let prefix = format!("GET {path} ");
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|head| head.starts_with(&prefix))
            .count()
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wakes the blocked accept; without it the thread would never see `stop`.
        if TcpStream::connect(self.addr).is_ok()
            && let Some(server) = self.server.take()
        {
            let _ = server.join();
        }
    }
}

/// A one-provider catalog whose provider `p` has a Chat Completions model per id.
fn catalog_of(ids: &[&str]) -> Value {
    let models: serde_json::Map<String, Value> = ids
        .iter()
        .map(|id| {
            let row = dev_row("chat-completions", "@ai-sdk/openai-compatible", json!({}));
            ((*id).to_owned(), row)
        })
        .collect();
    json!({"p": {"models": models}})
}

/// Loads the registry from `dir` (catalog caches and `auth.json`) with `config` as `models.jsonc`.
async fn load(dir: &Path, config: Value, extended_context: bool) -> Models {
    Models::load(ModelsOptions {
        cache_dir: Some(dir.to_path_buf()),
        extended_context,
        config: serde_json::from_value(config).unwrap(),
        credentials: Arc::new(FileCredentialStore::new(dir.join("auth.json"))),
        http_client: None,
    })
    .await
    .unwrap()
}

/// A tool-calling chat row of the models.dev v2 feed served over `api` (`messages` or
/// `chat-completions`) through `sdk`, with `extra` merged over it.
fn dev_row(api: &str, sdk: &str, extra: Value) -> Value {
    let mut row = json!({
        "type": "chat",
        "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": true}},
        "limit": {"context": 200000, "output": 64000},
        "recommended_api": api,
        "api": {api: {"base_url": "https://gw.example/v1/", "sdk": {"aisdk": sdk}}},
    });
    merge(&mut row, extra);
    row
}

fn merge(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                merge(target.entry(key).or_insert(Value::Null), value);
            }
        }
        (target, patch) => *target = patch,
    }
}

#[tokio::test]
async fn catalog_rows_map_from_feed_data() {
    let dir = tempfile::tempdir().unwrap();
    let messages = |extra| dev_row("messages", "@ai-sdk/anthropic", extra);
    let completions = |extra| dev_row("chat-completions", "@ai-sdk/openai-compatible", extra);
    let efforts = json!(["low", "medium", "high", "xhigh", "max"]);
    let catalog = json!({"p": {"env": ["P_KEY"], "models": {
        "adaptive": messages(json!({
            "capabilities": {"temperature": false, "reasoning": {"effort": efforts}},
            "api": {"messages": {"capabilities": {
                "system_updates": true, "effort_updates": true,
                "tools": {"updates": ["add", "remove", "replace"]}
            }}}
        })),
        "budget": messages(json!({"capabilities": {"reasoning": {
            "effort": efforts, "budget": {"min": 1024}
        }}})),
        "toggle": messages(json!({"capabilities": {"reasoning": {
            "effort": efforts, "toggle": true
        }}})),
        "completions": completions(json!({"capabilities": {"reasoning": {
            "effort": ["none", "low", "high"]
        }}})),
        "priced": completions(json!({"cost": {"text": {
            "input": {"price": 1, "cache": {"read": 0.1, "write": {"30m": 2.5, "12h": 5}},
                      "overrides": [{"when": {"context": {"gt": 272000}}, "price": 2}]},
            "output": {"price": 4,
                       "overrides": [{"when": {"context": {"gt": 272000}}, "price": 6},
                                     {"when": {"service_tier": "flex"}, "price": 1}]}
        }}})),
        "responses": dev_row("responses", "@ai-sdk/openai", json!({})),
        "groq": dev_row("chat-completions", "@ai-sdk/groq", json!({})),
        "image": messages(json!({"type": "image"})),
    }}});
    std::fs::write(
        dir.path().join("models.dev.v2.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    let models = load(dir.path(), json!({"refreshInterval": 0}), false).await;

    let adaptive = models.find("p/adaptive").unwrap();
    assert_eq!(adaptive.api, ApiKind::AnthropicMessages);
    assert_eq!(adaptive.base_url, "https://gw.example");
    assert_eq!(adaptive.compat.force_adaptive_thinking, Some(true));
    assert_eq!(
        supported_levels(&adaptive),
        [
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
            ThinkingLevel::Max
        ]
    );
    assert_eq!(
        adaptive.compat.supports_mid_convo_system_messages,
        Some(true)
    );
    assert_eq!(adaptive.compat.supports_mid_convo_effort, Some(true));
    assert_eq!(adaptive.compat.supports_mid_convo_tool_changes, Some(true));
    assert_eq!(adaptive.compat.supports_temperature, Some(false));
    assert_eq!(adaptive.tool_support(), TranscriptToolSupport::Changes);

    let budget = models.find("p/budget").unwrap();
    assert_eq!(budget.compat.force_adaptive_thinking, None);
    assert_eq!(budget.thinking_level_map, None);

    let toggle = models.find("p/toggle").unwrap();
    assert_eq!(supported_levels(&toggle)[0], ThinkingLevel::Off);

    let completions = models.find("p/completions").unwrap();
    assert_eq!(completions.api, ApiKind::OpenAiCompletions);
    assert_eq!(completions.base_url, "https://gw.example/v1");
    assert_eq!(
        completions.thinking_level_map.unwrap()[&ThinkingLevel::Off].as_deref(),
        Some("none")
    );

    for absent in ["p/responses", "p/groq", "p/image"] {
        assert!(models.find(absent).is_none(), "{absent}");
    }

    let cost = models.find("p/priced").unwrap().cost.unwrap();
    assert_eq!(cost.cache_write, 2.5);
    assert_eq!(cost.tiers.len(), 1);
    let tier = &cost.tiers[0];
    assert_eq!(tier.input_tokens_above, 272_000);
    assert_eq!((tier.input, tier.output), (2.0, 6.0));
    assert_eq!((tier.cache_read, tier.cache_write), (0.1, 2.5));
}

#[tokio::test]
async fn refresh_applies_new_catalog_data_and_keeps_the_last_good_one() {
    let dir = tempfile::tempdir().unwrap();
    let feed = Feed::start();
    feed.set("/api.json", 200, &catalog_of(&["a"]));
    let config = json!({"refreshInterval": 0, "catalogUrl": format!("{}/api.json", feed.url)});
    let models = load(dir.path(), config.clone(), false).await;
    assert!(models.find("p/a").is_some());

    feed.set("/api.json", 200, &catalog_of(&["a", "b"]));
    models.refresh().await.unwrap();
    assert!(models.find("p/b").is_some());

    let cache = dir.path().join("models.dev.v2.json");
    let cached = std::fs::read(&cache).unwrap();
    feed.set("/api.json", 500, &json!({}));
    assert!(models.refresh().await.is_err());
    feed.set("/api.json", 200, &json!({}));
    let error = models.refresh().await.unwrap_err().to_string();
    assert!(error.contains("no usable models"), "{error}");
    assert!(models.find("p/a").is_some() && models.find("p/b").is_some());
    assert_eq!(std::fs::read(&cache).unwrap(), cached);

    // The catalog lives in memory: neither a reload without the cache nor a failed refresh
    // loses it.
    std::fs::remove_file(&cache).unwrap();
    models
        .reload(serde_json::from_value(config).unwrap(), false)
        .await
        .unwrap();
    assert!(models.find("p/a").is_some() && models.find("p/b").is_some());
    feed.set("/api.json", 500, &json!({}));
    assert!(models.refresh().await.is_err());
    assert!(models.find("p/a").is_some() && models.find("p/b").is_some());
}

#[tokio::test]
async fn rejected_catalog_does_not_replace_bootable_cache() {
    let dir = tempfile::tempdir().unwrap();
    let feed = Feed::start();
    feed.set("/api.json", 200, &catalog_of(&["a"]));
    let config = json!({
        "refreshInterval": 0,
        "catalogUrl": format!("{}/api.json", feed.url),
        "providers": {"p": {"models": [{"id": "a", "name": "patched"}]}}
    });
    let models = load(dir.path(), config.clone(), false).await;
    let cache = dir.path().join("models.dev.v2.json");
    let cached = std::fs::read(&cache).unwrap();

    feed.set("/api.json", 200, &catalog_of(&["b"]));
    assert!(matches!(
        models.refresh().await,
        Err(Error::Config(message)) if message.contains(r#"models.jsonc: model p/a has no "api""#)
    ));
    assert_eq!(std::fs::read(&cache).unwrap(), cached);
    assert_eq!(models.find("p/a").unwrap().name, "patched");
    assert!(models.find("p/b").is_none());
    models
        .reload(serde_json::from_value(config.clone()).unwrap(), false)
        .await
        .unwrap();
    assert_eq!(models.find("p/a").unwrap().name, "patched");
    drop(models);
    let reopened = load(dir.path(), config, false).await;
    assert_eq!(reopened.find("p/a").unwrap().name, "patched");
    assert_eq!(feed.gets("/api.json"), 2);
}

#[tokio::test]
async fn stale_start_discovers_config_provider_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = std::fs::File::create(dir.path().join("models.dev.v2.json")).unwrap();
    serde_json::to_writer(&mut cache, &catalog_of(&["old"])).unwrap();
    cache.set_modified(SystemTime::UNIX_EPOCH).unwrap();
    drop(cache);

    let feed = Feed::start();
    feed.set("/api.json", 200, &catalog_of(&["new"]));
    feed.set("/v1/models", 200, &json!({"data": [{"id": "discovered"}]}));
    let config = json!({
        "refreshInterval": 3600,
        "catalogUrl": format!("{}/api.json", feed.url),
        "providers": {"local": {"api": "openai-completions", "baseUrl": format!("{}/v1", feed.url)}}
    });
    let models = load(dir.path(), config, false).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while models.find("p/new").is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stale catalog was not refreshed");
    let counts = || [feed.gets("/api.json"), feed.gets("/v1/models")];
    assert_eq!(counts(), [1, 1]);
    assert!(models.find("local/discovered").is_some());

    models.refresh().await.unwrap();
    assert_eq!(counts(), [2, 2]);
}

#[tokio::test]
async fn failed_discovery_keeps_previous_models() {
    let dir = tempfile::tempdir().unwrap();
    let feed = Feed::start();
    feed.set("/v1/models", 200, &json!({"data": [{"id": "m1"}]}));
    let config = json!({
        "refreshInterval": 0,
        "catalogUrl": format!("{}/missing", feed.url),
        "providers": {"local": {"api": "openai-completions", "baseUrl": format!("{}/v1", feed.url)}}
    });
    let models = load(dir.path(), config.clone(), false).await;
    assert!(models.find("local/m1").is_some());

    feed.set("/v1/models", 500, &json!({}));
    models
        .reload(serde_json::from_value(config).unwrap(), false)
        .await
        .unwrap();
    assert!(models.find("local/m1").is_some());
}

#[tokio::test]
async fn hidden_discovery_keeps_loaded_ids_without_contacting_the_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let feed = Feed::start();
    feed.set("/api.json", 200, &catalog_of(&["a"]));
    feed.set("/v1/models", 200, &json!({"data": [{"id": "old"}]}));
    feed.set(
        "/codex/models",
        200,
        &json!({"models": [{"slug": "account", "context_window": 4096}]}),
    );
    std::fs::write(
        dir.path().join("auth.json"),
        r#"{"loaded/openai-codex":{"type":"api_key","key":"token"}}"#,
    )
    .unwrap();
    let config = json!({
        "refreshInterval": 0,
        "catalogUrl": format!("{}/api.json", feed.url),
        "providers": {
            "local": {"api": "openai-completions", "baseUrl": format!("{}/v1", feed.url)},
            "openai-codex": {"baseUrl": feed.url}
        }
    });
    let models = Arc::new(load(dir.path(), config.clone(), false).await);
    let request_counts = || {
        let heads = feed.requests.lock().unwrap();
        ["GET /v1/models ", "GET /codex/models?"]
            .map(|prefix| heads.iter().filter(|head| head.starts_with(prefix)).count())
    };
    assert_eq!(request_counts(), [1, 1]);
    let old = models.find("local/old").unwrap();
    let account = models.find("openai-codex/account").unwrap();
    feed.set("/v1/models", 200, &json!({"data": [{"id": "new"}]}));
    feed.set("/api.json", 200, &catalog_of(&["b"]));

    for gated in [false, true] {
        let layer = models.provider_layer();
        let replacements = [&old, &account].map(|model| {
            let mut winner = model.clone();
            winner.id = "winner".into();
            Provider {
                id: winner.provider.clone(),
                models: vec![winner],
                env: Vec::new(),
                requires_key: false,
                login: None,
                rewrite: None,
            }
        });
        if gated {
            layer
                .replace(
                    replacements
                        .map(|p| ("package".into(), p, Some("not ready".into())))
                        .into(),
                )
                .unwrap();
        } else {
            for provider in replacements {
                models.register("host", provider).unwrap();
            }
        }
        models
            .reload(serde_json::from_value(config.clone()).unwrap(), false)
            .await
            .unwrap();
        models.refresh().await.unwrap();
        assert_eq!(request_counts(), [1, 1]);
        assert!(models.find("p/b").is_some());
        drop(layer);
        if !gated {
            assert!(models.unregister("local"));
            assert!(models.unregister("openai-codex"));
        }
        assert_eq!(models.find("local/old").as_ref(), Some(&old));
        assert_eq!(models.find("openai-codex/account").as_ref(), Some(&account));
        assert!(models.find("local/winner").is_none());
        assert!(models.find("local/new").is_none());
    }
    models.refresh().await.unwrap();
    assert_eq!(request_counts(), [2, 2]);
    assert!(models.find("local/new").is_some());
    assert!(models.find("local/old").is_none());
}

#[tokio::test]
async fn codex_models_come_from_the_account_list() {
    let dir = tempfile::tempdir().unwrap();
    let feed = Feed::start();
    let row = |slug: &str, visibility: &str| {
        json!({
            "slug": slug, "display_name": "GPT New", "context_window": 500000,
            "max_output_tokens": null, "input_modalities": ["text", "image"],
            "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}, {"effort": "ultra"}],
            "visibility": visibility
        })
    };
    let mut bare = row("gpt-bare", "list");
    bare["supported_reasoning_levels"] = json!([{"effort": "ultra"}]);
    let mut switchable = row("gpt-switchable", "list");
    switchable["supported_reasoning_levels"] = json!([{"effort": "none"}, {"effort": "low"}]);
    feed.set(
        "/codex/models",
        200,
        &json!({"models": [row("gpt-new", "list"), row("gpt-hidden", "hide"), bare, switchable]}),
    );
    let mut catalog = catalog_of(&["a"]);
    catalog["openai-codex"] = catalog_of(&["gpt-new", "impostor"])["p"].clone();
    catalog["openai"] = json!({"models": {"gpt-new": {
        "capabilities": {"reasoning": {"supported": true, "effort": ["none", "low", "high"]}},
        "cost": {"text": {"input": {"price": 3}, "output": {"price": 9}}}
    }}});
    feed.set("/api.json", 200, &catalog);
    let config = |url: &str| {
        json!({
            "refreshInterval": 0,
            "catalogUrl": format!("{url}/api.json"),
            "providers": {"openai-codex": {"baseUrl": url, "apiKey": "token"}}
        })
    };
    let models = load(dir.path(), config(&feed.url), true).await;

    let model = models.find("openai-codex/gpt-new").unwrap();
    assert_eq!(model.api, ApiKind::OpenAiCodexResponses);
    assert_eq!(
        models
            .all()
            .iter()
            .filter(|row| row.provider == "openai-codex" && row.id == "gpt-new")
            .count(),
        1
    );
    assert!(models.find("openai-codex/impostor").is_none());
    let cost = model.cost.as_ref().unwrap();
    assert_eq!((cost.input, cost.output), (3.0, 9.0));
    assert_eq!(
        supported_levels(&model),
        [ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High]
    );
    assert_eq!(model.context_window, 500_000);
    assert!(models.find("openai-codex/gpt-hidden").is_none());
    assert!(models.find("openai-codex/gpt-6-sol").is_none());
    let bare = models.find("openai-codex/gpt-bare").unwrap();
    assert!(!bare.reasoning);
    assert_eq!(supported_levels(&bare), [ThinkingLevel::Off]);
    let switchable = models.find("openai-codex/gpt-switchable").unwrap();
    assert_eq!(
        supported_levels(&switchable),
        [ThinkingLevel::Off, ThinkingLevel::Low]
    );
    assert_eq!(
        switchable.thinking_level_map.unwrap()[&ThinkingLevel::Off].as_deref(),
        Some("none")
    );
    let requests = feed.requests.lock().unwrap().clone();
    let listed = requests
        .iter()
        .find(|head| head.contains("/codex/models"))
        .unwrap();
    assert!(listed.contains("client_version=999.0.0"), "{listed}");

    feed.set("/codex/models", 500, &json!({}));
    models.refresh().await.unwrap();
    assert!(models.find("openai-codex/gpt-new").is_some());

    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let reopened = load(dir.path(), config(&closed), true).await;
    assert!(reopened.find("openai-codex/gpt-new").is_some());
}

#[tokio::test]
async fn built_in_windows_stop_before_expensive_context_unless_enabled() {
    let dir = tempfile::tempdir().unwrap();
    for (extended_context, expected) in [(false, 272_000), (true, 500_000)] {
        let models = Models::load(ModelsOptions {
            cache_dir: None,
            extended_context,
            config: ModelsConfig::default(),
            credentials: Arc::new(FileCredentialStore::new(dir.path().join("auth.json"))),
            http_client: None,
        })
        .await
        .unwrap();
        assert_eq!(
            models
                .find("openai-codex/gpt-6.1-sol")
                .unwrap()
                .context_window,
            expected,
        );
    }
}

#[tokio::test]
async fn explicit_windows_and_custom_models_bypass_price_caps() {
    let dir = tempfile::tempdir().unwrap();
    let config = ModelsConfig::parse_jsonc(
        r#"{
            "providers": {
                "openai-codex": {
                    "modelOverrides": {
                        "gpt-6.1-sol": { "contextWindow": 400000 }
                    },
                    "models": [
                        { "id": "gpt-6-sol", "contextWindow": 450000 },
                        {
                            "id": "custom",
                            "api": "openai-codex-responses",
                            "baseUrl": "https://example.invalid",
                            "contextWindow": 600000,
                            "cost": { "input": 1, "output": 2, "tiers": [{
                                "inputTokensAbove": 100000,
                                "input": 2, "output": 4,
                                "cacheRead": 0, "cacheWrite": 0
                            }] }
                        }
                    ]
                }
            }
        }"#,
    )
    .unwrap();
    let models = Models::load(ModelsOptions {
        cache_dir: None,
        extended_context: false,
        config,
        credentials: Arc::new(FileCredentialStore::new(dir.path().join("auth.json"))),
        http_client: None,
    })
    .await
    .unwrap();
    for (id, expected) in [
        ("openai-codex/gpt-6.1-sol", 400_000),
        ("openai-codex/gpt-6-sol", 450_000),
        ("openai-codex/custom", 600_000),
    ] {
        assert_eq!(models.find(id).unwrap().context_window, expected, "{id}");
    }
}

#[tokio::test]
async fn catalog_windows_respect_input_limits_and_lowest_price_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        ("smaller", json!({"context": 10000, "input": 8000}), 8000),
        ("larger", json!({"context": 10000, "input": 12000}), 10000),
        ("equal", json!({"context": 10000, "input": 10000}), 10000),
        ("zero-input", json!({"context": 10000, "input": 0}), 10000),
        ("missing-input", json!({"context": 10000}), 10000),
        ("zero-context", json!({"context": 0, "input": 2000}), 4096),
        ("missing-context", json!({"input": 2000}), 4096),
        ("missing-limits", json!({}), 4096),
    ];
    let row = |limit: serde_json::Value, cost: serde_json::Value| {
        json!({
            "type": "chat",
            "capabilities": {"tools": {"supported": true}},
            "recommended_api": "messages",
            "api": {"messages": {
                "base_url": "https://api.anthropic.com/v1",
                "sdk": {"aisdk": "@ai-sdk/anthropic"}
            }},
            "limit": limit,
            "cost": cost,
        })
    };
    let mut rows = serde_json::Map::new();
    for (id, limit, _) in &cases {
        rows.insert((*id).into(), row(limit.clone(), json!({})));
    }
    for (id, input) in [("tiered", 8000), ("below-tier", 1000)] {
        rows.insert(
            id.into(),
            row(
                json!({"context": 10000, "input": input}),
                json!({"text": {"input": {"price": 1, "overrides": [
                    {"when": {"context": {"gt": 6000}}, "price": 2},
                    {"when": {"context": {"gt": 2000}}, "price": 3}
                ]}}}),
            ),
        );
    }
    std::fs::write(
        dir.path().join("models.dev.v2.json"),
        serde_json::to_vec(&json!({"anthropic": {"models": rows}})).unwrap(),
    )
    .unwrap();
    for extended_context in [false, true] {
        let models = Models::load(ModelsOptions {
            cache_dir: Some(dir.path().to_path_buf()),
            extended_context,
            config: ModelsConfig::default(),
            credentials: Arc::new(FileCredentialStore::new(dir.path().join("auth.json"))),
            http_client: None,
        })
        .await
        .unwrap();
        for (id, _, expected) in &cases {
            assert_eq!(
                models
                    .find(&format!("anthropic/{id}"))
                    .unwrap()
                    .context_window,
                *expected,
                "{id}, extended_context={extended_context}",
            );
        }
        assert_eq!(
            models.find("anthropic/tiered").unwrap().context_window,
            if extended_context { 8000 } else { 2000 },
        );
        assert_eq!(
            models.find("anthropic/below-tier").unwrap().context_window,
            1000
        );
    }
}

#[tokio::test]
async fn command_api_keys_are_configuration_errors() {
    let dir = tempfile::tempdir().unwrap();
    let models = Models::load(ModelsOptions {
        cache_dir: None,
        extended_context: false,
        config: ModelsConfig::parse_jsonc(r#"{"providers":{"openai-codex":{"apiKey":"!echo k"}}}"#)
            .unwrap(),
        credentials: Arc::new(FileCredentialStore::new(dir.path().join("auth.json"))),
        http_client: None,
    })
    .await
    .unwrap();
    assert!(matches!(
        models.auth(&models.find("openai-codex/gpt-6-sol").unwrap()).await,
        Err(Error::Config(message)) if message.contains("command config values")
    ));
}

#[tokio::test]
async fn subscribe_and_scoped_views_isolate_and_propagate() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));
    let base = Models::load(ModelsOptions {
        cache_dir: None,
        extended_context: false,
        config: ModelsConfig::default(),
        credentials: credentials.clone(),
        http_client: None,
    })
    .await
    .unwrap();
    let scope_a = base.scoped();
    let scope_b = base.scoped();

    let mut sub_base = base.subscribe();
    let mut sub_a = scope_a.subscribe();
    let mut sub_b = scope_b.subscribe();
    assert_eq!(*sub_base.borrow(), 0);
    assert_eq!(*sub_a.borrow(), 0);
    assert_eq!(*sub_b.borrow(), 0);

    // Unchanged reload does not wake subscribers.
    base.reload(ModelsConfig::default(), false).await.unwrap();
    assert!(!sub_base.has_changed().unwrap());
    assert!(!sub_a.has_changed().unwrap());

    // Scoped layer in A is visible only in A and wakes only A.
    let layer_a = scope_a.provider_layer();
    let make_provider = |id: &str, model_id: &str| axo_ai::Provider {
        id: id.into(),
        models: vec![axo_ai::Model {
            id: model_id.into(),
            name: model_id.into(),
            api: axo_ai::ApiKind::OpenAiCompletions,
            provider: id.into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![axo_ai::InputKind::Text],
            cost: None,
            service_tiers: Default::default(),
            context_window: 8192,
            max_tokens: 1024,
            compat: Default::default(),
        }],
        env: Vec::new(),
        requires_key: true,
        login: None,
        rewrite: None,
    };
    layer_a
        .replace(vec![(
            "@scope/a/ext".into(),
            make_provider("acme", "a-model"),
            None,
        )])
        .unwrap();
    assert!(sub_a.has_changed().unwrap());
    let _ = *sub_a.borrow_and_update();
    assert!(!sub_base.has_changed().unwrap());
    assert!(!sub_b.has_changed().unwrap());
    assert!(scope_a.find("acme/a-model").is_some());
    assert!(base.find("acme/a-model").is_none());
    assert!(scope_b.find("acme/a-model").is_none());

    // Scope B can register the same provider id with a different model and credential slot.
    let layer_b = scope_b.provider_layer();
    layer_b
        .replace(vec![(
            "@scope/b/ext".into(),
            make_provider("acme", "b-model"),
            None,
        )])
        .unwrap();
    assert_eq!(
        scope_a.credential_slot("acme").unwrap(),
        "layer/@scope/a/ext/acme"
    );
    assert_eq!(
        scope_b.credential_slot("acme").unwrap(),
        "layer/@scope/b/ext/acme"
    );
    assert!(scope_b.find("acme/b-model").is_some());
    assert!(scope_a.find("acme/b-model").is_none());

    // Base registration propagates to base and both scopes.
    base.register("host", make_provider("shared", "s-model"))
        .unwrap();
    assert!(sub_base.has_changed().unwrap());
    assert!(sub_a.has_changed().unwrap());
    assert!(sub_b.has_changed().unwrap());
    assert!(scope_a.find("shared/s-model").is_some());
    assert!(scope_b.find("shared/s-model").is_some());

    // Dropping layer_a removes acme from A and bumps A's watch only.
    let _ = *sub_base.borrow_and_update();
    let _ = *sub_a.borrow_and_update();
    let _ = *sub_b.borrow_and_update();
    drop(layer_a);
    assert!(sub_a.has_changed().unwrap());
    assert!(!sub_base.has_changed().unwrap());
    assert!(!sub_b.has_changed().unwrap());
    assert!(scope_a.find("acme/a-model").is_none());
    assert!(scope_b.find("acme/b-model").is_some());

    // Scope priority is independent of base layer ordinals.
    let older_base = base.provider_layer();
    let local = scope_a.provider_layer();
    let newer_base = base.provider_layer();
    newer_base
        .replace(vec![(
            "host".into(),
            make_provider("ordinal", "base"),
            None,
        )])
        .unwrap();
    assert_eq!(older_base.hidden("ordinal"), None);
    assert_eq!(local.hidden("ordinal"), Some(axo_ai::ProviderOrigin::Layer));
    let mut provider = make_provider("ordinal", "local");
    provider.requires_key = false;
    local
        .replace(vec![("scope".into(), provider, None)])
        .unwrap();
    let _ = *sub_base.borrow_and_update();
    let _ = *sub_a.borrow_and_update();
    let _ = *sub_b.borrow_and_update();
    let snapshot = scope_a.all();
    let model = scope_a.find("ordinal/local").unwrap();
    let mut missing = model.clone();
    missing.id = "missing".into();
    let check_snapshot = || {
        assert!(Arc::ptr_eq(&snapshot, &scope_a.all()));
        assert!(!sub_base.has_changed().unwrap());
        assert!(!sub_a.has_changed().unwrap());
        assert!(!sub_b.has_changed().unwrap());
    };
    assert!(local.set_unavailable("ordinal", Some("offline".into())));
    check_snapshot();
    assert!(
        matches!(scope_a.auth(&missing).await, Err(Error::UnknownModel { provider, model }) if provider == "ordinal" && model == "missing")
    );
    assert!(
        matches!(scope_a.auth(&model).await, Err(Error::ProviderUnavailable { provider, reason }) if provider == "ordinal" && reason == "offline")
    );
    assert!(base.find("ordinal/base").is_some());
    assert!(local.set_unavailable("ordinal", None));
    check_snapshot();
    assert!(scope_a.auth(&model).await.is_ok());
}

#[tokio::test]
async fn logout_removes_credential_and_clears_codex_cache() {
    use axo_ai::CredentialStore;
    let dir = tempfile::tempdir().unwrap();
    let credentials = Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));
    tokio::fs::write(
        dir.path().join("codex-models.json"),
        serde_json::to_vec(&serde_json::json!([
            {
                "id": "account-only",
                "name": "Account Only",
                "contextWindow": 8192,
                "image": false,
                "efforts": []
            }
        ]))
        .unwrap(),
    )
    .await
    .unwrap();

    let models = Models::load(ModelsOptions {
        cache_dir: Some(dir.path().to_path_buf()),
        extended_context: false,
        config: ModelsConfig::parse_jsonc(r#"{"refreshInterval":0}"#).unwrap(),
        credentials: credentials.clone(),
        http_client: None,
    })
    .await
    .unwrap();
    let mut sub = models.subscribe();
    let slot = models.credential_slot("openai-codex").unwrap();
    credentials
        .set(
            &slot,
            axo_ai::Credential::ApiKey {
                key: "secret".into(),
            },
        )
        .await
        .unwrap();
    assert!(models.find("openai-codex/account-only").is_some());
    // Unknown Codex account model has unknown cost (None), distinct from $0.
    assert_eq!(models.find("openai-codex/account-only").unwrap().cost, None);

    // Unknown provider fails with Error::Config.
    assert!(matches!(
        models.logout("no-such-provider").await,
        Err(Error::Config(_))
    ));

    // Logout removes credential, deletes codex-models.json, rebuilds from floor, and bumps S.
    models.logout("openai-codex").await.unwrap();
    assert!(credentials.get(&slot).await.unwrap().is_none());
    assert!(!dir.path().join("codex-models.json").exists());
    assert!(models.find("openai-codex/account-only").is_none());
    assert!(models.find("openai-codex/gpt-6-sol").is_some());
    assert!(sub.has_changed().unwrap());

    // Second logout with no stored credential is a no-op Ok(()).
    let _ = *sub.borrow_and_update();
    models.logout("openai-codex").await.unwrap();
    assert!(!sub.has_changed().unwrap());
}

#[tokio::test]
async fn logout_reports_cache_removal_failure() {
    use axo_ai::CredentialStore;
    let dir = tempfile::tempdir().unwrap();
    // A directory in the cache file's place makes its deletion fail.
    std::fs::create_dir(dir.path().join("codex-models.json")).unwrap();
    let credentials = Arc::new(FileCredentialStore::new(dir.path().join("auth.json")));
    let models = Models::load(ModelsOptions {
        cache_dir: Some(dir.path().to_path_buf()),
        extended_context: false,
        config: ModelsConfig::parse_jsonc(r#"{"refreshInterval":0}"#).unwrap(),
        credentials: credentials.clone(),
        http_client: None,
    })
    .await
    .unwrap();
    let slot = models.credential_slot("openai-codex").unwrap();
    credentials
        .set(
            &slot,
            axo_ai::Credential::ApiKey {
                key: "secret".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        models.logout("openai-codex").await,
        Err(Error::Io(_))
    ));
    assert!(credentials.get(&slot).await.unwrap().is_none());
}

#[tokio::test]
async fn unknown_vs_free_cost_in_catalog_and_usage() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = json!({
        "p": {
            "models": {
                "unpriced": dev_row("chat-completions", "@ai-sdk/openai-compatible", json!({})),
                "free": dev_row(
                    "chat-completions",
                    "@ai-sdk/openai-compatible",
                    json!({"cost": {"text": {"input": {"price": 0.0}, "output": {"price": 0.0}}}})
                ),
            }
        }
    });
    tokio::fs::write(
        dir.path().join("models.dev.v2.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .await
    .unwrap();
    let models = load(dir.path(), json!({"refreshInterval": 0}), false).await;
    assert_eq!(models.find("p/unpriced").unwrap().cost, None);
    assert_eq!(
        models.find("p/free").unwrap().cost,
        Some(axo_ai::Cost::default())
    );

    // A cost patch on an unpriced model must name both prices; cache prices default to 0.
    let patched = |cost: Value| {
        serde_json::from_value(json!({
            "refreshInterval": 0,
            "providers": {"p": {"modelOverrides": {"unpriced": {"cost": cost}}}}
        }))
        .unwrap()
    };
    let options = |config| ModelsOptions {
        cache_dir: Some(dir.path().to_path_buf()),
        extended_context: false,
        config,
        credentials: Arc::new(FileCredentialStore::new(dir.path().join("auth.json"))),
        http_client: None,
    };
    match Models::load(options(patched(json!({"input": 3})))).await {
        Err(Error::Config(message)) => assert!(message.contains("needs both"), "{message}"),
        other => panic!("expected a config error, got {:?}", other.err()),
    }
    let invalid = serde_json::from_value(json!({
        "refreshInterval": 0,
        "providers": {"p": {"modelOverrides": {"unpriced": {"serviceTiers": {"standard": null}}}}}
    }))
    .unwrap();
    assert!(matches!(Models::load(options(invalid)).await,
        Err(Error::Config(message)) if message == r#"serviceTiers: "standard" has no multiplier"#
    ));
    let priced = Models::load(options(patched(json!({"input": 3, "output": 9}))))
        .await
        .unwrap();
    assert_eq!(
        priced.find("p/unpriced").unwrap().cost,
        Some(axo_ai::Cost {
            input: 3.0,
            output: 9.0,
            cache_read: 0.0,
            cache_write: 0.0,
            tiers: vec![],
        })
    );

    let mut total = axo_ai::Usage::default();
    let priced = axo_ai::Usage {
        input: 10,
        output: 5,
        total_tokens: 15,
        cost: Some(axo_ai::UsageCost {
            input: 0.01,
            output: 0.02,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.03,
        }),
        ..Default::default()
    };
    total += &priced;
    assert_eq!(total.cost.unwrap().total, 0.03);
    let unpriced = axo_ai::Usage {
        input: 10,
        output: 5,
        total_tokens: 15,
        cost: None,
        ..Default::default()
    };
    total += &unpriced;
    assert_eq!(total.cost, None);
}
