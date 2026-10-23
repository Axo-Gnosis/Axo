#![allow(dead_code)]
//! Helpers shared by the session-level tests.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axo::{Axo, ProjectAccess, SessionOptions, Settings, StaticEnv};
use axo_ai::{FileCredentialStore, Models, ModelsConfig, ModelsOptions, ScriptedApi, ScriptedTurn};
use serde_json::json;

const TIMEOUT: Duration = Duration::from_secs(5);

pub struct Host {
    pub axo: Axo,
    pub api: Arc<ScriptedApi>,
    pub models: Arc<Models>,
}

pub async fn host(
    dir: &Path,
    models: serde_json::Value,
    settings: Settings,
    turns: Vec<ScriptedTurn>,
) -> Host {
    let config = ModelsConfig::parse_jsonc(
        &json!({"providers": {"test": {
            "api": "openai-completions",
            "baseUrl": "http://127.0.0.1:9/v1",
            "models": models,
        }}})
        .to_string(),
    )
    .unwrap();
    let models = Arc::new(
        Models::load(ModelsOptions {
            cache_dir: None,
            config,
            extended_context: false,
            credentials: Arc::new(FileCredentialStore::new(dir.join("auth.json"))),
        })
        .await
        .unwrap(),
    );
    let api = Arc::new(ScriptedApi::new(turns));
    let axo = Axo::builder()
        .dir(dir.to_path_buf())
        .settings(settings)
        .models(models.clone())
        .streamer(api.clone())
        .build()
        .await
        .unwrap();
    Host { axo, api, models }
}

/// An `Axo` whose sessions stream from `turns`, with one model `test/m`.
pub async fn axo(dir: &Path, turns: Vec<ScriptedTurn>) -> Axo {
    host(dir, json!([{"id":"m"}]), Settings::default(), turns)
        .await
        .axo
}

/// Options for a session in `<dir>/work` on `test/m`, without project resources or a
/// dev shell.
pub fn options(dir: &Path) -> SessionOptions {
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    SessionOptions {
        cwd,
        model: Some("test/m".into()),
        project_access: Some(ProjectAccess::Ignore),
        exec_env: Some(Arc::new(StaticEnv(Arc::new(BTreeMap::new())))),
        ..SessionOptions::default()
    }
}

/// Awaits `future`, failing the test after five seconds.
pub async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .expect("timed out")
}
