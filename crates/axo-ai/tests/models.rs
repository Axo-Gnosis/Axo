use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use axo_ai::thinking::supported_levels;
use axo_ai::{
    ApiKind, Error, FileCredentialStore, Models, ModelsConfig, ModelsOptions, ThinkingLevel,
    TranscriptToolSupport,
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

    let cost = models.find("p/priced").unwrap().cost;
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
    feed.set(
        "/codex/models",
        200,
        &json!({"models": [row("gpt-new", "list"), row("gpt-hidden", "hide"), bare]}),
    );
    let mut catalog = catalog_of(&["a"]);
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
    assert_eq!((model.cost.input, model.cost.output), (3.0, 9.0));
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
                            "cost": { "tiers": [{
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
