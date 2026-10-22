use std::sync::Arc;

use axo_ai::{FileCredentialStore, Models, ModelsConfig, ModelsOptions};
use serde_json::json;

#[tokio::test]
async fn built_in_windows_stop_before_expensive_context_unless_enabled() {
    let dir = tempfile::tempdir().unwrap();
    for (extended_context, expected) in [(false, 272_000), (true, 500_000)] {
        let models = Models::load(ModelsOptions {
            cache_dir: None,
            extended_context,
            config: ModelsConfig::default(),
            credentials: Arc::new(FileCredentialStore::new(dir.path().join("auth.json"))),
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
        ("missing-limits", json!(null), 4096),
    ];
    let mut rows = serde_json::Map::new();
    for (id, limit, _) in &cases {
        rows.insert((*id).into(), json!({"tool_call": true, "limit": limit}));
    }
    for (id, input) in [("tiered", 8000), ("below-tier", 1000)] {
        rows.insert(
            id.into(),
            json!({
                "tool_call": true,
                "limit": {"context": 10000, "input": input},
                "cost": {"tiers": [
                    {"tier": {"type": "context", "size": 6000}},
                    {"tier": {"type": "context", "size": 2000}}
                ]}
            }),
        );
    }
    std::fs::write(
        dir.path().join("models.dev.json"),
        serde_json::to_vec(&json!({"anthropic": {"models": rows}})).unwrap(),
    )
    .unwrap();
    for extended_context in [false, true] {
        let models = Models::load(ModelsOptions {
            cache_dir: Some(dir.path().to_path_buf()),
            extended_context,
            config: ModelsConfig::default(),
            credentials: Arc::new(FileCredentialStore::new(dir.path().join("auth.json"))),
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
