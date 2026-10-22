//! Helpers shared by the session-level tests.

use std::path::Path;
use std::sync::Arc;

use axo::{Axo, Settings};
use axo_ai::{FileCredentialStore, Models, ModelsConfig, ModelsOptions, ScriptedApi, ScriptedTurn};

mod session;

pub use session::{options, within};

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
