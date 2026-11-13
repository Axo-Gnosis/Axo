#![allow(dead_code)]
//! Helpers shared by the session-level tests.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axo::{
    Axo, ConfigLayer, ConfigSource, Error, ProjectAccess, Session, SessionEvent, SessionOptions,
    SpawnRequest, StaticEnv, UserConfig,
};
use axo::{Entry, JsonlStore, SeqEntry, SessionStore};
use axo_agent::{AgentMessage, Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::testing::reply_text;
use axo_ai::{
    FileCredentialStore, Message, Models, ModelsConfig, ModelsOptions, ProviderFailure,
    ScriptedApi, ScriptedTurn, StopReason, Streamer, UserBlock,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;

const TIMEOUT: Duration = Duration::from_secs(5);

pub fn transient(delay: Option<u64>) -> ScriptedTurn {
    let mut message = reply_text("");
    message.stop_reason = StopReason::Error;
    message.error_message = Some("503 service unavailable".into());
    message.failure = Some(ProviderFailure {
        status: Some(503),
        code: None,
        retry_after_ms: delay,
        should_retry: None,
    });
    ScriptedTurn::Reply(message)
}

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
                    skill_roots: vec![dir.join("skills")],
                    ..Default::default()
                },
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
    let api = Arc::new(ScriptedApi::new(turns));
    let (axo, models, config) = axo_with(
        dir,
        models,
        settings,
        Arc::new(JsonlStore::new(dir.join("sessions"))),
        api.clone(),
    )
    .await;
    Host {
        axo,
        api,
        models,
        config,
    }
}

/// An `Axo` over `store` streaming from `streamer`, with provider `test` serving `models`,
/// [`TestConfig::for_dir`] and `<dir>/cache` as its cache directory.
pub async fn axo_with(
    dir: &Path,
    models: serde_json::Value,
    settings: serde_json::Value,
    store: Arc<dyn SessionStore>,
    streamer: Arc<dyn Streamer>,
) -> (Axo, Arc<Models>, Arc<TestConfig>) {
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
            http_client: None,
            cache_dir: None,
            config,
            extended_context: false,
            credentials: Arc::new(FileCredentialStore::new(dir.join("auth.json"))),
        })
        .await
        .unwrap(),
    );
    let config = Arc::new(TestConfig::for_dir(dir, settings));
    let axo = Axo::builder(models.clone(), store)
        .config(config.clone())
        .cache_dir(dir.join("cache"))
        .streamer(streamer)
        .build();
    (axo, models, config)
}

/// The session file's entries as raw JSON, header excluded.
pub fn journal(session: &Session) -> Vec<Value> {
    std::fs::read_to_string(session.path())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|value| value["type"] != "session")
        .collect()
}

/// The index of the only entry matching `pred`.
pub fn only(lines: &[Value], what: &str, pred: impl Fn(&Value) -> bool) -> usize {
    let found: Vec<usize> = (0..lines.len()).filter(|&i| pred(&lines[i])).collect();
    assert_eq!(found.len(), 1, "expected exactly one {what}: {lines:#?}");
    found[0]
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
        .filter_map(UserBlock::as_text)
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

/// Polls `ready` until it holds.
pub async fn until(mut ready: impl FnMut() -> bool) {
    within(async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

/// Waits for an entry matching `matches`, already written or yet to come.
pub async fn entry(session: &Session, matches: impl Fn(&Entry) -> bool) {
    let mut attached = session.attach(None).await;
    if attached.missed.iter().any(|seq| matches(&seq.entry)) {
        return;
    }
    within(async {
        loop {
            match attached.events.recv().await {
                Ok(SessionEvent::Entry(seq)) if matches(&seq.entry) => break,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("session closed"),
            }
        }
    })
    .await;
}

/// Waits until a message delivers `job`'s result.
pub async fn delivered(session: &Session, job: &str) {
    entry(session, |entry| {
        matches!(entry, Entry::Message { delivered_jobs, .. } if delivered_jobs.iter().any(|id| id == job))
    })
    .await;
}

/// A spawn of `task` on the `test/child` model.
pub fn request(task: &str) -> SpawnRequest {
    SpawnRequest {
        task: task.into(),
        model: Some("test/child".into()),
        ..Default::default()
    }
}

/// The first text block of a raw `message` entry.
pub fn message_text(line: &Value) -> &str {
    line["message"]["content"][0]["text"].as_str().unwrap()
}

/// The number of `run_end` entries in the session's journal.
pub fn run_ends(session: &Session) -> usize {
    journal(session)
        .iter()
        .filter(|line| line["type"] == "run_end")
        .count()
}

/// Runs until its call is cancelled.
pub struct Hang;

#[async_trait]
impl Tool for Hang {
    fn name(&self) -> &str {
        "hang"
    }

    fn description(&self) -> &str {
        "Waits until cancelled."
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> =
            LazyLock::new(|| json!({"type": "object", "properties": {}}));
        &PARAMETERS
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        cx.cancel.cancelled().await;
        Err(ToolError("Operation aborted".into()))
    }
}
