#![allow(dead_code)]
//! Helpers shared by the session-level tests.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axo::store::{Entry, JsonlStore, SeqEntry};
use axo::{
    Axo, ConfigLayer, ConfigSource, Error, ProjectAccess, SessionOptions, StaticEnv, UserConfig,
};
use axo_agent::AgentMessage;
use axo_ai::{
    FileCredentialStore, Message, Models, ModelsConfig, ModelsOptions, ScriptedApi, ScriptedTurn,
    UserBlock,
};
use serde_json::json;

const TIMEOUT: Duration = Duration::from_secs(5);

/// An in-memory [`ConfigSource`] that logs every call (`"user"`, `"project:<cwd>"`).
/// Acquiring a project for an unregistered cwd panics.
#[derive(Default)]
pub struct TestConfig {
    pub user: Mutex<UserConfig>,
    pub projects: Mutex<HashMap<PathBuf, ConfigLayer>>,
    pub calls: Mutex<Vec<String>>,
}

impl ConfigSource for TestConfig {
    fn user(&self) -> Result<UserConfig, Error> {
        self.calls.lock().unwrap().push("user".to_owned());
        Ok(self.user.lock().unwrap().clone())
    }

    fn project(&self, cwd: &Path) -> Result<ConfigLayer, Error> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("project:{}", cwd.display()));
        Ok(self
            .projects
            .lock()
            .unwrap()
            .get(cwd)
            .unwrap_or_else(|| panic!("project config acquired for {}", cwd.display()))
            .clone())
    }
}

impl TestConfig {
    /// The user layer `settings` (origin `"user"`) with `<dir>/skills` and
    /// `<dir>/AGENTS.md` as user resources, and no registered projects.
    pub fn for_dir(dir: &Path, settings: serde_json::Value) -> Self {
        Self {
            user: Mutex::new(UserConfig {
                layer: ConfigLayer {
                    origin: "user".into(),
                    settings: Some(settings),
                    ..Default::default()
                },
                skill_roots: vec![dir.join("skills")],
                context_files: vec![dir.join("AGENTS.md")],
            }),
            ..Default::default()
        }
    }

    /// Registers `layer` as the project contribution for sessions in `cwd`.
    pub fn set_project(&self, cwd: impl Into<PathBuf>, layer: ConfigLayer) {
        self.projects.lock().unwrap().insert(cwd.into(), layer);
    }

    /// The calls logged since the last take.
    pub fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

pub struct Host {
    pub axo: Axo,
    pub api: Arc<ScriptedApi>,
    pub models: Arc<Models>,
    pub config: Arc<TestConfig>,
}

/// A host over `dir` with [`TestConfig::for_dir`] and `<dir>/cache` as its cache
/// directory.
pub async fn host(
    dir: &Path,
    models: serde_json::Value,
    settings: serde_json::Value,
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
    let config = Arc::new(TestConfig::for_dir(dir, settings));
    let api = Arc::new(ScriptedApi::new(turns));
    let axo = Axo::builder(
        models.clone(),
        Arc::new(JsonlStore::new(dir.join("sessions"))),
    )
    .config(config.clone())
    .cache_dir(dir.join("cache"))
    .streamer(api.clone())
    .build();
    Host {
        axo,
        api,
        models,
        config,
    }
}

/// An `Axo` whose sessions stream from `turns`, with one model `test/m`.
pub async fn axo(dir: &Path, turns: Vec<ScriptedTurn>) -> Axo {
    host(dir, json!([{"id":"m"}]), json!({}), turns).await.axo
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
