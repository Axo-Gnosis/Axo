//! A conversation with one agent in one working directory, saved as it happens.

pub(crate) mod children;
pub(crate) mod coordination;
pub(crate) mod delivery;
pub(crate) mod events;
pub(crate) mod fork;
pub(crate) mod journal;
pub(crate) mod maintenance;
pub(crate) mod modes;
mod policy;
pub(crate) mod prompt;
mod prompt_cache;
pub(crate) mod registrations;
mod resume;
mod run;
pub(crate) mod selection;
pub(crate) mod spawn;
mod start;
mod surface;
pub(crate) mod team;
pub(crate) mod wire;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use axo_agent::{Agent, AgentMessage, CancelReason, QueuedMessage, ToolHooks};
use axo_ai::{Message, Model, Models, ThinkingLevel, Usage};
use axo_macros::expose;
use serde_json::Value;
use tokio::sync::{broadcast, watch};

use self::children::{ChildState, SessionTable};
use self::delivery::ActivityState;
pub use self::delivery::{
    Activity, HostDelivery, HostMessage, PromptRequest, PromptResult, QueuedInput,
};
pub use self::events::{
    Attach, ContextUsage, LiveSnapshot, ResultResolution, SessionEvent, SessionEventKind,
};
use self::journal::Journal;
use self::maintenance::Maintenance;
pub(crate) use self::maintenance::normalize_focus;
pub use self::maintenance::{MaintenanceKind, NavigateOptions, ShakeMode};
pub(crate) use self::modes::MODE_NOTICE;
pub use self::modes::{AcceptResultOptions, PendingResult};
use self::policy::ScopeCell;
pub use self::policy::{ToolPolicy, ToolScope};
pub use self::prompt::{ContextFilesInspection, PromptInspection};
use self::prompt::{SessionPrompt, ShapeBase};
pub(crate) use self::registrations::Layers;
use self::run::{LiveRun, RunRequest, run_to_settle};
use self::selection::Selection;
pub use self::spawn::{
    ChildHandle, InlineSpec, SpawnContext, SpawnDecision, SpawnHook, SpawnOrigin, SpawnRequest,
    SpawnTeam, TeamHandle, TeamMemberRequest, TeamRequest,
};
pub(crate) use self::start::{Inherited, SessionSetup};
pub(crate) use self::surface::TOOL_CATALOG;
pub use self::{fork::Fork, registrations::Registrations};
use crate::commands::{self, Command, CommandInfo, RegisteredCommand};
use crate::compaction::{self, Limits};
use crate::config::{EffectiveConfig, ProjectAccess};
use crate::context::agents::{AgentDef, AgentSpec, RunAs};
use crate::context::definitions::LiveDefinitions;
use crate::context::keywords::{KeywordMatch, KeywordSpec};
use crate::context::modes::ModeSpec;
use crate::context::skills::Skill;
use crate::context::{Diagnostic, DiagnosticOrigin};
use crate::error::Error;
use crate::exec::ExecEnv;
use crate::jobs::{JobHost, JobId, JobInfo, JobRegistry};
use crate::schemes::agent::AgentScheme;
use crate::schemes::artifact::Artifacts;
use crate::schemes::dir::{DirScheme, ensure_bundled_docs};
use crate::schemes::proc::ProcScheme;
use crate::schemes::skill::SkillScheme;
use crate::schemes::{Scheme, SchemeContext, Schemes};
use crate::store::{self, Entry, Projection, SeqEntry, SessionStore};
use crate::tools::{self, Answer, Exposure, PendingQuestions, SessionTools};

pub struct SessionOptions {
    /// Ignored by `Axo::open_session`, which uses the cwd in the session header.
    pub cwd: PathBuf,
    /// A model selector (`provider/model`, `@role`, or thinking and tier suffixes alone);
    /// falls back to the session's last model, then the root agent's `model`, then `defaultModel`.
    pub model: Option<String>,
    /// Overrides the host's model registry (and default streamer) for this session, its forks,
    /// and its spawned children. `None` uses `Axo::models()`.
    pub models: Option<Arc<Models>>,
    /// The root agent by name. `None` keeps the session's recorded agent (none for a new
    /// session); `Some` selects it, recording a change on reopen.
    pub agent: Option<String>,
    /// `false` loads no keywords, so nothing in a prompt fires one. For host-driven sessions
    /// whose prompts are not typed by the user. Forks inherit it.
    pub keywords_enabled: bool,
    /// Falls back to the session's last level, then `defaultThinkingLevel`; clamped to
    /// the model's supported levels.
    pub thinking: Option<ThinkingLevel>,
    /// Register the `ask` tool.
    pub ask_enabled: bool,
    /// Forces the project's access level; `None` takes it from `project.paths`/`project.access`
    /// in user settings.
    pub project_access: Option<ProjectAccess>,
    /// Overrides the environment provider built from settings.
    pub exec_env: Option<Arc<dyn ExecEnv>>,
    /// Host registrations. Definitions rank files → layer → host; other lists apply host → layer.
    pub host: Registrations,
    /// A second registration set, replaced live by `Session::set_layer` for a composer such
    /// as an extension manager.
    pub layer: Registrations,
    /// What `Axo::open_session` does with a run left unfinished.
    pub recovery: RecoveryPolicy,
}

impl Default for SessionOptions {
    /// `cwd` is the process's current directory (empty if it cannot be read).
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_default(),
            model: None,
            models: None,
            agent: None,
            keywords_enabled: true,
            thinking: None,
            ask_enabled: false,
            project_access: None,
            exec_env: None,
            host: Registrations::default(),
            layer: Registrations::default(),
            recovery: RecoveryPolicy::default(),
        }
    }
}

/// What opening a session does with a run a crash left unfinished.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RecoveryPolicy {
    /// Record interrupted tool results, then continue the run in the background.
    #[default]
    Resume,
    /// Record interrupted tool results and close the run as `interrupted`.
    MarkInterrupted,
}

/// One tool as [`Session::tools`] lists it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolInfo {
    pub name: String,
    /// The description's first non-blank line, as the deferred catalog shows it.
    pub summary: String,
    pub description: String,
    pub parameters: Value,
    pub exposure: Exposure,
    /// A deferred tool the model has loaded into its tool list.
    pub promoted: bool,
    /// Whether the effective mode's tool limits permit calling it.
    pub allowed_in_mode: bool,
    /// Who registered it (`HostTool::origin`); `None` for built-ins and unlabeled host tools.
    pub origin: Option<String>,
}

pub struct Session {
    agent: Arc<Agent>,
    hooks: Arc<SessionHooks>,
    inherited: Arc<Inherited>,
    diagnostics: Vec<Diagnostic>,
}

/// The session's loop hooks, journal and event relay.
struct SessionHooks {
    events: broadcast::Sender<Arc<SessionEvent>>,
    inherited: Arc<Inherited>,
    child: Option<ChildState>,
    /// The header id and lazily created session file.
    session_id: String,
    path: PathBuf,
    /// Shared tool state, restored as the selected branch changes.
    tools: Arc<SessionTools>,
    questions: Arc<PendingQuestions>,
    results: modes::PendingResults,
    shutdown: AtomicBool,
    /// Owns the session file: every entry is appended through it, and every reader takes
    /// its snapshots.
    journal: Journal,
    /// The open run's live state; `persist` updates it without awaiting.
    live: Mutex<LiveRun>,
    /// The live model, thinking level and chosen service tier.
    selection: watch::Sender<Selection>,
    /// Who owns the session; `shutdown` waits on it going idle.
    activity: ActivityState,
    /// Held across a steer or follow-up's write, so the agent queues them in journal order.
    queue_order: tokio::sync::Mutex<()>,
    /// Durable host messages held until a run's next delivery boundary.
    quiet: Mutex<Vec<QueuedMessage>>,
    /// Held across a selection change's write and its apply, so changes take effect in
    /// journal order and only once durable.
    selection_order: tokio::sync::Mutex<()>,
    /// The registry's definition of the selected model when it was last applied; a different
    /// one now means the registry changed. `None` when the registry does not have the model.
    registry_model: Mutex<Option<Arc<Model>>>,
    maintenance: Maintenance,
    /// Prompt-cache clock and warm refusal.
    cache: prompt_cache::PromptCache,
    /// Prompts between their fast-path admission and their run start; idle-wake delivery
    /// holds off meanwhile, so pending results ride the prompt instead of making it busy.
    preparing: AtomicUsize,
    prompt: SessionPrompt,
    definitions: Arc<LiveDefinitions>,
    shape_base: Mutex<ShapeBase>,
    promoted: Mutex<Vec<String>>,
    mode: Mutex<modes::ModeState>,
    agent_spec: Mutex<Option<AgentSpec>>,
    scope: Arc<ScopeCell>,
    /// Holds queued input and finished job results until the user's next input or
    /// `resume_wakes`. A Stop latches when pressed; a run that fails latches when it settles.
    auto_paused: AtomicBool,
    /// Set by a Stop while the session is claimed; the next run start honors it, so a Stop
    /// before the run begins is not lost.
    stop_requested: AtomicBool,
    /// Set once in `Session::start`; weak, since the agent holds these hooks.
    agent: OnceLock<Weak<Agent>>,
    /// These hooks, for tasks that deliver job results.
    this: Weak<SessionHooks>,
    /// Σ `Entry::usage` over this session's journal; seeded at start, grown by `after_append`.
    own_usage: Mutex<Usage>,
    /// The tree total last emitted as `TreeUsage` (and pushed to the parent's row); held
    /// while computing and pushing, so an older total never lands after a newer one.
    last_tree: Mutex<Usage>,
    /// The live host and layer tool policies, installed on the agent after `fixed_hooks`.
    policies: Mutex<Vec<Arc<dyn ToolPolicy>>>,
    /// The agent hooks ahead of the policies: the child budget (children only), then `ModeGate`.
    fixed_hooks: Vec<Arc<dyn ToolHooks>>,
    /// The live host and layer spawn hooks.
    spawn_hooks: Mutex<Vec<Arc<dyn SpawnHook>>>,
    /// The live host and layer schemes, layered over the built-ins in `tools.schemes`.
    host_schemes: Mutex<Vec<Arc<dyn Scheme>>>,
    /// The live host and layer registered commands; always empty for children.
    commands: Mutex<Vec<RegisteredCommand>>,
    /// The host's registrations and the layer, kept apart so either can be replaced; the
    /// cells above hold their joined lists.
    registrations: Mutex<Layers>,
}

/// Starts `work` on its own task now and returns its result, so dropping the caller's
/// future never cuts a state change off between its journal write and what follows.
fn detached<T: Send + 'static>(
    work: impl Future<Output = T> + Send + 'static,
) -> impl Future<Output = T> {
    let task = tokio::spawn(work);
    async move {
        match task.await {
            Ok(value) => value,
            Err(join) => std::panic::resume_unwind(join.into_panic()),
        }
    }
}

/// Σ `Entry::usage` over `entries`: maintenance and failed requests included, run and job
/// rollups not.
fn entries_usage<'a>(entries: impl IntoIterator<Item = &'a SeqEntry>) -> Usage {
    let mut total = Usage::default();
    for usage in entries.into_iter().filter_map(|seq| seq.entry.usage()) {
        total += usage;
    }
    total
}

impl SessionHooks {
    /// The parent's registry when this session is a child whose parent is live.
    fn parent_registry(&self) -> Option<Arc<JobRegistry>> {
        let child = self.child.as_ref()?;
        self.inherited
            .sessions
            .registry(&child.link.parent_session_id)
    }

    /// Own usage plus every child row's tree total.
    fn tree_usage(&self) -> Usage {
        let mut usage = self
            .own_usage
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        usage += &self.tools.runner.jobs.children_tree_usage();
        usage
    }

    /// Appends `entries` as one journal batch (see [`Journal::append`]), queued before
    /// this returns.
    fn append(
        &self,
        entries: Vec<Entry>,
    ) -> impl Future<Output = Result<Vec<SeqEntry>, Error>> + Send + '_ {
        let queued = self.check_writable().map(|()| self.journal.append(entries));
        async move {
            let appended = queued?.await?;
            self.after_append(&appended);
            Ok(appended)
        }
    }

    /// [`SessionHooks::append`], only if the journal is still at `revision`.
    fn append_if(
        &self,
        revision: u64,
        entries: Vec<Entry>,
    ) -> impl Future<Output = Result<Vec<SeqEntry>, Error>> + Send + '_ {
        let queued = self
            .check_writable()
            .map(|()| self.journal.append_if(revision, entries));
        async move {
            let appended = queued?.await?;
            self.after_append(&appended);
            Ok(appended)
        }
    }

    /// Re-derives the file ledger and context tracker once a batch moved or rewrote the
    /// transcript, and adds the batch's recorded usage to the session's own.
    fn after_append(&self, appended: &[SeqEntry]) {
        if appended.iter().any(|seq| seq.entry.rewrites_context()) {
            let view = self.journal.view();
            self.tools.ledger.restore(&view.projection.messages);
            self.tools.context.restore(&view.projection.messages);
        }
        if appended.iter().any(|seq| {
            seq.entry.rewrites_context()
                || matches!(
                    &seq.entry,
                    Entry::Message {
                        message: AgentMessage::Llm(Message::Assistant(_)),
                        ..
                    }
                )
        }) {
            self.emit_context();
        }
        let mut changed = false;
        {
            let mut own = self
                .own_usage
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for usage in appended.iter().filter_map(|seq| seq.entry.usage()) {
                *own += usage;
                changed = true;
            }
        }
        if changed {
            JobHost::tree_changed(self);
        }
    }

    fn context_usage(&self) -> Option<ContextUsage> {
        self.context_usage_for(&self.journal.view().projection)
    }

    fn context_usage_for(&self, projection: &Projection) -> Option<ContextUsage> {
        let limits = Limits::resolve(&self.model(), &self.maintenance.settings).ok()?;
        Some(ContextUsage {
            tokens: compaction::estimate(projection),
            window: limits.window,
            trigger: limits.trigger,
            reserve: limits.reserve,
        })
    }

    fn emit_context(&self) {
        self.journal.read(|view| {
            if let Some(usage) = self.context_usage_for(&view.projection) {
                let _ = self.events.send(Arc::new(SessionEvent::Context(usage)));
            }
        });
    }

    fn set_auto_paused(&self, paused: bool) {
        let _live = self.live();
        if self.auto_paused.swap(paused, Ordering::AcqRel) != paused {
            let _ = self
                .events
                .send(Arc::new(SessionEvent::AutoPaused { paused }));
            if let Some(team) = self.child.as_ref().and_then(|child| child.team.as_ref()) {
                team.bump();
            }
        }
    }
}

/// Session side folders, relative to `<session_dir>` (the session file path without `.jsonl`).
const ARTIFACTS_DIR: &str = "artifacts";
const LOCAL_DIR: &str = "local";

/// Where `send` routes parent and teammate messages from a session.
#[derive(Clone)]
pub(crate) struct AgentRoutes {
    pub(crate) session_id: String,
    /// `None` for root sessions and forks.
    pub(crate) parent: Option<String>,
    pub(crate) team: Option<crate::store::TeamLink>,
}

pub(crate) struct LiveSchemes {
    pub(crate) jobs: Arc<JobRegistry>,
    pub(crate) sessions: Arc<SessionTable>,
    pub(crate) store: Arc<dyn SessionStore>,
    pub(crate) deferred: Arc<tools::deferred::Deferred>,
}

/// Builds the session's file-backed schemes, plus the process, agent and tool schemes
/// when live state is supplied. Children reuse their parent's docs flag.
pub(crate) fn session_schemes(
    docs_dir: Option<&Path>,
    name: &str,
    cx: SchemeContext,
    host: &[Arc<dyn Scheme>],
    live: Option<LiveSchemes>,
    definitions: Arc<LiveDefinitions>,
    docs: Option<bool>,
) -> Result<(Schemes, Vec<Diagnostic>, bool), Error> {
    let mut diagnostics = Vec::new();
    let mut builtins: Vec<Arc<dyn Scheme>> = vec![
        Arc::new(SkillScheme::new(definitions)),
        Arc::new(DirScheme::local(cx.session_dir.join(LOCAL_DIR))),
    ];
    let docs = docs_dir.is_some_and(|docs_dir| {
        let ready = match docs {
            Some(ready) => ready,
            None => match ensure_bundled_docs(docs_dir, name) {
                Ok(()) => true,
                Err(error) => {
                    diagnostics.push(Diagnostic {
                        origin: Some(DiagnosticOrigin::Path(docs_dir.to_path_buf())),
                        message: error.to_string(),
                    });
                    false
                }
            },
        };
        if ready {
            builtins.push(Arc::new(DirScheme::docs(name, docs_dir.to_path_buf())));
        }
        ready
    });
    if let Some(live) = live {
        builtins.push(Arc::new(ProcScheme {
            jobs: live.jobs.clone(),
        }));
        builtins.push(Arc::new(AgentScheme::new(
            live.jobs,
            live.sessions,
            live.store,
        )));
        builtins.push(Arc::new(crate::schemes::tool::ToolScheme::new(
            live.deferred,
        )));
    }
    let artifacts = Arc::new(Artifacts::new(cx.session_dir.join(ARTIFACTS_DIR)));
    let schemes = Schemes::new(cx, artifacts, builtins, host)?;
    Ok((schemes, diagnostics, docs))
}

#[expose(block = SESSION_CORE, object = "session")]
impl Session {
    /// The session file. With lazy creation it exists only after the first submission.
    #[ext]
    pub fn path(&self) -> &Path {
        &self.hooks.path
    }

    /// The header id (UUIDv7), known before the session file exists.
    #[ext(skip = "the handle carries it as the `id` property")]
    pub fn id(&self) -> &str {
        &self.hooks.session_id
    }

    /// The spawn link of a child session; `None` for root sessions and forks.
    #[ext]
    pub fn spawn_link(&self) -> Option<&store::SpawnLink> {
        self.hooks.child.as_ref().map(|child| &child.link)
    }

    /// The current root persona or the definition this child was spawned with.
    #[ext]
    pub fn agent(&self) -> Option<AgentSpec> {
        self.hooks
            .agent_spec
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The effective configuration for this session.
    #[ext]
    pub fn config(&self) -> &EffectiveConfig {
        &self.inherited.config
    }

    /// Sets the file-wide session name; newlines become spaces and blank names clear it.
    /// May be called while a run is active; this entry contributes no context message.
    #[ext]
    pub async fn set_name(&self, name: Option<String>) -> Result<SeqEntry, Error> {
        let name = normalize_focus(name.map(|name| name.replace(['\r', '\n'], " ")));
        let mut appended = self
            .hooks
            .append(vec![Entry::SessionInfo {
                base: journal::base(),
                name,
            }])
            .await?;
        Ok(appended.remove(0))
    }

    /// The latest session name in file order, independent of the selected branch.
    #[ext]
    pub fn name(&self) -> Option<String> {
        self.hooks.journal.read(|view| {
            view.entries
                .iter()
                .rev()
                .find_map(|seq| match &seq.entry {
                    Entry::SessionInfo { name, .. } => Some(name.clone()),
                    _ => None,
                })
                .flatten()
        })
    }

    /// Appends branch-scoped host state without adding a context message.
    /// May be called while a run is active; an empty type is rejected.
    #[ext(skip = "namespaced per caller; see the wire method")]
    pub async fn append_custom(
        &self,
        custom_type: &str,
        data: Option<Value>,
    ) -> Result<SeqEntry, Error> {
        if custom_type.is_empty() {
            return Err(Error::InvalidInput("Custom type must not be empty".into()));
        }
        let mut appended = self
            .hooks
            .append(vec![Entry::Custom {
                base: journal::base(),
                custom_type: custom_type.to_owned(),
                data,
            }])
            .await?;
        Ok(appended.remove(0))
    }

    /// The latest host state of this type on the current root-to-leaf path.
    #[ext(skip = "namespaced per caller; see the wire method")]
    pub fn custom(&self, custom_type: &str) -> Option<Value> {
        self.hooks.journal.read(|view| {
            let leaf = view.entries.last()?.entry.id()?;
            store::path_to(&view.entries, leaf)
                .into_iter()
                .rev()
                .find_map(|at| match &view.entries[at].entry {
                    Entry::Custom {
                        custom_type: kind,
                        data,
                        ..
                    } if kind == custom_type => Some(data.clone()),
                    _ => None,
                })
                .flatten()
        })
    }

    /// The skills in force now; the prompt's catalog and `/name` commands use these.
    #[ext]
    pub fn skills(&self) -> Vec<Skill> {
        self.hooks.definitions.get().skills.clone()
    }

    /// Agent and team definitions in force now.
    #[ext]
    pub fn agents(&self) -> Vec<AgentDef> {
        self.hooks.definitions.get().agents.clone()
    }

    /// Start-time context, skill, prompt, and scheme diagnostics, also logged as warnings.
    #[ext]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The background jobs `read proc://` lists (promoted, retained and rebuilt,
    /// finished ones included), ascending.
    #[ext]
    pub fn jobs(&self) -> Vec<JobInfo> {
        self.hooks.tools.runner.jobs.list()
    }

    /// Stops job `id` (`bg_N` or `agent_N`), reporting whether it was running.
    #[ext]
    pub fn kill_job(&self, id: &str) -> crate::coordination::KillReceipt {
        let stopping = JobId::parse(id).is_some_and(|id| {
            self.hooks
                .tools
                .runner
                .jobs
                .kill(id, crate::jobs::Killer::Host)
        });
        crate::coordination::KillReceipt {
            id: id.to_owned(),
            stopping,
        }
    }

    /// Opens the call's newest command's flushed raw output, foreground commands included.
    /// A handle remains readable after the command's spool is unlinked.
    #[ext(skip = "live reader; the wire method reads one page")]
    pub fn command_output(&self, call_id: &str) -> Option<crate::jobs::CommandOutput> {
        self.hooks
            .tools
            .runner
            .jobs
            .command_output(call_id, self.hooks.tools.schemes.artifacts().dir())
    }

    /// Opens one command's flushed raw output by job id (`bg_N`), foreground commands included.
    #[ext(skip = "live reader; the wire method reads one page")]
    pub fn job_output(&self, job_id: &str) -> Option<crate::jobs::CommandOutput> {
        self.hooks
            .tools
            .runner
            .jobs
            .job_output(job_id, self.hooks.tools.schemes.artifacts().dir())
    }

    /// The job ids of the commands a tool call started, oldest first, foreground commands included.
    #[ext]
    pub fn command_jobs(&self, call_id: &str) -> Vec<String> {
        self.hooks.tools.runner.jobs.command_jobs(call_id)
    }

    /// Requests promotion of every foreground command the call started. A command that finishes first stays foreground.
    #[ext]
    pub fn background(&self, call_id: &str) -> bool {
        self.hooks.tools.runner.jobs.background(call_id)
    }

    /// The tools this session exposes: direct ones in prompt order, then deferred catalog
    /// entries. Reading it promotes nothing.
    #[ext]
    pub fn tools(&self) -> Vec<SessionToolInfo> {
        self.hooks.tool_infos()
    }

    /// This session's URL schemes: recognize with `exact`, resolve files with `resolve`,
    /// or read UTF-8 text from either file or virtual targets with `read`.
    #[ext(
        skip = "live registry; scheme operations are on tools (e.g. parseUrl, exactUrl, locate, readFile)"
    )]
    pub fn schemes(&self) -> &Schemes {
        &self.hooks.tools.schemes
    }

    /// The commands a host may offer: built-ins, modes, registered commands, then unhidden
    /// skills.
    #[ext]
    pub fn commands(&self) -> Vec<CommandInfo> {
        let defs = self.hooks.definitions.get();
        commands::catalog(&defs.skills, &defs.modes, &self.hooks.commands())
    }

    /// The mode definitions in force now; empty for children.
    #[ext]
    pub fn modes(&self) -> Vec<Arc<ModeSpec>> {
        self.hooks.definitions.get().modes.clone()
    }

    /// Every keyword definition in force now, disabled ones included; empty for children.
    #[ext]
    pub fn keywords(&self) -> Vec<Arc<KeywordSpec>> {
        self.hooks.definitions.get().keywords.clone()
    }

    /// The keywords that would fire in `text` typed as a message right now, as byte ranges
    /// into `text`. A leading `/command` token is skipped, as on submission.
    #[ext]
    pub fn keyword_matches(&self, text: &str) -> Vec<KeywordMatch> {
        let active = self.hooks.active_keywords();
        delivery::keyword_spans(text, true, &active)
            .into_iter()
            .map(|(name, range)| KeywordMatch { name, range })
            .collect()
    }

    /// Parses built-in commands first, then commands naming this session's modes, then
    /// registered commands.
    #[ext(skip = "nested result; the wire method flattens it")]
    pub fn parse_command(&self, text: &str) -> Option<Result<Command, Error>> {
        if let Some(command) = Command::parse(text) {
            return Some(command);
        }
        let rest = text.strip_prefix('/')?;
        let (name, prompt) = match rest.find(char::is_whitespace) {
            Some(index) => (&rest[..index], rest[index..].trim()),
            None => (rest, ""),
        };
        let defs = self.hooks.definitions.get();
        if defs.modes.iter().any(|mode| mode.name == name) {
            return Some(Ok(Command::Mode {
                name: name.to_owned(),
                prompt: (!prompt.is_empty()).then(|| prompt.to_owned()),
            }));
        }
        if !commands::valid_command_name(name) {
            return None;
        }
        self.hooks
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|command| command.name == name)
            .then(|| {
                Ok(Command::Registered {
                    name: name.to_owned(),
                    args: prompt.to_owned(),
                })
            })
    }

    /// Runs a command under its policy: `model`/`thinking`/`tier`, modes and registered commands
    /// alongside a turn, `compact`/`shake`/`navigate` only when idle (`Error::Busy`
    /// otherwise).
    #[ext]
    pub async fn run_command(&self, command: Command) -> Result<(), Error> {
        match command {
            Command::Registered { name, args } => {
                let handler = {
                    let defs = self.hooks.definitions.get();
                    let registered = self
                        .hooks
                        .commands
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    commands::registered_winner(&name, &registered, &defs.modes)
                        .map(|command| command.handler.clone())
                };
                let handler = handler.ok_or_else(|| Error::InvalidCommand {
                    command: name,
                    message: "unknown command".into(),
                })?;
                handler.run(self, &args).await
            }
            Command::Mode { name, prompt } => {
                self.set_mode(Some(&name)).await?;
                if let Some(prompt) = prompt {
                    self.steer(prompt).await?;
                }
                Ok(())
            }
            Command::Compact { focus } => self.compact(focus).await,
            Command::Shake { mode } => self.shake(mode).await,
            Command::Model { model } => {
                let choice = self.hooks.resolve_model(&model)?;
                let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
                detached(async move { hooks.select_choice(&agent, choice).await }).await
            }
            Command::Thinking { level } => self.set_thinking(level).await.map(drop),
            Command::Tier { tier } => self.set_service_tier(tier).await.map(drop),
            Command::Navigate {
                entry,
                summarize,
                focus,
            } => {
                self.navigate(Some(entry), NavigateOptions { summarize, focus })
                    .await
            }
        }
    }

    /// Stops the active run (`CancelReason::User`). Stopping an active run holds queued
    /// input and job results until the user's next input or `resume_wakes`; input sent
    /// after the Stop starts the next run once this one settles. An idle Stop changes nothing.
    #[ext]
    pub fn abort(&self) {
        self.hooks.activity.send_if_modified(|activity| {
            if activity.running {
                self.hooks.stop_requested.store(true, Ordering::Release);
                self.hooks.set_auto_paused(true);
            }
            false
        });
        self.hooks.cancel_maintenance(false);
        self.agent.cancel(CancelReason::User);
    }

    /// Stops the session as a host does before exiting: the active run, and any run started
    /// later, is cancelled with `CancelReason::Shutdown`, so nothing is recorded as aborted
    /// and the run resumes when the session is next opened. Then every command job is
    /// killed. Returns once the run settled and every job task finished.
    #[ext(skip = "lifecycle belongs to the host")]
    pub async fn shutdown(&self) {
        // Record CancelReason::Shutdown before cancelling the shared maintenance token.
        self.agent.shutdown();
        self.hooks.cancel_maintenance(true);
        let _ = self
            .hooks
            .activity
            .subscribe()
            .wait_for(|activity| !activity.running)
            .await;
        let ids: Vec<_> = self
            .hooks
            .tools
            .runner
            .jobs
            .children()
            .into_iter()
            .map(|child| child.session_id)
            .collect();
        let children: Vec<_> = ids
            .iter()
            .filter_map(|id| self.inherited.sessions.live(id))
            .collect();
        futures::future::join_all(children.iter().map(|child| Box::pin(child.shutdown()))).await;
        self.inherited.sessions.remove(&ids);
        self.hooks.tools.runner.jobs.shutdown().await;
    }

    /// Switches the root persona from the next request, applying its model selector when
    /// present. Changing the system prompt and tools is cache-heavy on models without
    /// mid-conversation system-message and tool changes.
    #[ext]
    pub async fn set_agent(&self, name: Option<&str>) -> Result<(), Error> {
        self.reject_child()?;
        let spec = name
            .map(|name| {
                let spec = self
                    .hooks
                    .definitions
                    .get()
                    .agents
                    .iter()
                    .find_map(|definition| match definition {
                        AgentDef::Agent(spec) if spec.name == name => Some(spec.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| Error::AgentNotFound(name.to_owned()))?;
                if spec.run_as == RunAs::Subagent {
                    return Err(Error::AgentNotRoot(name.to_owned()));
                }
                Ok(spec)
            })
            .transpose()?;
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move {
            let _order = hooks.selection_order.lock().await;
            let choice = spec
                .as_ref()
                .and_then(|spec| spec.model.as_deref())
                .map(|selector| hooks.resolve_model(selector))
                .transpose()?;
            let mut entries = vec![Entry::AgentChange {
                base: journal::base(),
                agent: spec.as_ref().map(|spec| spec.name.clone()),
            }];
            let selection = choice.map(|choice| {
                let current = hooks.selection();
                let model = choice.model.unwrap_or_else(|| current.model.clone());
                let target = hooks.target_selection(&current, model, choice.thinking, choice.tier);
                entries.extend(target.entries_from(&current, true));
                target
            });
            hooks.append(entries).await?;
            if let Some(target) = selection {
                let definition = hooks.registry_definition(&target.model);
                hooks.apply_selection(
                    &agent,
                    Some((target.model, definition)),
                    target.thinking,
                    target.tier,
                );
            }
            let (new_shape, nested) = hooks.reshape(spec.clone()).await;
            *hooks
                .prompt
                .shape
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = new_shape;
            hooks
                .tools
                .context
                .reset(nested.root, nested.names, &nested.startup, &nested.seen);
            hooks
                .tools
                .context
                .restore(&hooks.journal.view().projection.messages);
            hooks.install_tools();
            *hooks
                .agent_spec
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = spec;
            hooks.prompt.rebuild();
            hooks.emit_context();
            hooks.rescope();
            Ok(())
        })
        .await
    }

    /// Settles the pending question of tool call `call_id`. A submission that does not fit
    /// the document returns `Error::InvalidInput` and leaves the question pending.
    #[ext]
    pub fn answer(&self, call_id: &str, answers: Vec<Answer>) -> Result<(), Error> {
        self.hooks.questions.answer(call_id, answers)
    }

    /// Dismisses a pending question or mode result. A dismissed question returns a result
    /// telling the model to continue without answers. A dismissed mode result cancels the run.
    #[ext]
    pub fn dismiss(&self, call_id: &str) -> Result<(), Error> {
        let Err(error) = self.hooks.questions.dismiss(call_id) else {
            return Ok(());
        };
        self.hooks
            .results
            .settle(call_id, modes::ResultOutcome::Dismissed)
            .map_err(|_| error)?;
        self.abort();
        Ok(())
    }
}

/// Dropping a session shuts it down: its run task then settles and releases the writer
/// lock, and every command job's container is killed.
impl Drop for Session {
    fn drop(&mut self) {
        self.agent.shutdown();
        self.hooks.cancel_maintenance(true);
        self.hooks.tools.runner.jobs.kill_all();
        let ids: Vec<_> = self
            .hooks
            .tools
            .runner
            .jobs
            .children()
            .into_iter()
            .map(|child| child.session_id)
            .collect();
        self.inherited.sessions.remove(&ids);
        self.inherited
            .sessions
            .unregister(self.id(), &self.hooks.tools.runner.jobs);
    }
}
