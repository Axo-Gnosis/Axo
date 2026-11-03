#![allow(dead_code)]
//! Helpers shared by the session-level tests.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axo::store::{Entry, SeqEntry};
use axo::{Axo, ProjectAccess, SessionOptions, Settings, StaticEnv};
use axo_agent::AgentMessage;
use axo_ai::{
    FileCredentialStore, Message, Models, ModelsConfig, ModelsOptions, ScriptedApi, ScriptedTurn,
    UserBlock,
};
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

/// [`options`] with the real Nix-provided bash on `PATH`, keeping the isolated fixture
/// environment otherwise.
pub fn bash_options(dir: &Path) -> SessionOptions {
    let mut options = options(dir);
    options.exec_env = Some(Arc::new(StaticEnv(Arc::new(
        [(
            "PATH".into(),
            std::env::var_os("PATH").expect("test process needs PATH"),
        )]
        .into_iter()
        .collect(),
    ))));
    options
}

/// The text of the successful tool result for `call_id`.
pub fn tool_text(entries: &[SeqEntry], call_id: &str) -> String {
    let result = entries
        .iter()
        .find_map(|seq| match &seq.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } if result.tool_call_id == call_id => Some(result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing result for {call_id}"));
    assert!(!result.is_error, "{call_id}: {:?}", result.content);
    result
        .content
        .iter()
        .filter_map(|block| match block {
            UserBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Gone or a zombie awaiting its reaper.
pub fn dead(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        // The state follows the parenthesized command name, which may itself hold ")".
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

/// Awaits `future`, failing the test after five seconds.
pub async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .expect("timed out")
}
