//! Helpers shared by the session-level tests.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axo::{Axo, ProjectAccess, SessionOptions, Settings, StaticEnv};
use axo_ai::{FileCredentialStore, Models, ModelsConfig, ModelsOptions, ScriptedApi, ScriptedTurn};

const TIMEOUT: Duration = Duration::from_secs(5);

/// An `Axo` whose sessions stream from `turns`, with one model `test/m`.
pub async fn axo(dir: &Path, turns: Vec<ScriptedTurn>) -> Axo {
    let config = ModelsConfig::parse_jsonc(
        r#"{ "providers": { "test": { "api": "openai-completions",
             "baseUrl": "http://127.0.0.1:9/v1", "models": [{ "id": "m" }] } } }"#,
    )
    .unwrap();
    let models = Models::load(ModelsOptions {
        cache_dir: None,
        config,
        extended_context: false,
        credentials: Arc::new(FileCredentialStore::new(dir.join("auth.json"))),
    })
    .await
    .unwrap();
    Axo::builder()
        .dir(dir.to_path_buf())
        .settings(Settings::default())
        .models(Arc::new(models))
        .streamer(Arc::new(ScriptedApi::new(turns)))
        .build()
        .await
        .unwrap()
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
