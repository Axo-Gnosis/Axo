//! A conversation with one agent in one working directory, saved as it happens.

pub(crate) mod children;
pub(crate) mod coordination;
pub(crate) mod delivery;
pub(crate) mod journal;
pub(crate) mod maintenance;
pub(crate) mod modes;
mod policy;
pub(crate) mod prompt;
mod resume;
mod side_files;
pub(crate) mod spawn;
mod surface;
pub(crate) mod team;
pub(crate) mod wire;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{
    Agent, AgentConfig, AgentContext, AgentEvent, AgentMessage, CancelReason, HookError, LoopHooks,
    Persist, PersistError, QueuedMessage, Replay, RunOutcome, Tool, ToolHooks, TurnContext,
    TurnFinish,
};
use axo_ai::thinking::clamp_level;
use axo_ai::{
    Accumulator, AssistantMessage, Message, Model, Models, ServiceTier, StopReason, Streamer,
    ThinkingLevel, ToolCall, ToolResultMessage, Usage, UserBlock,
};
use axo_macros::expose;
use futures::FutureExt;
use futures::future::{BoxFuture, join_all};
use serde_json::Value;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use self::children::{ChildState, SessionTable, TurnBudget};
pub use self::delivery::Activity;
use self::delivery::{ActivityState, enqueue};
use self::journal::Journal;
use self::maintenance::Maintenance;
pub(crate) use self::modes::MODE_NOTICE;
pub use self::modes::{AcceptResultOptions, PendingResult};
use self::policy::{ModeGate, ScopeCell, effective_bash, effective_writes, tool_hooks};
pub use self::policy::{ToolPolicy, ToolScope};
pub use self::prompt::{ContextFilesInspection, PromptInspection};
use self::prompt::{PromptState, SessionPrompt, ShapeBase, ToolBase, shape};
use self::resume::Resumption;
use self::side_files::{ARTIFACTS_DIR, LOCAL_DIR, copy_for_fork};
use self::spawn::SpawnTool;
pub use self::spawn::{
    ChildHandle, InlineSpec, SpawnContext, SpawnDecision, SpawnHook, SpawnOrigin, SpawnRequest,
    SpawnTeam, TeamHandle, TeamMemberRequest, TeamRequest,
};
pub(crate) use self::surface::TOOL_CATALOG;
use self::team::TeamSeat;
use crate::commands::{self, Command, CommandInfo, RegisteredCommand};
use crate::compaction::{self, Limits};
use crate::config::{EffectiveConfig, ModelChoice, ProjectAccess};
use crate::context::agents::{AgentDef, AgentSpec, RunAs};
use crate::context::definitions::{
    Definitions, FileDefinitions, LiveDefinitions, ResolvedDefinitions, merge,
};
use crate::context::files::NestedContext;
use crate::context::keywords::{KeywordMatch, KeywordSpec};
use crate::context::modes::{BashAccess, ModeSpec};
use crate::context::prompt::PromptSources;
use crate::context::skills::Skill;
use crate::context::{Diagnostic, DiagnosticOrigin};
use crate::error::Error;
use crate::exec::ExecEnv;
use crate::exec::host::ProcessSupervisor;
use crate::input::{PromptInput, PromptPart};
use crate::jobs::{ChildProgress, JobHost, JobId, JobInfo, JobRegistry};
use crate::recovery::{self, RecoveryPlan, plan_recovery};
use crate::schemes::agent::AgentScheme;
use crate::schemes::artifact::Artifacts;
use crate::schemes::dir::{DirScheme, ensure_bundled_docs};
use crate::schemes::proc::ProcScheme;
use crate::schemes::skill::SkillScheme;
use crate::schemes::{Scheme, SchemeContext, Schemes};
use crate::store::{
    self, CompactionReason, Entry, EntryBase, EntryId, Header, Projection, SeqEntry, SessionStore,
    SessionWriter, SubmissionKind, project,
};
use crate::tools::output::{MAX_YIELD_FAILURES, YIELD, YIELD_NUDGE, YieldState, YieldTool};
use crate::tools::{self, Answer, AskBlock, Exposure, HostTool, PendingQuestions, SessionTools};

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

/// One set of live registrations, for a host or a composition layer.
#[derive(Clone, Default)]
pub struct Registrations {
    /// Agents, modes, keywords, and skills supplied in code; the first name wins.
    pub definitions: Definitions,
    /// Tools built per session (and per fork) from its `SessionTools`; a tool named like a built-in replaces it.
    pub tools: Vec<HostTool>,
    /// Ordered tool policies, inherited by forks and children.
    pub tool_policies: Vec<Arc<dyn ToolPolicy>>,
    /// Ordered spawn policies, inherited by children.
    pub spawn_hooks: Vec<Arc<dyn SpawnHook>>,
    /// A scheme named like a built-in replaces it; artifact, file, and tool are reserved.
    pub schemes: Vec<Arc<dyn Scheme>>,
    /// Prompt sections, applied in order; `None` removes a section.
    pub sections: Vec<(String, Option<String>)>,
    /// Commands listed after modes; forks inherit them.
    pub commands: Vec<RegisteredCommand>,
}

/// The host's own registrations and the layer applied after them.
#[derive(Clone, Default)]
pub(crate) struct Layers {
    pub host: Registrations,
    pub layer: Registrations,
}

impl Layers {
    /// The host's entries of one list followed by the layer's.
    pub(crate) fn joined<T: Clone>(&self, of: impl Fn(&Registrations) -> &Vec<T>) -> Vec<T> {
        of(&self.host)
            .iter()
            .chain(of(&self.layer))
            .cloned()
            .collect()
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

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
    specta::Type,
)]
#[serde(rename_all = "lowercase")]
pub enum ShakeMode {
    #[default]
    Elide,
    Images,
    Thinking,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct NavigateOptions {
    pub summarize: bool,
    /// What the branch summary should prioritize (trimmed; blank counts as none); requires `summarize`.
    pub focus: Option<String>,
}

/// Trims a user's summary focus in place; blank counts as none.
pub(crate) fn normalize_focus(focus: Option<String>) -> Option<String> {
    let mut text = focus?;
    text.truncate(text.trim_end().len());
    let start = text.len() - text.trim_start().len();
    text.drain(..start);
    (!text.is_empty()).then_some(text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum MaintenanceKind {
    Compact,
    Shake,
    BranchSummary,
}

// Boxing `Agent` would not save anything: the broadcast clones the value per receiver.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SessionEvent {
    Agent(AgentEvent),
    /// Authoritative ownership state, including maintenance and navigation.
    Activity(Activity),
    /// An entry was appended to the session.
    Entry(SeqEntry),
    /// The pending input queue changed; refresh the live queued snapshot.
    QueueChanged,
    /// An `ask` call is waiting for `Session::answer` or `Session::dismiss`.
    QuestionPending {
        call_id: String,
        blocks: Vec<AskBlock>,
    },
    QuestionResolved {
        call_id: String,
    },
    MaintenanceStart {
        kind: MaintenanceKind,
    },
    MaintenanceEnd {
        kind: MaintenanceKind,
        error: Option<String>,
        aborted: bool,
    },
    Retry {
        attempt: u32,
        delay_ms: u64,
        max_retries: u32,
        error: Option<String>,
        failure: Option<axo_ai::ProviderFailure>,
    },
    /// A listed job appeared or changed.
    Job(JobInfo),
    /// A job left the list.
    JobGone {
        id: String,
    },
    /// The estimated context and the current model's compaction limits.
    Context(ContextUsage),
    /// Automatic result delivery was paused or resumed.
    AutoPaused {
        paused: bool,
    },
    /// The selected session mode changed.
    Mode {
        mode: Option<String>,
    },
    /// The selected mode reached a request boundary.
    EffectiveMode {
        mode: Option<String>,
    },
    /// A mode result, optionally waiting for host review.
    ModeResult {
        call_id: String,
        mode: String,
        result: Value,
        awaiting_review: bool,
    },
    /// A pending review left the pending list, with how it ended.
    ModeResultResolved {
        call_id: String,
        outcome: ResultResolution,
    },
    /// The session's whole-tree spend changed: its own entries plus every descendant's.
    TreeUsage {
        usage: Usage,
    },
}

/// The `type` tag of a [`SessionEvent`], used to select which events to receive.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, specta::Type,
)]
#[serde(rename_all = "camelCase")]
pub enum SessionEventKind {
    Agent,
    Activity,
    Entry,
    QueueChanged,
    QuestionPending,
    QuestionResolved,
    MaintenanceStart,
    MaintenanceEnd,
    Retry,
    Job,
    JobGone,
    Context,
    AutoPaused,
    Mode,
    EffectiveMode,
    ModeResult,
    ModeResultResolved,
    TreeUsage,
}

#[expose]
impl SessionEvent {
    /// Returns this event's tag without serializing its payload.
    #[ext(skip = "derivable from the value in TypeScript")]
    pub fn kind(&self) -> SessionEventKind {
        match self {
            Self::Agent(_) => SessionEventKind::Agent,
            Self::Activity(_) => SessionEventKind::Activity,
            Self::Entry(_) => SessionEventKind::Entry,
            Self::QueueChanged => SessionEventKind::QueueChanged,
            Self::QuestionPending { .. } => SessionEventKind::QuestionPending,
            Self::QuestionResolved { .. } => SessionEventKind::QuestionResolved,
            Self::MaintenanceStart { .. } => SessionEventKind::MaintenanceStart,
            Self::MaintenanceEnd { .. } => SessionEventKind::MaintenanceEnd,
            Self::Retry { .. } => SessionEventKind::Retry,
            Self::Job(_) => SessionEventKind::Job,
            Self::JobGone { .. } => SessionEventKind::JobGone,
            Self::Context(_) => SessionEventKind::Context,
            Self::AutoPaused { .. } => SessionEventKind::AutoPaused,
            Self::Mode { .. } => SessionEventKind::Mode,
            Self::EffectiveMode { .. } => SessionEventKind::EffectiveMode,
            Self::ModeResult { .. } => SessionEventKind::ModeResult,
            Self::ModeResultResolved { .. } => SessionEventKind::ModeResultResolved,
            Self::TreeUsage { .. } => SessionEventKind::TreeUsage,
        }
    }
}

/// How a pending mode result review ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum ResultResolution {
    /// `Session::accept_result`.
    Accepted,
    /// `Session::revise_result`.
    Revised,
    /// `Session::dismiss`.
    Dismissed,
    /// A steer arrived while the review was pending.
    Steered,
    /// A mode change ended the mode before review.
    Superseded,
    /// The `yield` call was cancelled while waiting.
    Withdrawn,
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

/// A prompt with an optional client id that makes resubmission idempotent.
#[derive(Debug, Clone, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptRequest {
    pub input: PromptInput,
    pub request_id: Option<String>,
}

/// What `Session::prompt_with` did.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PromptResult {
    /// The prompt was submitted and its run has settled.
    Settled {
        submission: SeqEntry,
        outcome: RunOutcome,
    },
    /// The `request_id` was already submitted: the earlier `submission` entry. No run
    /// was started.
    Duplicate(SeqEntry),
}
/// A durable host instruction, delivered without changing the prompt's system sections.
#[derive(Debug, Clone, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostMessage {
    pub custom_type: String,
    pub text: String,
    pub display: bool,
    pub delivery: HostDelivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum HostDelivery {
    /// The next turn boundary; starts a run when idle.
    Steer,
    /// When the active run would end; starts a run when idle.
    FollowUp,
    /// The next turn boundary or run start; never starts a run.
    Quiet,
}

/// Estimated context size and effective compaction limits, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub tokens: u64,
    pub window: u64,
    pub trigger: u64,
    pub reserve: u64,
}

/// A client's view when attaching: what it missed, the live state, and the events from
/// there on. All three come from one point in the append order.
#[derive(Debug)]
pub struct Attach {
    /// Entries with a `seq` after the one the client already has (all with `None`).
    pub missed: Vec<SeqEntry>,
    pub live: LiveSnapshot,
    pub events: broadcast::Receiver<Arc<SessionEvent>>,
}

#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct LiveSnapshot {
    /// The open run, if any.
    pub run_id: Option<String>,
    pub maintenance: Option<MaintenanceKind>,
    pub running: bool,
    pub streaming: Option<AssistantMessage>,
    pub pending_tool_calls: Vec<ToolCall>,
    /// Steers, then follow-ups, each in delivery order.
    pub queued: Vec<QueuedInput>,
    /// The projected conversation at this point in the append order, from the journal's view.
    pub messages: Vec<AgentMessage>,
    /// Unanswered `ask` calls by tool-call id. A question raised while attaching can
    /// also arrive as a `QuestionPending` event (duplicated, never missed).
    pub pending_questions: Vec<(String, Vec<AskBlock>)>,
    /// Mode results currently waiting for host review.
    pub pending_results: Vec<PendingResult>,
    /// The background jobs, as [`Session::jobs`] returns them.
    pub jobs: Vec<JobInfo>,
    /// Recorded spend, including maintenance and failed assistant requests, without run/job rollups.
    pub usage: Usage,
    /// `usage` plus every descendant session's own recorded spend, maintenance included.
    pub tree_usage: Usage,
    pub model: Model,
    pub thinking: ThinkingLevel,
    pub service_tier: ServiceTier,
    pub agent: Option<String>,
    pub mode: Option<String>,
    pub effective_mode: Option<String>,
    pub context: Option<ContextUsage>,
    pub auto_paused: bool,
}

/// One steer or follow-up waiting in the agent's queues.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct QueuedInput {
    pub submission_id: String,
    /// `Prompt` (input requeued when the session reopened) is delivered like `Steer`.
    pub kind: SubmissionKind,
    pub parts: Vec<PromptPart>,
    /// Who sent the input when it did not come from the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<crate::coordination::SubmissionProvenance>,
    /// `Some` for a host `post` message; resubmit it with `Session::post`, not `steer`.
    pub custom: Option<store::SubmittedCustom>,
}

impl QueuedInput {
    fn from_submission(entry: &Entry) -> Option<Self> {
        let Entry::Submission {
            submission_id,
            kind,
            parts,
            provenance,
            custom,
            ..
        } = entry
        else {
            return None;
        };
        Some(Self {
            submission_id: submission_id.clone(),
            kind: *kind,
            parts: parts.clone(),
            provenance: provenance.clone(),
            custom: custom.clone(),
        })
    }
}

fn queued_inputs(entries: &[SeqEntry], ids: Vec<String>) -> Vec<QueuedInput> {
    if ids.is_empty() {
        return Vec::new();
    }
    let mut positions: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect();
    let mut slots = vec![None; ids.len()];
    for seq in entries.iter().rev() {
        if let Entry::Submission { submission_id, .. } = &seq.entry
            && let Some(index) = positions.remove(submission_id.as_str())
        {
            slots[index] = QueuedInput::from_submission(&seq.entry);
            if positions.is_empty() {
                break;
            }
        }
    }
    slots.into_iter().flatten().collect()
}

/// A new session copied from a point of another.
pub struct Fork {
    pub session: Session,
    /// The parts of the user message forked at, for the client to edit and resubmit.
    pub editable: Option<Vec<PromptPart>>,
}

/// Immutable host inputs inherited by forks and child sessions.
#[derive(Clone)]
pub(crate) struct Inherited {
    pub(crate) cwd: PathBuf,
    /// Resolved once by `Axo` for this session; a fork inherits its parent's.
    pub(crate) config: Arc<EffectiveConfig>,
    /// Where bundled docs are materialized for the `docs` section and `axo://`; `None`
    /// omits both.
    pub(crate) docs_dir: Option<PathBuf>,
    pub(crate) harness_name: String,
    pub(crate) files: Arc<FileDefinitions>,
    pub(crate) definition_diagnostics: Vec<Diagnostic>,
    pub(crate) agent: Option<AgentSpec>,
    pub(crate) streamer: Arc<dyn Streamer>,
    /// Where `/model` looks up models.
    pub(crate) models: Arc<Models>,
    pub(crate) exec: Arc<dyn ExecEnv>,
    /// Runs every local command in its own supervised container (process group or cgroup).
    pub(crate) supervisor: Arc<ProcessSupervisor>,
    pub(crate) ask_enabled: bool,
    pub(crate) store: Arc<dyn SessionStore>,
    pub(crate) sessions: Arc<SessionTable>,
    /// Child runs streaming at once under one root session and its live forks.
    pub(crate) run_slots: Arc<tokio::sync::Semaphore>,
    /// Some for children: the parent's docs flag; skips docs materialization.
    pub(crate) docs: Option<bool>,
}

/// Everything `Axo` resolved for a new, opened or forked session.
pub(crate) struct SessionSetup {
    pub(crate) inherited: Arc<Inherited>,
    pub(crate) layers: Layers,
    pub(crate) merged: Option<(ResolvedDefinitions, Vec<Diagnostic>)>,
    pub(crate) model: Model,
    /// Already clamped to the model.
    pub(crate) thinking: ThinkingLevel,
    pub(crate) tier: ServiceTier,
    /// The header id, forwarded to providers as the session id.
    pub(crate) session_id: String,
    pub(crate) writer: Box<dyn SessionWriter>,
    /// Entries already in the session; empty for a new one.
    pub(crate) entries: Vec<SeqEntry>,
    /// `project(&entries, None)`.
    pub(crate) projection: Projection,
    /// Recovery to run on `entries`; `None` for new sessions and forks.
    pub(crate) recovery: Option<RecoveryPolicy>,
    pub(crate) link: Option<store::SpawnLink>,
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

/// The open run's live state, updated and relayed under the same lock.
#[derive(Default)]
struct LiveRun {
    /// The open run's id, set by its `run_start` (or by recovery).
    run_id: Option<String>,
    streaming: Option<Accumulator>,
    tool_calls: Vec<ToolCall>,
    /// Assistant usage of the open run so far; seeded from the journal when a run resumes.
    usage: Usage,
}

/// The live request selection, journaled in the order changes take effect.
#[derive(Clone)]
struct Selection {
    model: Model,
    thinking: ThinkingLevel,
    tier: ServiceTier,
}

/// How `run_to_settle` starts its run.
#[allow(clippy::large_enum_variant)] // One owned startup value; boxing would add an allocation.
enum RunRequest {
    Prompt(Vec<QueuedMessage>),
    Continue,
    RecoverResponse {
        response: AssistantMessage,
        model: Model,
        results: Vec<ToolResultMessage>,
    },
}

/// Runs `request` and settles it. Callers spawn it, so the run ends (and `run_end` is
/// written) even when nobody awaits the result.
async fn run_to_settle(
    agent: Arc<Agent>,
    hooks: Arc<SessionHooks>,
    request: RunRequest,
) -> Result<RunOutcome, Error> {
    hooks.advance().await;
    // ponytail: a waiting child holds its slot while its own children queue; nested trees at maxConcurrent can deadlock — release the slot while in wait if maxDepth > 1 becomes common.
    // ponytail: only a team's first runs reserve slots together; later member runs queue one by one
    let _slot = if let Some(child) = &hooks.child {
        let first_slot = child
            .first_slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let reserved = match first_slot {
            Some(receiver) => receiver.await.ok(),
            None => None,
        };
        Some(match reserved {
            Some(permit) => permit,
            None => match hooks.inherited.run_slots.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    if let Some(parent) = hooks
                        .inherited
                        .sessions
                        .registry(&child.link.parent_session_id)
                    {
                        parent.child_queued(&hooks.session_id);
                    }
                    hooks
                        .inherited
                        .run_slots
                        .clone()
                        .acquire_owned()
                        .await
                        .expect("never closed")
                }
            },
        })
    } else {
        None
    };
    let result = std::panic::AssertUnwindSafe(async {
        match request {
            RunRequest::Prompt(messages) => agent.prompt(messages).await,
            RunRequest::Continue => agent.continue_run().await,
            RunRequest::RecoverResponse {
                response,
                model,
                results,
            } => agent.recover_response(response, model, results).await,
        }
    })
    .catch_unwind()
    .await;
    // A fallible hook may have committed an omission or a prefix of edits before
    // failing. Reconcile even then; the next prompt must not resurrect raw failures.
    agent.set_messages(hooks.journal.view().projection.messages.clone());
    match result {
        Ok(result) => hooks.settle(result).await,
        Err(panic) => {
            let outcome = if hooks.shutdown.load(Ordering::Acquire) {
                RunOutcome::Aborted(CancelReason::Shutdown)
            } else {
                RunOutcome::Failed("Agent task panicked".into())
            };
            let _ = hooks.settle(Ok(outcome)).await;
            std::panic::resume_unwind(panic)
        }
    }
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

fn check_section_name(name: &str) -> Result<(), String> {
    if crate::context::prompt::valid_section_name(name) {
        Ok(())
    } else {
        Err(format!("Invalid system prompt section name: {name}"))
    }
}

fn check_sections(sections: &[(String, Option<String>)]) -> Result<(), Error> {
    sections
        .iter()
        .try_for_each(|(name, _)| check_section_name(name))
        .map_err(Error::InvalidInput)
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

/// The own usage of the session file at `path` (spawn depth `depth`) plus, while
/// `depth < max_depth`, every child it started, read recursively from their files. Parked
/// files are immutable, so this equals what the live tree would report. An unreadable file
/// counts as zero.
fn subtree_usage<'a>(
    store: &'a dyn SessionStore,
    path: &'a Path,
    depth: u32,
    max_depth: u32,
) -> BoxFuture<'a, Usage> {
    Box::pin(async move {
        let (header, entries) = match store.open_read(path).await {
            Ok(read) => read,
            Err(err) => {
                warn!(
                    %err,
                    path = %path.display(),
                    "child session unreadable; its usage counts as zero"
                );
                return Usage::default();
            }
        };
        let mut usage = entries_usage(&entries);
        if depth < max_depth {
            // Only children this session owns; a fork's copied `child_start`s are its source's.
            let children = entries.iter().filter_map(|seq| match &seq.entry {
                Entry::ChildStart {
                    session_id,
                    child_path,
                    ..
                } if *session_id == header.id => {
                    Some(subtree_usage(store, child_path, depth + 1, max_depth))
                }
                _ => None,
            });
            for child in join_all(children).await {
                usage += &child;
            }
        }
        usage
    })
}

impl SessionHooks {
    fn live(&self) -> MutexGuard<'_, LiveRun> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn registrations(&self) -> MutexGuard<'_, Layers> {
        self.registrations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts the joined policies, spawn hooks and commands of `layers` in their cells;
    /// returns the commands' diagnostics. Callers hold the `registrations` lock, so these
    /// writes land in the order the layers were stored.
    fn apply_registrations(&self, agent: &Agent, layers: &Layers) -> Vec<Diagnostic> {
        self.install_policies(agent, layers.joined(|r| &r.tool_policies));
        *self
            .spawn_hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = layers.joined(|r| &r.spawn_hooks);
        self.install_commands(layers)
    }

    fn install_commands(&self, layers: &Layers) -> Vec<Diagnostic> {
        let commands = layers.joined(|r| &r.commands);
        let defs = self.definitions.get();
        let diagnostics = commands::registered_diagnostics(&commands, &defs.modes, &defs.skills);
        *self.commands.lock().unwrap_or_else(PoisonError::into_inner) = commands;
        diagnostics
    }

    /// Keep live state and its event on the same side of an attach snapshot.
    fn relay_live<T>(&self, event: &AgentEvent, update: impl FnOnce(&mut LiveRun) -> T) -> T {
        let mut live = self.live();
        let out = update(&mut live);
        let _ = self
            .events
            .send(Arc::new(SessionEvent::Agent(event.clone())));
        out
    }

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

    /// Show a child's executing tools on its parent's job row, so hosts see them without
    /// subscribing to the child.
    fn publish_child_tools(&self, tools: Vec<String>) {
        if let Some(parent) = self.parent_registry() {
            parent.child_tools(&self.session_id, tools);
        }
    }

    fn selection(&self) -> Selection {
        self.selection.borrow().clone()
    }

    fn model(&self) -> Model {
        self.selection.borrow().model.clone()
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

    /// Hides the failed response at entry `at` and its tool results from the context.
    async fn omit_response(&self, at: usize, response: &AssistantMessage) -> Result<(), Error> {
        let edits: Vec<Entry> = {
            let view = self.journal.view();
            let path = view.projection.path();
            let mut targets = Vec::new();
            for index in path.iter().copied().filter(|&index| index >= at) {
                if index > at
                    && matches!(
                        &view.entries[index].entry,
                        Entry::Message {
                            message: AgentMessage::Llm(Message::Assistant(_)),
                            ..
                        }
                    )
                {
                    break;
                }
                let entry = &view.entries[index].entry;
                if (index == at
                    || matches!(entry,
                        Entry::Message { message: AgentMessage::Llm(Message::ToolResult(result)), .. }
                            if response.tool_calls().any(|call| call.id == result.tool_call_id)))
                    && let Some(id) = entry.id()
                    && !view.projection.is_omitted(id)
                {
                    targets.push(EntryId(id.to_owned()));
                }
            }
            targets
                .into_iter()
                .map(|target_id| Entry::ContextEdit {
                    base: journal::base(),
                    target_id,
                    replacement: None,
                })
                .collect()
        };
        // Only this run's own loop changes its transcript meanwhile, so the targets hold.
        self.append(edits).await?;
        Ok(())
    }

    fn projected_messages(&self) -> Vec<AgentMessage> {
        self.journal.view().projection.messages.clone()
    }

    /// Closes the run with `run_end` (none after a shutdown) and releases the session
    /// for the next run, or starts delivering pending job results. Reservations and
    /// admissions the run never committed are released first. A failed run, or a failed
    /// `run_end`, pauses automatic delivery until the user acts; a Stop already latched
    /// when pressed, so input sent since then still starts the next run.
    async fn settle(
        &self,
        result: Result<RunOutcome, axo_agent::Error>,
    ) -> Result<RunOutcome, Error> {
        self.tools.runner.jobs.release_uncommitted();
        if !matches!(
            result,
            Ok(RunOutcome::Done | RunOutcome::Aborted(CancelReason::User))
        ) {
            self.set_auto_paused(true);
        }
        let run_id = {
            let mut live = self.live();
            live.streaming = None;
            live.tool_calls.clear();
            live.run_id.take()
        };
        let mut closed = Ok(());
        if let Some(run_id) = run_id {
            let end = match &result {
                Ok(RunOutcome::Done) => Some((store::RunOutcome::Done, None)),
                Ok(RunOutcome::Aborted(CancelReason::User)) => {
                    Some((store::RunOutcome::Aborted, None))
                }
                Ok(RunOutcome::Aborted(CancelReason::Shutdown)) => None,
                Ok(RunOutcome::Failed(error)) => {
                    Some((store::RunOutcome::Failed, Some(error.clone())))
                }
                Err(error) => Some((store::RunOutcome::Failed, Some(error.to_string()))),
            };
            if let Some((outcome, reason)) = end {
                // Held across the snapshot and the write: input queued meanwhile lands either
                // in queued or after this run_end.
                let order = self.queue_order.lock().await;
                closed = self
                    .append(vec![Entry::RunEnd {
                        base: journal::base(),
                        run_id: run_id.clone(),
                        outcome,
                        reason,
                        usage: Some(store::run_usage(&self.journal.view().entries, &run_id)),
                        queued: self.queued_submissions(),
                    }])
                    .await
                    .map(drop);
                drop(order);
                if closed.is_ok()
                    && let Some(child) = &self.child
                    && let Some(parent) = self
                        .inherited
                        .sessions
                        .registry(&child.link.parent_session_id)
                {
                    let message = crate::jobs::child::run_message(
                        &self.journal.view().entries,
                        &run_id,
                        child.link.spec.output.as_ref(),
                    );
                    if let Some(message) = message
                        && let Err(error) =
                            parent.child_message(&self.session_id, message, true).await
                    {
                        warn!(%error, "child result delivery failed");
                    }
                }
            }
        }
        if closed.is_err() {
            self.set_auto_paused(true);
        }
        self.release_or_deliver();
        closed?;
        result.map_err(Error::Agent)
    }

    /// Records differences between the current branch and the live selection
    /// (pi `_recordSelection`), so reopening restores what is live.
    async fn record_selection(
        &self,
        model: Option<&str>,
        thinking: Option<ThinkingLevel>,
        tier: Option<ServiceTier>,
        agent: Option<&str>,
    ) -> Result<(), Error> {
        let _order = self.selection_order.lock().await;
        let selection = self.selection();
        let mut entries = Vec::new();
        if model != Some(format!("{}/{}", selection.model.provider, selection.model.id).as_str()) {
            entries.push(Entry::ModelChange {
                base: journal::base(),
                provider: selection.model.provider.clone(),
                model_id: selection.model.id.clone(),
            });
        }
        if thinking != Some(selection.thinking) {
            entries.push(Entry::ThinkingLevelChange {
                base: journal::base(),
                thinking_level: selection.thinking,
                automatic: false,
                service_tier: None,
            });
        }
        if tier.unwrap_or_default() != selection.tier {
            entries.push(Entry::ServiceTierChange {
                base: journal::base(),
                service_tier: selection.tier,
            });
        }
        // A child's persona is its SpawnLink spec; agent_change names only root catalog personas.
        if self.child.is_none() {
            let live_agent = self
                .agent_spec
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(|spec| spec.name.clone());
            if agent != live_agent.as_deref() {
                entries.push(Entry::AgentChange {
                    base: journal::base(),
                    agent: live_agent,
                });
            }
        }
        if !entries.is_empty() {
            self.append(entries).await?;
        }
        Ok(())
    }

    /// Records the model, thinking level and tier, then applies them once written.
    async fn select_model(
        &self,
        agent: &Agent,
        model: Model,
        thinking: Option<ThinkingLevel>,
        tier: Option<ServiceTier>,
    ) -> Result<(), Error> {
        let _order = self.selection_order.lock().await;
        let current = self.selection();
        let (entries, level, tier) = self.model_entries(&current, &model, thinking, tier);
        self.append(entries).await?;
        let definition = self.registry_definition(&model);
        self.apply_selection(agent, model, level, tier, definition);
        Ok(())
    }

    /// The chosen tier after moving from `current` to `model`: a tier named by the selector
    /// wins; otherwise it survives only on the same model or under `serviceTier.onModelChange: "keep"`.
    fn next_tier(
        &self,
        current: &Selection,
        model: &Model,
        explicit: Option<ServiceTier>,
    ) -> ServiceTier {
        self.inherited
            .config
            .settings
            .service_tier
            .on_model_change
            .select_tier(
                Some((&current.model.provider, &current.model.id)),
                model,
                current.tier,
                explicit,
            )
    }

    fn model_entries(
        &self,
        current: &Selection,
        model: &Model,
        thinking: Option<ThinkingLevel>,
        tier: Option<ServiceTier>,
    ) -> (Vec<Entry>, ThinkingLevel, ServiceTier) {
        let level = clamp_level(model, thinking.unwrap_or(current.thinking));
        let tier = self.next_tier(current, model, tier);
        let mut entries = vec![Entry::ModelChange {
            base: journal::base(),
            provider: model.provider.clone(),
            model_id: model.id.clone(),
        }];
        if level != current.thinking {
            entries.push(Entry::ThinkingLevelChange {
                base: journal::base(),
                thinking_level: level,
                automatic: false,
                service_tier: None,
            });
        }
        if tier != current.tier {
            entries.push(Entry::ServiceTierChange {
                base: journal::base(),
                service_tier: tier,
            });
        }
        (entries, level, tier)
    }

    fn apply_selection(
        &self,
        agent: &Agent,
        model: Model,
        level: ThinkingLevel,
        tier: ServiceTier,
        definition: Option<Arc<Model>>,
    ) {
        agent.set_selection(Some(model.clone()), level, tier);
        *self
            .registry_model
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = definition;
        self.selection.send_replace(Selection {
            model,
            thinking: level,
            tier,
        });
        self.emit_context();
    }

    fn registry_definition(&self, model: &Model) -> Option<Arc<Model>> {
        self.inherited.models.lookup(&model.provider, &model.id)
    }

    /// Adopts the registry's definition of the selected model when it changed since this session
    /// last applied one, so limits, prices and thinking levels from a reload or refresh apply
    /// from the next request. A copy the host set stays while the registry is unchanged, and so
    /// does a model the registry no longer has (its request then fails with `UnknownModel`).
    async fn follow_registry(&self) -> Result<(), Error> {
        let _order = self.selection_order.lock().await;
        let (now, thinking, tier) = {
            let current = self.selection.borrow();
            let Some(now) = self.registry_definition(&current.model) else {
                return Ok(());
            };
            (now, current.thinking, current.tier)
        };
        let applied = self
            .registry_model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, &now) || old == &now);
        if applied {
            return Ok(());
        }
        let Some(agent) = self.agent.get().and_then(Weak::upgrade) else {
            return Ok(());
        };
        // Provider and id are unchanged, so only a clamped thinking level is journaled.
        let level = clamp_level(&now, thinking);
        if level != thinking {
            self.append(vec![Entry::ThinkingLevelChange {
                base: journal::base(),
                thinking_level: level,
                automatic: true,
                service_tier: None,
            }])
            .await?;
        }
        self.apply_selection(&agent, (*now).clone(), level, tier, Some(now));
        Ok(())
    }

    /// Journal and publish the two suffixes as one selection, or leave both untouched.
    async fn select_suffixes(
        &self,
        agent: &Agent,
        thinking: Option<ThinkingLevel>,
        tier: Option<ServiceTier>,
    ) -> Result<(ThinkingLevel, ServiceTier), Error> {
        let _order = self.selection_order.lock().await;
        let (level, chosen, effective, entry) = {
            let current = self.selection.borrow();
            let level =
                thinking.map_or(current.thinking, |level| clamp_level(&current.model, level));
            let chosen = tier.unwrap_or(current.tier);
            let effective = current.model.effective_tier(chosen);
            let entry = if level != current.thinking {
                Entry::ThinkingLevelChange {
                    base: journal::base(),
                    thinking_level: level,
                    automatic: false,
                    service_tier: (chosen != current.tier).then_some(chosen),
                }
            } else if chosen != current.tier {
                Entry::ServiceTierChange {
                    base: journal::base(),
                    service_tier: chosen,
                }
            } else {
                return Ok((level, effective));
            };
            (level, chosen, effective, entry)
        };
        self.append(vec![entry]).await?;
        self.apply_suffixes(agent, level, chosen);
        Ok((level, effective))
    }

    fn apply_suffixes(&self, agent: &Agent, level: ThinkingLevel, tier: ServiceTier) {
        agent.set_selection(None, level, tier);
        self.selection.send_modify(|selection| {
            selection.thinking = level;
            selection.tier = tier;
        });
    }

    /// Applies a resolved `/model` selector and any thinking or tier suffixes.
    pub(super) async fn select_choice(
        &self,
        agent: &Agent,
        choice: ModelChoice,
    ) -> Result<(), Error> {
        match choice.model {
            Some(model) => {
                self.select_model(agent, model, choice.thinking, choice.tier)
                    .await
            }
            None => {
                self.select_suffixes(agent, choice.thinking, choice.tier)
                    .await?;
                Ok(())
            }
        }
    }

    /// [`SessionHooks::record_selection`] against the current position.
    async fn record_current_selection(&self) -> Result<(), Error> {
        let (model, thinking, tier, agent) = {
            let view = self.journal.view();
            (
                view.projection.model.clone(),
                view.projection.thinking,
                view.projection.tier,
                view.projection.agent.clone(),
            )
        };
        self.record_selection(model.as_deref(), thinking, tier, agent.as_deref())
            .await
    }

    fn yield_nudge(&self) -> bool {
        self.child
            .as_ref()
            .and_then(|child| child.output.as_ref())
            .is_some_and(|output| output.nudge())
    }
}

/// Executing tool names in start order.
fn tool_names(live: &LiveRun) -> Vec<String> {
    live.tool_calls
        .iter()
        .map(|call| call.name.clone())
        .collect()
}

#[async_trait]
impl LoopHooks for SessionHooks {
    /// pi `convertToLlm` (`messages.ts:150-196`): a custom message reaches the model as a
    /// user message with its content.
    fn convert_to_llm(&self, messages: Vec<AgentMessage>) -> Vec<Message> {
        crate::context::lower_messages(messages)
    }

    fn should_skip_tools(&self, response: &AssistantMessage, model: &Model) -> bool {
        recovery::is_pressure(response, model)
            && (response.stop_reason != StopReason::Stop || response.tool_calls().next().is_some())
    }

    async fn prepare_request(
        &self,
        _context: &AgentContext,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<AgentMessage>>, HookError> {
        if let Some(child) = &self.child {
            child.budget.request();
        }
        self.follow_registry()
            .await
            .map_err(|error| HookError(error.to_string()))?;
        let compacted = self
            .automatic(CompactionReason::Threshold, cancel)
            .await
            .map_err(|error| HookError(error.to_string()))?;
        let announced = self
            .announce()
            .await
            .map_err(|error| HookError(error.to_string()))?;
        if compacted || announced {
            Ok(Some(self.projected_messages()))
        } else {
            Ok(None)
        }
    }

    async fn finish_turn(
        &self,
        turn: TurnContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<TurnFinish, HookError> {
        let result: Result<TurnFinish, Error> = async {
            let response = turn.message;
            self.check_live(cancel)?;
            let model = self.model();
            if !recovery::served_by(response, &model) {
                let state = {
                    let view = self.journal.view();
                    recovery::failure_state(&view.entries, &view.projection, response, turn.model)?
                };
                let unusable = state.hidden || recovery::foreign_unusable(response, &model);
                if unusable {
                    self.omit_response(state.at, response).await?;
                    return Ok(TurnFinish {
                        continue_turn: true,
                        messages: Some(self.projected_messages()),
                    });
                }
                return Ok(TurnFinish::default());
            }
            let pressure = recovery::is_pressure(response, turn.model);
            let silent_success = pressure
                && response.stop_reason == StopReason::Stop
                && response.tool_calls().next().is_none();
            if silent_success {
                let changed = self.automatic(CompactionReason::Overflow, cancel).await?;
                let messages = changed.then(|| self.projected_messages());
                return Ok(TurnFinish {
                    continue_turn: self.yield_nudge(),
                    messages,
                });
            }
            let faulty =
                pressure || matches!(response.stop_reason, StopReason::Error | StopReason::Length);
            if faulty {
                // `Some((delay, shrink))` retries after omitting the response.
                let (at, retries, retry) = {
                    let view = self.journal.view();
                    let state = recovery::failure_state(
                        &view.entries,
                        &view.projection,
                        response,
                        turn.model,
                    )?;
                    let retry = if pressure {
                        if !self.maintenance.settings.enabled || state.overflow_used {
                            return Ok(TurnFinish::default());
                        }
                        let completed = Limits::resolve(&model, &self.maintenance.settings)
                            .is_ok_and(|limits| {
                                recovery::checkpoint_completed(
                                    &view.entries,
                                    &view.projection,
                                    &state,
                                    &limits,
                                )
                            });
                        Some((None, !(state.hidden && completed)))
                    } else {
                        recovery::retry_delay(
                            &self.inherited.config.settings.retry,
                            response,
                            state.retries,
                        )
                        .map(|delay| (Some(delay), false))
                    };
                    (state.at, state.retries, retry)
                };
                let Some((delay, shrink)) = retry else {
                    return Ok(TurnFinish::default());
                };
                self.omit_response(at, response).await?;
                if let Some(delay) = delay {
                    let _ = self.events.send(Arc::new(SessionEvent::Retry {
                        attempt: retries + 1,
                        delay_ms: delay.as_millis() as u64,
                        max_retries: self.inherited.config.settings.retry.max_retries,
                        error: response.error_message.clone(),
                        failure: response.failure.clone(),
                    }));
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(Error::MaintenanceAborted),
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                if shrink {
                    self.automatic(CompactionReason::Overflow, cancel).await?;
                }
                return Ok(TurnFinish {
                    continue_turn: true,
                    messages: Some(self.projected_messages()),
                });
            }
            if response.tool_calls().next().is_none() {
                let changed = self.automatic(CompactionReason::Threshold, cancel).await?;
                let messages = changed.then(|| self.projected_messages());
                return Ok(TurnFinish {
                    continue_turn: self.yield_nudge(),
                    messages,
                });
            }
            Ok(TurnFinish::default())
        }
        .await;
        let result = result.map_err(|error| HookError(error.to_string()))?;
        if let Some(output) = self.child.as_ref().and_then(|child| child.output.as_ref()) {
            let failures = turn
                .tool_results
                .iter()
                .filter(|result| result.tool_name == YIELD && result.is_error)
                .count() as u32;
            if failures > 0 && output.record_failures(failures) >= MAX_YIELD_FAILURES {
                return Err(HookError(format!(
                    "yield was rejected {MAX_YIELD_FAILURES} times"
                )));
            }
        }
        Ok(result)
    }

    async fn prepare_next_turn(
        &self,
        turn: TurnContext<'_>,
        _cancel: &CancellationToken,
    ) -> Vec<AgentMessage> {
        self.advance().await;
        let mut messages = self.take_system();
        if let Some(child) = &self.child
            && child.budget.requests() == child.budget.limit
            && turn.message.tool_calls().next().is_some()
        {
            let limit = child.budget.limit;
            let text = if child.output.is_some() {
                format!(
                    "Turn budget exhausted: this run used its {limit} turns. Tool calls other than yield are no longer executed. Call yield now with your best final result."
                )
            } else {
                format!(
                    "Turn budget exhausted: this run used its {limit} turns. Tool calls are no longer executed. Reply now with your final answer: what you completed, what remains, and what your parent needs to continue."
                )
            };
            messages.push(AgentMessage::Custom(axo_agent::CustomMessage {
                custom_type: crate::jobs::child::TURN_BUDGET.into(),
                content: vec![UserBlock::Text { text }],
                display: true,
                details: None,
                timestamp: axo_ai::now_ms(),
            }));
        }
        if self
            .child
            .as_ref()
            .and_then(|child| child.output.as_ref())
            .is_some_and(|output| output.take_nudge())
        {
            messages.push(AgentMessage::Custom(axo_agent::CustomMessage {
                custom_type: YIELD_NUDGE.into(),
                content: vec![UserBlock::Text {
                    text: "You ended your turn without calling yield. Call yield now with your final result; your run ends when it is accepted.".into(),
                }],
                display: true,
                details: None,
                timestamp: axo_ai::now_ms(),
            }));
        }
        messages
    }

    /// Finished job results, mail and quiet host messages reach each steering drain
    /// point without setting the steer signal; mail also wakes `wait`.
    async fn steering(&self) -> Vec<QueuedMessage> {
        self.reserve_delivery()
    }
}

#[async_trait]
impl Persist for SessionHooks {
    /// Saves the run's start and every finished message before the loop goes on, then
    /// relays the event onto the session channel. Only the writes await, and they await
    /// only the journal task; live state changes under a lock that is never held across
    /// an await.
    async fn persist(&self, event: &AgentEvent) -> Result<(), PersistError> {
        if matches!(event, AgentEvent::AgentStart)
            && let Some(child) = &self.child
        {
            child.budget.reset();
            if let Some(output) = &child.output {
                output.reset();
            }
            if let Some(parent) = self
                .inherited
                .sessions
                .registry(&child.link.parent_session_id)
            {
                parent.child_running(&self.session_id);
            }
        }
        match event {
            // A resumed run keeps the id of the run it continues.
            AgentEvent::AgentStart if self.live().run_id.is_none() => {
                let run_id = Uuid::now_v7().to_string();
                self.append(vec![Entry::RunStart {
                    base: journal::base(),
                    run_id: run_id.clone(),
                }])
                .await
                .map_err(|err| PersistError(err.to_string()))?;
                let mut live = self.live();
                live.run_id = Some(run_id);
                live.usage = Usage::default();
            }
            AgentEvent::MessageUpdate { event: update } => {
                self.relay_live(event, |live| {
                    live.streaming
                        .get_or_insert_with(Accumulator::new)
                        .apply(update);
                });
                return Ok(());
            }
            AgentEvent::MessageEnd { message, origin } => {
                // Cleared before the entry lands: an attach in between sees neither and
                // receives the entry.
                self.live().streaming = None;
                let jobs = &self.tools.runner.jobs;
                let mut receipts = jobs.receipts(origin);
                let mut batch = Vec::with_capacity(2);
                // The start first: a receipt never names a job whose start is not on disk.
                batch.extend(receipts.start.take());
                batch.push(Entry::Message {
                    base: journal::base(),
                    message: message.clone(),
                    submission_id: receipts.submission_id.take(),
                    admitted_jobs: std::mem::take(&mut receipts.admitted_jobs),
                    delivered_jobs: std::mem::take(&mut receipts.delivered_jobs),
                });
                let appended = self.append(batch).await;
                jobs.settle(receipts, appended.is_ok()).await;
                appended.map_err(|err| PersistError(err.to_string()))?;
                let (open, usage) = {
                    let mut live = self.live();
                    if let AgentMessage::Llm(Message::Assistant(response)) = message {
                        live.usage += &response.usage;
                    }
                    (live.run_id.is_some(), live.usage.clone())
                };
                if matches!(message, AgentMessage::Llm(Message::Assistant(_)))
                    && let Some(child) = &self.child
                    && let Some(parent) = self.parent_registry()
                    && open
                {
                    let turns = child.budget.requests();
                    parent.child_progress(
                        &self.session_id,
                        ChildProgress {
                            turns,
                            max_turns: child.budget.limit,
                            wrapping_up: turns >= child.budget.limit,
                            yield_failures: child
                                .output
                                .as_ref()
                                .map_or(0, |output| output.failures()),
                            usage,
                            tools: Vec::new(),
                        },
                    );
                }
            }
            AgentEvent::ToolExecutionStart {
                call_id,
                name,
                args,
            } => {
                let tools = self.relay_live(event, |live| {
                    live.tool_calls.push(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: args.clone(),
                        arguments_error: None,
                    });
                    tool_names(live)
                });
                self.publish_child_tools(tools);
                return Ok(());
            }
            AgentEvent::ToolExecutionEnd { call_id, .. } => {
                let tools = self.relay_live(event, |live| {
                    live.tool_calls.retain(|call| &call.id != call_id);
                    tool_names(live)
                });
                self.publish_child_tools(tools);
                return Ok(());
            }
            AgentEvent::AgentEnd { .. } => {
                self.relay_live(event, |live| {
                    live.streaming = None;
                    live.tool_calls.clear();
                });
                self.publish_child_tools(Vec::new());
                return Ok(());
            }
            _ => {}
        }
        if matches!(event, AgentEvent::AgentStart)
            && self.stop_requested.swap(false, Ordering::AcqRel)
            && let Some(agent) = self.agent.get().and_then(Weak::upgrade)
        {
            agent.cancel(CancelReason::User);
        }
        let _ = self
            .events
            .send(Arc::new(SessionEvent::Agent(event.clone())));
        Ok(())
    }
}

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
    /// Builds the tools, loads context files and skills, resolves the exec environment
    /// once for its prompt note, restores the transcript and the read record, records
    /// model, thinking and prompt changes, and runs recovery. A resumed run, or requeued
    /// prompts, run in a spawned task.
    pub(crate) async fn start(setup: SessionSetup) -> Result<Session, Error> {
        let SessionSetup {
            inherited,
            mut layers,
            merged,
            model,
            thinking,
            tier,
            session_id,
            writer,
            entries,
            projection,
            recovery,
            link,
        } = setup;
        let Inherited {
            cwd,
            config,
            docs_dir,
            exec,
            ..
        } = inherited.as_ref();
        let mut section_diagnostics = Vec::new();
        for sections in [&mut layers.host.sections, &mut layers.layer.sections] {
            sections.retain(|(name, _)| match check_section_name(name) {
                Ok(()) => true,
                Err(message) => {
                    section_diagnostics.push(Diagnostic {
                        origin: None,
                        message,
                    });
                    false
                }
            });
        }
        let (resolved, merge_diagnostics) = merged.unwrap_or_else(|| {
            merge(
                &inherited.files,
                &[&layers.layer.definitions, &layers.host.definitions],
            )
        });
        let definitions = Arc::new(LiveDefinitions::new(resolved));
        let resolved = definitions.get();
        let joined_tools = layers.joined(|r| &r.tools);
        let joined_policies = layers.joined(|r| &r.tool_policies);
        let joined_schemes = layers.joined(|r| &r.schemes);
        let joined_commands = layers.joined(|r| &r.commands);

        let path = writer.path().to_path_buf();
        let (events, _) = broadcast::channel(1024);
        let questions = Arc::new(PendingQuestions::new(events.clone()));
        let settings = &config.settings;
        let project = config.access != ProjectAccess::Ignore;
        let root_agent = inherited.agent.as_ref();
        let (jobs, unfinished) = JobRegistry::new(
            session_id.clone(),
            link.as_ref().map_or(1, |link| link.depth + 1),
            &entries,
            inherited.sessions.clone(),
            events.clone(),
        );
        // Before the host attaches, so seeding never notifies; a revived child pushes its
        // reseeded total to its parent once started.
        let store = inherited.store.as_ref();
        let max_depth = settings.subagents.max_depth;
        let child_depth = link.as_ref().map_or(1, |link| link.depth + 1);
        let seeds = jobs.children().into_iter().map(|child| async move {
            let usage = subtree_usage(store, &child.path, child_depth, max_depth).await;
            (child.session_id, usage)
        });
        for (child, usage) in join_all(seeds).await {
            jobs.child_tree_usage(&child, usage);
        }
        let own_usage = entries_usage(&entries);
        let mut tree_usage = own_usage.clone();
        tree_usage += &jobs.children_tree_usage();
        inherited
            .sessions
            .register(session_id.clone(), Arc::downgrade(&jobs));
        let deferred = Arc::new(tools::deferred::Deferred::default());
        let (schemes, scheme_diagnostics, docs) = session_schemes(
            docs_dir.as_deref(),
            &inherited.harness_name,
            SchemeContext {
                session_id: session_id.clone(),
                session_dir: path.with_extension(""),
                cwd: cwd.clone(),
            },
            &joined_schemes,
            Some(LiveSchemes {
                jobs: jobs.clone(),
                sessions: inherited.sessions.clone(),
                store: inherited.store.clone(),
                deferred: deferred.clone(),
            }),
            definitions.clone(),
            inherited.docs,
        )?;
        let (summary_prompts, summary_diagnostics) =
            compaction::summary::SummaryPrompts::load(&config.prompts, &inherited.harness_name);
        let runner = crate::jobs::command::CommandRunner {
            jobs: jobs.clone(),
            artifacts: schemes.artifacts().clone(),
            supervisor: inherited.supervisor.clone(),
            ci: settings.bash.ci,
            promote_after: Duration::from_millis(settings.bash.promote_after_ms),
        };
        let session_tools = Arc::new(SessionTools::new(
            cwd.clone(),
            exec.clone(),
            NestedContext::disabled(),
            schemes,
            runner,
            deferred,
            crate::tools::SnapshotStore::for_session(&path),
        ));
        let yield_state = link
            .as_ref()
            .and_then(|link| link.spec.output.as_ref())
            .map(|_| Arc::new(YieldState::default()));
        let team = link.as_ref().and_then(|link| {
            link.team
                .as_ref()
                .map(|team| inherited.sessions.team(&link.parent_session_id, team))
        });
        let (host, host_deferred) = tools::instantiate(&joined_tools, &session_tools);
        let spawn_tool = Arc::new(SpawnTool::new(&resolved.agents, &settings.model_roles));
        let depth = link.as_ref().map_or(0, |link| link.depth);
        let routes = AgentRoutes {
            session_id: session_id.clone(),
            parent: link.as_ref().map(|link| link.parent_session_id.clone()),
            team: link.as_ref().and_then(|link| link.team.clone()),
        };
        let has_peers = routes.parent.is_some() || routes.team.is_some();
        let send = (depth < settings.subagents.max_depth || has_peers).then(|| {
            Arc::new(tools::send::SendTool::new(
                jobs.clone(),
                inherited.sessions.clone(),
                routes,
            )) as Arc<dyn Tool>
        });
        let builtin = tools::builtin(
            &session_tools,
            Some(spawn_tool.clone()),
            send,
            team.clone().map(|team| TeamSeat {
                team,
                member: session_id.clone(),
                sessions: inherited.sessions.clone(),
            }),
            inherited.ask_enabled,
            questions.clone(),
            settings.ask.timeout_ms.map(Duration::from_millis),
        );
        let yield_tool: Option<Arc<dyn Tool>> = link
            .as_ref()
            .and_then(|link| link.spec.output.as_ref())
            .zip(yield_state.as_ref())
            .map(|(output, state)| {
                Arc::new(YieldTool::new(output, state.clone())) as Arc<dyn Tool>
            });
        let enter_mode = link
            .is_none()
            .then(|| modes::EnterModeTool::new(&resolved.modes))
            .flatten()
            .map(Arc::new);
        let shape_base = ShapeBase {
            config: config.clone(),
            cwd: cwd.clone(),
            project,
            host_tool_factories: joined_tools,
            host_sections: layers.joined(|r| &r.sections),
            scheme_docs: session_tools.schemes.prompt_docs(),
            tool_base: Arc::new(ToolBase {
                builtin,
                host_tools: host,
                host_deferred,
                depth,
                max_depth,
                has_peers,
                call: Arc::new(tools::call::CallTool::new()),
                enter_mode: enter_mode
                    .as_ref()
                    .map(|tool| tool.clone() as Arc<dyn Tool>),
                reserve_mode_tools: link.is_none() && !resolved.modes.is_empty(),
                yield_tool,
                limits: link
                    .as_ref()
                    .map(|link| link.limits.clone())
                    .unwrap_or_default(),
            }),
        };
        let (prompt_shape, nested, context_diagnostics) = shape(&shape_base, root_agent);
        session_tools
            .context
            .reset(nested.root, nested.names, &nested.startup);
        let tools = prompt_shape.tools.clone();
        let selected_mode = if link.is_none() {
            projection.mode.as_ref().and_then(|name| {
                resolved
                    .modes
                    .iter()
                    .find(|spec| &spec.name == name)
                    .cloned()
            })
        } else {
            None
        };
        let mode_diagnostic = projection
            .mode
            .as_ref()
            .filter(|_| selected_mode.is_none())
            .map(|name| Diagnostic {
                origin: None,
                message: format!(
                    "mode \"{name}\" from the session is unavailable; running without it"
                ),
            });
        let diagnostics: Vec<_> = context_diagnostics
            .into_iter()
            .chain(prompt_shape.tool_diagnostics.iter().cloned())
            .chain(scheme_diagnostics)
            .chain(commands::reserved_skill_diagnostics(&resolved.skills))
            .chain(commands::hidden_skill_diagnostics(
                &resolved.skills,
                &resolved.modes,
            ))
            .chain(commands::registered_diagnostics(
                &joined_commands,
                &resolved.modes,
                &resolved.skills,
            ))
            .chain(summary_diagnostics)
            .chain(section_diagnostics)
            .chain(inherited.definition_diagnostics.iter().cloned())
            .chain(merge_diagnostics)
            .chain(mode_diagnostic)
            .collect();
        for diagnostic in &diagnostics {
            warn!("{diagnostic}");
        }

        let journal = Journal::start(writer, entries, projection.clone(), events.clone());
        let child = link.map(|link| ChildState {
            budget: Arc::new(TurnBudget::new(
                link.spec.max_turns.unwrap_or(settings.subagents.max_turns),
            )),
            output: yield_state,
            team,
            first_slot: Mutex::new(None),
            link,
        });
        let scope_agent = root_agent
            .cloned()
            .or_else(|| child.as_ref().map(|child| child.link.spec.clone()));
        let bash = effective_bash(
            scope_agent.as_ref(),
            selected_mode.as_deref(),
            child
                .as_ref()
                .map_or(BashAccess::Full, |child| child.link.bash),
        );
        session_tools
            .read_only_bash
            .store(!bash.is_full(), Ordering::Release);
        let writes = effective_writes(
            &session_tools,
            scope_agent.as_ref(),
            selected_mode.as_deref(),
            child.as_ref().and_then(|child| child.link.writes.as_ref()),
        );
        *session_tools
            .writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = writes.clone();
        let scope = Arc::new(Mutex::new(Arc::new(ToolScope {
            session_id: session_id.clone(),
            depth,
            agent: scope_agent,
            mode: selected_mode.clone(),
            bash,
            writes,
        })));
        let mut fixed_hooks: Vec<Arc<dyn ToolHooks>> = Vec::new();
        if let Some(child) = &child {
            fixed_hooks.push(child.budget.clone());
        }
        fixed_hooks.push(Arc::new(ModeGate {
            scope: scope.clone(),
        }));
        let tool_hooks = tool_hooks(&fixed_hooks, &scope, &joined_policies);
        let hooks = Arc::new_cyclic(|this: &Weak<SessionHooks>| {
            jobs.attach(this.clone() as Weak<dyn JobHost>);
            spawn_tool.attach(this.clone());
            if let Some(tool) = &enter_mode {
                tool.attach(this.clone());
            }
            SessionHooks {
                events: events.clone(),
                inherited: inherited.clone(),
                definitions: definitions.clone(),
                session_id: session_id.clone(),
                shape_base: Mutex::new(shape_base),
                promoted: Mutex::new(Vec::new()),
                mode: Mutex::new(modes::ModeState {
                    selected: selected_mode.clone(),
                    yield_tool: modes::ModeYield::for_mode(selected_mode.as_ref(), this.clone()),
                    effective: selected_mode,
                }),
                agent_spec: Mutex::new(root_agent.cloned()),
                scope,
                path,
                tools: session_tools.clone(),
                questions,
                results: modes::PendingResults::new(events.clone()),
                shutdown: AtomicBool::new(false),
                journal,
                live: Mutex::new(LiveRun::default()),
                selection: watch::Sender::new(Selection {
                    model: model.clone(),
                    thinking,
                    tier,
                }),
                activity: ActivityState::new(events.clone()),
                queue_order: tokio::sync::Mutex::new(()),
                quiet: Mutex::new(Vec::new()),
                selection_order: tokio::sync::Mutex::new(()),
                registry_model: Mutex::new(inherited.models.lookup(&model.provider, &model.id)),
                maintenance: Maintenance {
                    settings: settings.compaction.clone(),
                    prompts: summary_prompts,
                    cancel: Mutex::new(None),
                },
                stop_requested: AtomicBool::new(false),
                preparing: AtomicUsize::new(0),
                prompt: SessionPrompt {
                    sources: PromptSources {
                        delegation: child.is_some(),
                        team: child
                            .as_ref()
                            .and_then(|child| child.link.team.as_ref())
                            .map(team::section),
                        name: inherited.harness_name.clone(),
                        cwd: cwd.clone(),
                        docs,
                        definitions,
                    },
                    shape: Mutex::new(prompt_shape),
                    state: Mutex::new(PromptState {
                        note: None,
                        date: String::new(),
                        sections: None,
                        pending: Vec::new(),
                    }),
                },
                auto_paused: AtomicBool::new(false),
                agent: OnceLock::new(),
                this: this.clone(),
                own_usage: Mutex::new(own_usage),
                last_tree: Mutex::new(tree_usage),
                policies: Mutex::new(joined_policies),
                fixed_hooks,
                spawn_hooks: Mutex::new(layers.joined(|r| &r.spawn_hooks)),
                host_schemes: Mutex::new(joined_schemes),
                commands: Mutex::new(joined_commands),
                registrations: Mutex::new(layers),
                child,
            }
        });
        let agent = Arc::new(Agent::new(AgentConfig {
            thinking,
            service_tier: tier,
            tools,
            hooks: hooks.clone(),
            tool_hooks,
            persist: Some(hooks.clone()),
            steering_mode: settings.steering_mode,
            follow_up_mode: settings.follow_up_mode,
            session_id: Some(session_id.clone()),
            ..AgentConfig::new(inherited.streamer.clone(), model)
        }));
        let _ = hooks.agent.set(Arc::downgrade(&agent));
        hooks.rebase_promoted();
        hooks.install_tools();
        if hooks.child.is_some() {
            tokio::spawn(children::park_when_idle(
                Arc::downgrade(&hooks),
                hooks.activity.subscribe(),
                jobs.subscribe(),
            ));
            // Reseeded from the journal; the parent's row may hold an older total.
            let last = hooks
                .last_tree
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(parent) = hooks.parent_registry() {
                parent.child_tree_usage(&hooks.session_id, last.clone());
            }
        }

        let session = Session {
            agent,
            hooks,
            inherited,
            diagnostics,
        };
        let resolved = session
            .inherited
            .exec
            .resolve(
                &session.hooks.inherited.cwd,
                &CancellationToken::new(),
                &|_| {},
            )
            .await;
        session.hooks.prompt.refresh(resolved.prompt_note);
        session.hooks.prompt.take_pending();

        // Nothing else runs on the session yet: these writes go in order.
        session
            .hooks
            .record_selection(
                projection.model.as_deref(),
                projection.thinking,
                projection.tier,
                projection.agent.as_deref(),
            )
            .await?;
        session.hooks.sync_sections().await?;
        if recovery.is_some() {
            session.reconcile_jobs(unfinished).await?;
        }
        let resume = match recovery {
            Some(policy) => session.recover(policy).await?,
            None => None,
        };
        {
            // After recovery's results, so a replayed `write` finds the file it saw.
            let messages = &session.hooks.journal.view().projection.messages;
            session.hooks.tools.ledger.restore(messages);
            session.hooks.tools.context.restore(messages);
            session.agent.set_messages(messages.clone());
        }

        let agent = &session.agent;
        let request = match resume {
            None => None,
            Some(Resumption::RecoverResponse(request)) => Some(request),
            Some(Resumption::Continue(queued)) => {
                let has_turns = agent
                    .messages()
                    .iter()
                    .any(|message| !matches!(message, AgentMessage::Llm(Message::System(_))));
                // With no turn to continue, the requeued input starts the run instead.
                let mut first = Vec::new();
                for (kind, message) in queued {
                    if has_turns || kind == SubmissionKind::Quiet {
                        enqueue(agent, &session.hooks, kind, message);
                    } else {
                        first.push(message);
                    }
                }
                Some(if first.is_empty() {
                    RunRequest::Continue
                } else {
                    RunRequest::Prompt(first)
                })
            }
            Some(Resumption::Requeue { queued, wake }) => {
                let mut prompts = Vec::new();
                for (kind, message) in queued {
                    match kind {
                        SubmissionKind::Prompt | SubmissionKind::Mail if wake => {
                            prompts.push(message);
                        }
                        _ => enqueue(agent, &session.hooks, kind, message),
                    }
                }
                (!prompts.is_empty()).then_some(RunRequest::Prompt(prompts))
            }
        };
        if let Some(request) = request {
            let request = match request {
                RunRequest::Prompt(messages) => {
                    let mut input = session
                        .hooks
                        .run_input(session.hooks.reserve_delivery(), None);
                    input.extend(messages);
                    RunRequest::Prompt(input)
                }
                other => other,
            };
            let run = run_to_settle(session.agent.clone(), session.hooks.clone(), request);
            tokio::spawn(async move {
                if let Err(err) = run.await {
                    warn!(%err, "resumed run failed");
                }
            });
        }
        if recovery.is_some() {
            let reconciling = session.hooks.inherited.sessions.reconciling();
            let reconcile = children::reconcile_children(Arc::downgrade(&session.hooks));
            tokio::spawn(async move {
                reconcile.await;
                drop(reconciling);
            });
        }
        Ok(session)
    }

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

    /// The currently selected model.
    #[ext]
    pub fn model(&self) -> Model {
        self.hooks.model()
    }

    /// The thinking level applied to the current model.
    #[ext]
    pub fn thinking(&self) -> ThinkingLevel {
        self.hooks.selection().thinking
    }

    /// The chosen service tier; it applies only on models that support it.
    #[ext]
    pub fn service_tier(&self) -> ServiceTier {
        self.hooks.selection.borrow().tier
    }

    /// The tier requests are sent with on the current model.
    #[ext]
    pub fn effective_service_tier(&self) -> ServiceTier {
        let selection = self.hooks.selection.borrow();
        selection.model.effective_tier(selection.tier)
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

    /// Replaces the host's registered commands; returns the diagnostics of every registered
    /// command, layer included (invalid, hidden or replaced names, hidden skills). Hosts pass
    /// commands again on reopen.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub fn set_commands(&self, commands: Vec<RegisteredCommand>) -> Result<Vec<Diagnostic>, Error> {
        self.reject_child()?;
        let mut layers = self.hooks.registrations();
        layers.host.commands = commands;
        Ok(self.hooks.install_commands(&layers))
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
                let choice = self
                    .inherited
                    .config
                    .settings
                    .resolve_model(&self.inherited.models, &model)?;
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

    /// Switches the model from the next request; the thinking level is clamped to it.
    #[ext]
    pub async fn set_model(&self, model: Model) -> Result<(), Error> {
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move { hooks.select_model(&agent, model, None, None).await }).await
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
                .map(|selector| {
                    hooks
                        .inherited
                        .config
                        .settings
                        .resolve_model(&hooks.inherited.models, selector)
                })
                .transpose()?;
            let mut entries = vec![Entry::AgentChange {
                base: journal::base(),
                agent: spec.as_ref().map(|spec| spec.name.clone()),
            }];
            let selection = choice.map(|choice| {
                let current = hooks.selection();
                let model = choice.model.unwrap_or_else(|| current.model.clone());
                let (model_entries, level, tier) =
                    hooks.model_entries(&current, &model, choice.thinking, choice.tier);
                entries.extend(model_entries);
                (model, level, tier)
            });
            hooks.append(entries).await?;
            if let Some((model, level, tier)) = selection {
                let definition = hooks.registry_definition(&model);
                hooks.apply_selection(&agent, model, level, tier, definition);
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
                .reset(nested.root, nested.names, &nested.startup);
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

    /// Changes a host section from the next request. Hosts pass sections again on reopen.
    /// This is cache-heavy on models without mid-conversation system messages.
    #[ext(skip = "edits the host's own registrations; extensions register through their manifest")]
    pub async fn set_section(&self, name: &str, body: Option<String>) -> Result<(), Error> {
        self.reject_child()?;
        check_section_name(name).map_err(Error::InvalidInput)?;
        let _order = self.hooks.selection_order.lock().await;
        let sections = {
            let mut layers = self.hooks.registrations();
            let sections = &mut layers.host.sections;
            if let Some((_, value)) = sections.iter_mut().rfind(|(key, _)| key == name) {
                *value = body;
            } else {
                sections.push((name.to_owned(), body));
            }
            layers.joined(|r| &r.sections)
        };
        self.hooks.stage_sections(sections);
        self.hooks.prompt.rebuild();
        Ok(())
    }

    /// Replaces every host section from the next request; an invalid name rejects the
    /// whole set. Hosts pass sections again on reopen.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub async fn set_sections(&self, sections: Vec<(String, Option<String>)>) -> Result<(), Error> {
        self.reject_child()?;
        check_sections(&sections)?;
        let _order = self.hooks.selection_order.lock().await;
        let sections = {
            let mut layers = self.hooks.registrations();
            layers.host.sections = sections;
            layers.joined(|r| &r.sections)
        };
        self.hooks.stage_sections(sections);
        self.hooks.prompt.rebuild();
        Ok(())
    }

    /// Replaces host tool policies for hook invocations that start after this returns.
    /// Live children keep theirs.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub fn set_tool_policies(&self, policies: Vec<Arc<dyn ToolPolicy>>) -> Result<(), Error> {
        self.reject_child()?;
        let mut layers = self.hooks.registrations();
        layers.host.tool_policies = policies;
        self.hooks
            .install_policies(&self.agent, layers.joined(|r| &r.tool_policies));
        Ok(())
    }

    /// Replaces host spawn hooks for spawns resolved after this returns.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub fn set_spawn_hooks(&self, hooks: Vec<Arc<dyn SpawnHook>>) -> Result<(), Error> {
        self.reject_child()?;
        let mut layers = self.hooks.registrations();
        layers.host.spawn_hooks = hooks;
        *self
            .hooks
            .spawn_hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = layers.joined(|r| &r.spawn_hooks);
        Ok(())
    }

    /// Replaces host schemes. URLs resolve through the new set at once; the prompt's
    /// schemes section changes from the next request. An invalid or reserved name rejects
    /// the whole set and keeps the previous one.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub async fn set_schemes(&self, schemes: Vec<Arc<dyn Scheme>>) -> Result<(), Error> {
        self.reject_child()?;
        let hooks = &self.hooks;
        let _order = hooks.selection_order.lock().await;
        let joined = schemes
            .iter()
            .chain(&hooks.registrations().layer.schemes)
            .cloned()
            .collect();
        hooks.stage_schemes(joined)?;
        hooks.registrations().host.schemes = schemes;
        hooks.prompt.rebuild();
        Ok(())
    }

    fn reject_child(&self) -> Result<(), Error> {
        if self.hooks.child.is_some() {
            return Err(Error::InvalidInput(
                "This operation is only available on root sessions".into(),
            ));
        }
        Ok(())
    }

    /// Replaces host tool factories from the next request, still honoring the persona's
    /// allowlist; returns the new tool shape's diagnostics. Hosts pass tools again on
    /// reopen. Changes are cache-heavy on models without mid-conversation tool changes.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub async fn set_tools(&self, tools: Vec<HostTool>) -> Result<Vec<Diagnostic>, Error> {
        self.reject_child()?;
        let hooks = self.hooks.clone();
        detached(async move {
            let _order = hooks.selection_order.lock().await;
            let tools = {
                let mut layers = hooks.registrations();
                layers.host.tools = tools;
                layers.joined(|r| &r.tools)
            };
            let diagnostics = hooks.stage_tools(tools);
            hooks.prompt.rebuild();
            Ok(diagnostics)
        })
        .await
    }

    /// Replaces the host's code-supplied definitions from the next request. File definitions
    /// and the layer's keep their precedence. The selected persona and mode keep the copy they
    /// were selected with. Returns merge, skill and command diagnostics, then the tool shape's.
    /// Hosts pass definitions again on reopen.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub async fn set_definitions(
        &self,
        definitions: Definitions,
    ) -> Result<Vec<Diagnostic>, Error> {
        self.reject_child()?;
        let hooks = self.hooks.clone();
        detached(async move {
            let _order = hooks.selection_order.lock().await;
            hooks.registrations().host.definitions = definitions;
            let (changed, mut diagnostics) = hooks.stage_definitions();
            let defs = hooks.definitions.get();
            diagnostics.extend(commands::registered_diagnostics(
                &hooks.commands(),
                &defs.modes,
                &defs.skills,
            ));
            if changed {
                diagnostics.extend(hooks.restage());
                hooks.prompt.rebuild();
            } else {
                diagnostics.extend(
                    hooks
                        .prompt
                        .shape
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .tool_diagnostics
                        .iter()
                        .cloned(),
                );
            }
            Ok(diagnostics)
        })
        .await
    }

    /// Replaces the layer over the host's registrations, and the layer's definitions, in one
    /// pass. An invalid section name or an invalid or reserved scheme name rejects the whole
    /// layer and changes nothing. Live children keep what they were spawned with. Returns the
    /// new tool shape's diagnostics, then definition diagnostics, then those of every
    /// registered command.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub async fn set_layer(&self, layer: Registrations) -> Result<Vec<Diagnostic>, Error> {
        self.reject_child()?;
        check_sections(&layer.sections)?;
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move {
            let _order = hooks.selection_order.lock().await;
            // The only fallible stage goes first, so an error changes nothing.
            let schemes = hooks
                .registrations()
                .host
                .schemes
                .iter()
                .chain(&layer.schemes)
                .cloned()
                .collect();
            hooks.stage_schemes(schemes)?;
            hooks.registrations().layer = layer;
            let (_, definition_diagnostics) = hooks.stage_definitions();
            let (sections, tools, command_diagnostics) = {
                let layers = hooks.registrations();
                let command_diagnostics = hooks.apply_registrations(&agent, &layers);
                (
                    layers.joined(|r| &r.sections),
                    layers.joined(|r| &r.tools),
                    command_diagnostics,
                )
            };
            hooks.stage_sections(sections);
            let mut diagnostics = hooks.stage_tools(tools);
            hooks.prompt.rebuild();
            diagnostics.extend(definition_diagnostics);
            diagnostics.extend(command_diagnostics);
            Ok::<_, Error>(diagnostics)
        })
        .await
    }

    /// Sets the thinking level from the next request, clamped to the model; returns the
    /// level applied.
    #[ext]
    pub async fn set_thinking(&self, level: ThinkingLevel) -> Result<ThinkingLevel, Error> {
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move {
            hooks
                .select_suffixes(&agent, Some(level), None)
                .await
                .map(|s| s.0)
        })
        .await
    }

    /// Chooses the service tier from the next request; returns the tier the current model applies.
    #[ext]
    pub async fn set_service_tier(&self, tier: ServiceTier) -> Result<ServiceTier, Error> {
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move {
            hooks
                .select_suffixes(&agent, None, Some(tier))
                .await
                .map(|s| s.1)
        })
        .await
    }

    /// Settles the pending question of tool call `call_id`. A submission that does not fit
    /// the document returns `Error::InvalidInput` and leaves the question pending.
    #[ext]
    pub fn answer(&self, call_id: &str, answers: Vec<Answer>) -> Result<(), Error> {
        self.hooks.questions.answer(call_id, answers)
    }

    /// Dismisses a pending question or mode result; the run is cancelled.
    #[ext]
    pub fn dismiss(&self, call_id: &str) -> Result<(), Error> {
        if let Err(error) = self.hooks.questions.dismiss(call_id) {
            self.hooks
                .results
                .settle(call_id, modes::ResultOutcome::Dismissed)
                .map_err(|_| error)?;
        }
        self.abort();
        Ok(())
    }

    /// The current estimated context size and effective compaction limits.
    #[ext]
    pub fn context_usage(&self) -> Option<ContextUsage> {
        self.hooks.context_usage()
    }

    /// Whether a run, maintenance or navigation currently owns the session.
    /// Observing idle does not reserve it against future submissions or background wakes.
    #[ext]
    pub fn is_running(&self) -> bool {
        self.hooks.activity.borrow().running
    }

    /// This session's own recorded spend plus every descendant's, maintenance included;
    /// `SessionEvent::TreeUsage` reports changes.
    #[ext]
    pub fn tree_usage(&self) -> Usage {
        self.hooks.tree_usage()
    }

    /// Events from now on, without the snapshot `attach` takes.
    #[ext(skip = "live receiver; the bridge serves session.subscribe")]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<SessionEvent>> {
        self.hooks.events.subscribe()
    }

    /// The entries after `from` (all with `None`), the live state and an event receiver,
    /// all taken at one point of the append order: no entry is missed or duplicated.
    /// Never waits on a write in flight.
    #[ext(skip = "returns a live receiver; use entries, live and subscribe")]
    pub async fn attach(&self, from: Option<u64>) -> Attach {
        // Entries publish under the journal lock, live updates relay under the live lock,
        // job rows announce under the registry lock and tree totals emit under `last_tree`:
        // subscribing inside all four splits every event into before (in the snapshot) or
        // after (received).
        let activity = self.hooks.activity.borrow();
        self.hooks.journal.read(|view| {
            let live = self.hooks.live();
            let mode = self
                .hooks
                .mode
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let queued = queued_inputs(&view.entries, self.agent.queued_ids());
            let _tree = self
                .hooks
                .last_tree
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let tree_usage = self.hooks.tree_usage();
            let (jobs, events) = self
                .hooks
                .tools
                .runner
                .jobs
                .announced_with(|| self.hooks.events.subscribe());
            let start = from.map_or(0, |seq| {
                view.entries.partition_point(|entry| entry.seq <= seq)
            });
            let selection = self.hooks.selection();
            Attach {
                missed: view.entries[start..].to_vec(),
                live: LiveSnapshot {
                    run_id: live.run_id.clone(),
                    maintenance: activity.maintenance,
                    running: activity.running,
                    model: selection.model,
                    thinking: selection.thinking,
                    service_tier: selection.tier,
                    agent: self.agent().map(|spec| spec.name),
                    mode: mode.selected.as_ref().map(|mode| mode.name.clone()),
                    effective_mode: mode.effective.as_ref().map(|mode| mode.name.clone()),
                    context: self.hooks.context_usage_for(&view.projection),
                    auto_paused: self.hooks.auto_paused.load(Ordering::Acquire),
                    streaming: live.streaming.as_ref().and_then(Accumulator::snapshot),
                    pending_tool_calls: live.tool_calls.clone(),
                    queued,
                    messages: view.projection.messages.clone(),
                    pending_questions: self.hooks.questions.pending(),
                    pending_results: self.hooks.results.snapshot(),
                    jobs,
                    usage: entries_usage(&view.entries),
                    tree_usage,
                },
                events,
            }
        })
    }

    /// Copies the root→`entry_id` path into a new session file that records this one as
    /// its parent. The fork runs with this session's resolved configuration. Forking at a
    /// user message copies the path to its parent and returns the message's parts to edit
    /// and resubmit. A live fork shares this session's child-run slots; a fork of a child
    /// also keeps its inherited host sections (with `ask` disabled). Reopening the fork with
    /// `Axo::open_session` resolves it as an independent root.
    #[ext(skip = "lifecycle belongs to the host")]
    pub async fn fork(&self, entry_id: &EntryId) -> Result<Fork, Error> {
        let Selection {
            model,
            thinking,
            tier,
        } = self.hooks.selection();
        let (path, editable) = {
            let log = self.hooks.journal.view();
            let index: HashMap<&str, &SeqEntry> = log
                .entries
                .iter()
                .filter_map(|seq| seq.entry.id().map(|id| (id, seq)))
                .collect();
            let target = index
                .get(entry_id.as_str())
                .ok_or_else(|| Error::EntryNotFound(entry_id.to_string()))?;
            let (next, editable) = match &target.entry {
                Entry::Message {
                    message: AgentMessage::Llm(Message::User(user)),
                    base,
                    submission_id,
                    ..
                } => {
                    let submitted = submission_id.as_deref().and_then(|wanted| {
                        log.entries.iter().find_map(|seq| match &seq.entry {
                            Entry::Submission {
                                submission_id,
                                parts,
                                ..
                            } if submission_id == wanted => Some(parts.clone()),
                            _ => None,
                        })
                    });
                    // Messages without a submission (older sessions) offer their text.
                    let parts = submitted.unwrap_or_else(|| {
                        vec![PromptPart::Text {
                            text: user.content.iter().filter_map(UserBlock::as_text).collect(),
                        }]
                    });
                    (base.parent_id.as_ref().map(EntryId::as_str), Some(parts))
                }
                entry => (entry.id(), None),
            };
            let path = next.map_or_else(Vec::new, |id| {
                store::path_to(&log.entries, id)
                    .into_iter()
                    .map(|index| log.entries[index].entry.clone())
                    .collect()
            });
            (path, editable)
        };

        let header = Header::new(
            self.hooks.inherited.cwd.clone(),
            Some(self.hooks.path.clone()),
            Some(entry_id.clone()),
        );
        let session_id = header.id.clone();
        let mut writer = self.inherited.store.create(header).await?;
        let parent_dir = self.hooks.path.with_extension("");
        let child_dir = writer.path().with_extension("");
        // The blobs the fork's path references: its file changes stay reviewable.
        let snapshots = path
            .iter()
            .flat_map(|entry| match entry {
                Entry::Message {
                    message: AgentMessage::Llm(Message::ToolResult(result)),
                    ..
                } => tools::file_changes(result),
                _ => Vec::new(),
            })
            .flat_map(|change| change.before.into_iter().chain([change.after]))
            .filter(|blob| blob.omitted.is_none())
            .map(|blob| blob.sha256)
            .collect();
        copy_for_fork(
            &parent_dir,
            &child_dir,
            self.hooks.tools.schemes.artifacts().live(),
            snapshots,
        )
        .await;
        let mut entries = Vec::with_capacity(path.len() + 1);
        for entry in path {
            entries.push(writer.append(entry).await?);
        }
        // A run open at the fork point continues nowhere: close it so opening the fork
        // does not try to recover it.
        let fork_projection = project(&entries, None);
        let open = match plan_recovery(&entries, &fork_projection, &model, &|_| Replay::Never) {
            RecoveryPlan::None | RecoveryPlan::Requeue { .. } => None,
            RecoveryPlan::Finish {
                run_id,
                outcome,
                reason,
                ..
            } => Some((run_id, outcome, reason)),
            RecoveryPlan::Park { run_id, .. }
            | RecoveryPlan::Resume { run_id, .. }
            | RecoveryPlan::RecoverResponse { run_id, .. } => Some((
                run_id,
                store::RunOutcome::Interrupted,
                Some("fork".to_owned()),
            )),
        };
        if let Some((run_id, outcome, reason)) = open {
            let parent = entries
                .last()
                .and_then(|last| last.entry.id())
                .map(|id| EntryId(id.to_owned()));
            let id = EntryId::generate(|id| entries.iter().any(|seq| seq.entry.id() == Some(id)));
            entries.push(
                writer
                    .append(Entry::RunEnd {
                        base: EntryBase::new(id, parent),
                        usage: Some(store::run_usage(&entries, &run_id)),
                        run_id,
                        outcome,
                        reason,
                        queued: Vec::new(),
                    })
                    .await?,
            );
        }

        let projection = project(&entries, None);
        let mut inherited = (*self.inherited).clone();
        inherited.agent = self.agent();
        let layers = self.hooks.registrations().clone();
        let session = Session::start(SessionSetup {
            inherited: Arc::new(inherited),
            layers,
            merged: None,
            model,
            thinking,
            tier,
            session_id,
            writer,
            entries,
            projection,
            recovery: None,
            link: None,
        })
        .await?;
        Ok(Fork { session, editable })
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
