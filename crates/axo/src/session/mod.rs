//! A conversation with one agent in one working directory, saved as it happens.

pub(crate) mod children;
mod delivery;
pub(crate) mod journal;
mod maintenance;
mod prompt;
mod resume;
mod side_files;
pub(crate) mod spawn;
pub(crate) mod team;

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
    Accumulator, AssistantMessage, Message, Model, Models, StopReason, Streamer, ThinkingLevel,
    ToolCall, ToolResultMessage, Usage, UserBlock,
};
use futures::FutureExt;
use serde_json::Value;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use self::children::{ChildState, SessionTable, TurnBudget};
use self::delivery::{Activity, enqueue};
use self::journal::Journal;
use self::maintenance::Maintenance;
use self::prompt::{PromptState, SessionPrompt, ShapeBase, shape};
use self::resume::Resumption;
use self::side_files::{ARTIFACTS_DIR, LOCAL_DIR, copy_for_fork};
use self::spawn::SpawnTool;
pub use self::spawn::{
    ChildHandle, InlineSpec, SpawnContext, SpawnDecision, SpawnHook, SpawnOrigin, SpawnRequest,
    SpawnTeam, TeamHandle, TeamMemberRequest, TeamRequest,
};
use self::team::TeamSeat;
use crate::commands::{self, Command, CommandInfo};
use crate::compaction::{self, Limits};
use crate::config::{EffectiveConfig, ProjectAccess};
use crate::context::agents::{AgentDef, AgentSpec, RunAs};
use crate::context::files::NestedContext;
use crate::context::prompt::PromptSources;
use crate::context::skills::{Skill, load_skills};
use crate::context::{Diagnostic, DiagnosticOrigin, home_agents_dir};
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
use crate::tools::{self, Answer, PendingQuestions, Question, SessionTools, ToolFactory};

pub struct SessionOptions {
    /// Ignored by `Axo::open_session`, which uses the cwd in the session header.
    pub cwd: PathBuf,
    /// A model selector (`provider/model`, `@role`, either with `:thinking`, or `:thinking` alone);
    /// falls back to the session's last model, then the root agent's `model`, then `defaultModel`.
    pub model: Option<String>,
    /// The root agent by name. `None` keeps the session's recorded agent (none for a new
    /// session); `Some` selects it, recording a change on reopen.
    pub agent: Option<String>,
    /// Agent definitions supplied in code, after every file-defined one; the first name wins.
    pub agents: Vec<AgentSpec>,
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
    /// Host prompt sections, applied last; `None` removes a section.
    pub sections: Vec<(String, Option<String>)>,
    /// Host tools, built per session (and per fork) from its `SessionTools`; one named like a built-in replaces it.
    pub tools: Vec<ToolFactory>,
    /// Ordered host tool policies, inherited by forks.
    pub tool_hooks: Vec<Arc<dyn ToolHooks>>,
    /// Ordered spawn policies; children inherit them.
    pub spawn_hooks: Vec<Arc<dyn SpawnHook>>,
    /// Host schemes; one named like a built-in replaces it (artifact and file are reserved).
    pub schemes: Vec<Arc<dyn Scheme>>,
    /// What `Axo::open_session` does with a run left unfinished.
    pub recovery: RecoveryPolicy,
}

impl Default for SessionOptions {
    /// `cwd` is the process's current directory (empty if it cannot be read).
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_default(),
            model: None,
            agent: None,
            agents: Vec::new(),
            thinking: None,
            ask_enabled: false,
            project_access: None,
            exec_env: None,
            sections: Vec::new(),
            tools: Vec::new(),
            tool_hooks: Vec::new(),
            spawn_hooks: Vec::new(),
            schemes: Vec::new(),
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
)]
#[serde(rename_all = "lowercase")]
pub enum ShakeMode {
    #[default]
    Elide,
    Images,
    Thinking,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceKind {
    Compact,
    Shake,
    BranchSummary,
}

// Boxing `Agent` would not save anything: the broadcast clones the value per receiver.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Agent(AgentEvent),
    /// An entry was appended to the session.
    Entry(SeqEntry),
    /// An `ask` call is waiting for `Session::answer` or `Session::dismiss`.
    QuestionPending {
        call_id: String,
        questions: Vec<Question>,
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
}

/// A prompt with an optional client id that makes resubmission idempotent.
#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub input: PromptInput,
    pub request_id: Option<String>,
}

/// What `Session::prompt_with` did.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub struct HostMessage {
    pub custom_type: String,
    pub text: String,
    pub display: bool,
    pub delivery: HostDelivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostDelivery {
    /// The next turn boundary; starts a run when idle.
    Steer,
    /// When the active run would end; starts a run when idle.
    FollowUp,
    /// The next turn boundary or run start; never starts a run.
    Quiet,
}

/// Estimated context size and effective compaction limits, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub events: broadcast::Receiver<SessionEvent>,
}

#[derive(Debug, Clone)]
pub struct LiveSnapshot {
    /// The open run, if any.
    pub run_id: Option<String>,
    pub maintenance: Option<MaintenanceKind>,
    pub running: bool,
    pub streaming: Option<AssistantMessage>,
    pub pending_tool_calls: Vec<ToolCall>,
    pub queued_steering: Vec<AgentMessage>,
    pub queued_follow_up: Vec<AgentMessage>,
    /// The projected conversation at this point in the append order, from the journal's view.
    pub messages: Vec<AgentMessage>,
    /// Unanswered `ask` calls by tool-call id. A question raised while attaching can
    /// also arrive as a `QuestionPending` event (duplicated, never missed).
    pub pending_questions: Vec<(String, Vec<Question>)>,
    /// The background jobs, as [`Session::jobs`] returns them.
    pub jobs: Vec<JobInfo>,
    /// Recorded spend, including maintenance and failed assistant requests, without run/job rollups.
    pub usage: Usage,
    pub model: Model,
    pub thinking: ThinkingLevel,
    pub agent: Option<String>,
    pub context: Option<ContextUsage>,
    pub auto_paused: bool,
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
    pub(crate) agents: Vec<AgentDef>,
    pub(crate) agent_diagnostics: Vec<Diagnostic>,
    pub(crate) agent: Option<AgentSpec>,
    pub(crate) streamer: Arc<dyn Streamer>,
    /// Where `/model` looks up models.
    pub(crate) models: Arc<Models>,
    pub(crate) exec: Arc<dyn ExecEnv>,
    /// Runs every local command in its own supervised container (process group or cgroup).
    pub(crate) supervisor: Arc<ProcessSupervisor>,
    pub(crate) ask_enabled: bool,
    pub(crate) host_sections: Vec<(String, Option<String>)>,
    pub(crate) host_tools: Vec<ToolFactory>,
    pub(crate) host_schemes: Vec<Arc<dyn Scheme>>,
    pub(crate) tool_hooks: Vec<Arc<dyn ToolHooks>>,
    pub(crate) store: Arc<dyn SessionStore>,
    pub(crate) sessions: Arc<SessionTable>,
    pub(crate) spawn_hooks: Vec<Arc<dyn SpawnHook>>,
    /// Child runs streaming at once under one root session and its live forks.
    pub(crate) run_slots: Arc<tokio::sync::Semaphore>,
    /// `Some` for children: `session_schemes` skips skill discovery and docs materialization.
    pub(crate) loaded: Option<Loaded>,
}

/// Skills and the docs flag a child takes from its parent instead of loading them again.
#[derive(Clone)]
pub(crate) struct Loaded {
    pub(crate) skills: Vec<Skill>,
    pub(crate) docs: bool,
}

/// Everything `Axo` resolved for a new, opened or forked session.
pub(crate) struct SessionSetup {
    pub(crate) inherited: Arc<Inherited>,
    pub(crate) model: Model,
    /// Already clamped to the model.
    pub(crate) thinking: ThinkingLevel,
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
    events: broadcast::Sender<SessionEvent>,
    inherited: Arc<Inherited>,
    child: Option<ChildState>,
    /// The header id and lazily created session file.
    session_id: String,
    path: PathBuf,
    /// Shared tool state, restored as the selected branch changes.
    tools: Arc<SessionTools>,
    questions: Arc<PendingQuestions>,
    shutdown: AtomicBool,
    /// Owns the session file: every entry is appended through it, and every reader takes
    /// its snapshots.
    journal: Journal,
    /// The open run's live state; `persist` updates it without awaiting.
    live: Mutex<LiveRun>,
    /// The live model and thinking level.
    selection: watch::Sender<Selection>,
    /// Who owns the session; `shutdown` waits on it going idle.
    activity: watch::Sender<Activity>,
    /// Held across a steer or follow-up's write, so the agent queues them in journal order.
    queue_order: tokio::sync::Mutex<()>,
    /// Durable host messages held until a run's next delivery boundary.
    quiet: Mutex<Vec<QueuedMessage>>,
    /// Held across a selection change's write and its apply, so changes take effect in
    /// journal order and only once durable.
    selection_order: tokio::sync::Mutex<()>,
    maintenance: Maintenance,
    /// Prompts between their fast-path admission and their run start; idle-wake delivery
    /// holds off meanwhile, so pending results ride the prompt instead of making it busy.
    preparing: AtomicUsize,
    prompt: SessionPrompt,
    shape_base: Mutex<ShapeBase>,
    agent_spec: Mutex<Option<AgentSpec>>,
    /// Set when a run ends other than `Done` or the user stops an active one; finished
    /// job results then wait for the user's next input instead of starting a run.
    auto_paused: AtomicBool,
    /// Set by a Stop while the session is claimed; the next run start honors it, so a Stop
    /// before the run begins is not lost.
    stop_requested: AtomicBool,
    /// Set once in `Session::start`; weak, since the agent holds these hooks.
    agent: OnceLock<Weak<Agent>>,
    /// These hooks, for tasks that deliver job results.
    this: Weak<SessionHooks>,
}

/// The open run's live state, updated and relayed under the same lock.
#[derive(Default)]
struct LiveRun {
    /// The open run's id, set by its `run_start` (or by recovery).
    run_id: Option<String>,
    streaming: Option<Accumulator>,
    tool_calls: Vec<ToolCall>,
}

/// The live model and thinking level; `model_change` / `thinking_level_change` entries
/// record each change in the order it took effect.
#[derive(Clone)]
struct Selection {
    model: Model,
    thinking: ThinkingLevel,
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

impl SessionHooks {
    fn live(&self) -> MutexGuard<'_, LiveRun> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keep live state and its event on the same side of an attach snapshot.
    fn relay_live(&self, event: &AgentEvent, update: impl FnOnce(&mut LiveRun)) {
        let mut live = self.live();
        update(&mut live);
        let _ = self.events.send(SessionEvent::Agent(event.clone()));
    }

    fn selection(&self) -> Selection {
        self.selection.borrow().clone()
    }

    /// What a child of this session reuses instead of loading again.
    pub(super) fn loaded(&self) -> Loaded {
        Loaded {
            skills: self.prompt.sources.skills.clone(),
            docs: self.prompt.sources.docs,
        }
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
    /// transcript.
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
                let _ = self.events.send(SessionEvent::Context(usage));
            }
        });
    }

    fn set_auto_paused(&self, paused: bool) {
        let _live = self.live();
        if self.auto_paused.swap(paused, Ordering::AcqRel) != paused {
            let _ = self.events.send(SessionEvent::AutoPaused { paused });
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
    /// admissions the run never committed are released first; a run that did not end
    /// `Done`, or a failed `run_end`, pauses automatic delivery until the user acts.
    async fn settle(
        &self,
        result: Result<RunOutcome, axo_agent::Error>,
    ) -> Result<RunOutcome, Error> {
        self.tools.runner.jobs.release_uncommitted();
        if !matches!(result, Ok(RunOutcome::Done)) {
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
                closed = self
                    .append(vec![Entry::RunEnd {
                        base: journal::base(),
                        run_id: run_id.clone(),
                        outcome,
                        reason,
                        usage: Some(store::run_usage(&self.journal.view().entries, &run_id)),
                    }])
                    .await
                    .map(drop);
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

    /// Appends `model_change` / `thinking_level_change` when the current branch records a
    /// different selection than the live one (pi `_recordSelection`), so reopening
    /// restores what is live.
    async fn record_selection(
        &self,
        model: Option<&str>,
        thinking: Option<ThinkingLevel>,
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
            });
        }
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
        if !entries.is_empty() {
            self.append(entries).await?;
        }
        Ok(())
    }

    /// Records `model` (and the thinking level it clamps to), then applies it once written.
    async fn select_model(
        &self,
        agent: &Agent,
        model: Model,
        thinking: Option<ThinkingLevel>,
    ) -> Result<(), Error> {
        let _order = self.selection_order.lock().await;
        let current = self.selection();
        let (entries, level) = self.model_entries(&current, &model, thinking);
        self.append(entries).await?;
        self.apply_selection(agent, model, level);
        Ok(())
    }

    fn model_entries(
        &self,
        current: &Selection,
        model: &Model,
        thinking: Option<ThinkingLevel>,
    ) -> (Vec<Entry>, ThinkingLevel) {
        let level = clamp_level(model, thinking.unwrap_or(current.thinking));
        let mut entries = vec![Entry::ModelChange {
            base: journal::base(),
            provider: model.provider.clone(),
            model_id: model.id.clone(),
        }];
        if level != current.thinking {
            entries.push(Entry::ThinkingLevelChange {
                base: journal::base(),
                thinking_level: level,
            });
        }
        (entries, level)
    }

    fn apply_selection(&self, agent: &Agent, model: Model, level: ThinkingLevel) {
        if level != self.selection().thinking {
            agent.set_thinking(level);
        }
        agent.set_model(model.clone());
        self.selection.send_replace(Selection {
            model,
            thinking: level,
        });
        self.emit_context();
    }

    /// Records `level`, clamped to the model, then applies it once written; returns the
    /// level applied.
    async fn select_thinking(
        &self,
        agent: &Agent,
        level: ThinkingLevel,
    ) -> Result<ThinkingLevel, Error> {
        let _order = self.selection_order.lock().await;
        let current = self.selection();
        let applied = clamp_level(&current.model, level);
        if applied == current.thinking {
            return Ok(applied);
        }
        self.append(vec![Entry::ThinkingLevelChange {
            base: journal::base(),
            thinking_level: applied,
        }])
        .await?;
        agent.set_thinking(applied);
        self.selection
            .send_modify(|selection| selection.thinking = applied);
        Ok(applied)
    }

    /// [`SessionHooks::record_selection`] against the current position.
    async fn record_current_selection(&self) -> Result<(), Error> {
        let (model, thinking, agent) = {
            let view = self.journal.view();
            (
                view.projection.model.clone(),
                view.projection.thinking,
                view.projection.agent.clone(),
            )
        };
        self.record_selection(model.as_deref(), thinking, agent.as_deref())
            .await
    }

    fn yield_nudge(&self) -> bool {
        self.child
            .as_ref()
            .and_then(|child| child.output.as_ref())
            .is_some_and(|output| output.nudge())
    }
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
        if self
            .automatic(CompactionReason::Threshold, cancel)
            .await
            .map_err(|error| HookError(error.to_string()))?
        {
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
                    let _ = self.events.send(SessionEvent::Retry {
                        attempt: retries + 1,
                        delay_ms: delay.as_millis() as u64,
                    });
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
                self.live().run_id = Some(run_id);
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
                let run_id = self.live().run_id.clone();
                if matches!(message, AgentMessage::Llm(Message::Assistant(_)))
                    && let Some(child) = &self.child
                    && let Some(parent) = self
                        .inherited
                        .sessions
                        .registry(&child.link.parent_session_id)
                    && let Some(run_id) = run_id
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
                            usage: store::run_usage(&self.journal.view().entries, &run_id),
                        },
                    );
                }
            }
            AgentEvent::ToolExecutionStart {
                call_id,
                name,
                args,
            } => {
                self.relay_live(event, |live| {
                    live.tool_calls.push(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: args.clone(),
                        arguments_error: None,
                    });
                });
                return Ok(());
            }
            AgentEvent::ToolExecutionEnd { call_id, .. } => {
                self.relay_live(event, |live| {
                    live.tool_calls.retain(|call| &call.id != call_id);
                });
                return Ok(());
            }
            AgentEvent::AgentEnd { .. } => {
                self.relay_live(event, |live| {
                    live.streaming = None;
                    live.tool_calls.clear();
                });
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
        let _ = self.events.send(SessionEvent::Agent(event.clone()));
        Ok(())
    }
}

/// Where `write agent://parent` and `write agent://<teammate>` go from a session.
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
    pub(crate) routes: AgentRoutes,
}

/// Loads the skills a session in `cwd` sees and builds its file-backed schemes over them:
/// `artifact://` and `local://` in the side folders beside the session file `path`,
/// `skill://`, `axo://` docs when `docs_dir` is given, `proc://` and `agent://` when
/// `live` is given, then `host` schemes. Open sessions and `Axo::schemes` both build
/// them here, so given the same effective config a file-backed URL resolves alike
/// whether its session is open or not; only open sessions get `proc://` and
/// `agent://` (`Axo::schemes` passes no live state). A child passes its parent's
/// `loaded` skills and docs flag, which skips discovery and docs materialization.
pub(crate) fn session_schemes(
    config: &EffectiveConfig,
    docs_dir: Option<&Path>,
    name: &str,
    cx: SchemeContext,
    host: &[Arc<dyn Scheme>],
    live: Option<LiveSchemes>,
    loaded: Option<&Loaded>,
) -> Result<(Schemes, Vec<Skill>, Vec<Diagnostic>, bool), Error> {
    let (skills, mut diagnostics) = match loaded {
        Some(loaded) => (loaded.skills.clone(), Vec::new()),
        None => {
            let settings = &config.settings;
            let home_agents = settings.home_agents_dir.then(home_agents_dir).flatten();
            load_skills(
                &config.skill_roots,
                home_agents.as_deref(),
                &cx.cwd,
                config.access != ProjectAccess::Ignore && settings.project.files.skills,
            )
        }
    };
    let mut builtins: Vec<Arc<dyn Scheme>> = vec![
        Arc::new(SkillScheme::new(&skills)),
        Arc::new(DirScheme::local(cx.session_dir.join(LOCAL_DIR))),
    ];
    let docs = docs_dir.is_some_and(|docs_dir| {
        let ready = match loaded {
            Some(loaded) => loaded.docs,
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
            live.routes,
        )));
    }
    let artifacts = Arc::new(Artifacts::new(cx.session_dir.join(ARTIFACTS_DIR)));
    let schemes = Schemes::new(cx, artifacts, builtins, host)?;
    Ok((schemes, skills, diagnostics, docs))
}

impl Session {
    /// Builds the tools, loads context files and skills, resolves the exec environment
    /// once for its prompt note, restores the transcript and the read record, records
    /// model, thinking and prompt changes, and runs recovery. A resumed run, or requeued
    /// prompts, run in a spawned task.
    pub(crate) async fn start(setup: SessionSetup) -> Result<Session, Error> {
        let SessionSetup {
            inherited,
            model,
            thinking,
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
            host_sections,
            host_tools,
            host_schemes,
            ..
        } = inherited.as_ref();

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
        inherited
            .sessions
            .register(session_id.clone(), Arc::downgrade(&jobs));
        let (schemes, skills, skill_diagnostics, docs) = session_schemes(
            config,
            docs_dir.as_deref(),
            &inherited.harness_name,
            SchemeContext {
                session_id: session_id.clone(),
                session_dir: path.with_extension(""),
                cwd: cwd.clone(),
            },
            host_schemes,
            Some(LiveSchemes {
                jobs: jobs.clone(),
                sessions: inherited.sessions.clone(),
                store: inherited.store.clone(),
                routes: AgentRoutes {
                    session_id: session_id.clone(),
                    parent: link.as_ref().map(|link| link.parent_session_id.clone()),
                    team: link.as_ref().and_then(|link| link.team.clone()),
                },
            }),
            inherited.loaded.as_ref(),
        )?;
        let (summary_prompts, summary_diagnostics) =
            compaction::summary::SummaryPrompts::load(&config.prompts, &inherited.harness_name);
        let mut section_diagnostics = Vec::new();
        let validated_host_sections = host_sections
            .iter()
            .filter(|(name, _)| {
                let valid = crate::context::prompt::valid_section_name(name);
                if !valid {
                    section_diagnostics.push(Diagnostic {
                        origin: None,
                        message: format!("Invalid system prompt section name: {name}"),
                    });
                }
                valid
            })
            .cloned()
            .collect();

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
        let host: Vec<Arc<dyn Tool>> = host_tools
            .iter()
            .map(|factory| factory(&session_tools))
            .collect();
        let spawn_tool = Arc::new(SpawnTool::new(&inherited.agents, &settings.model_roles));
        let builtin = tools::builtin(
            &session_tools,
            Some(spawn_tool.clone()),
            team.clone().map(|team| TeamSeat {
                team,
                member: session_id.clone(),
                sessions: inherited.sessions.clone(),
            }),
            inherited.ask_enabled,
            questions.clone(),
            settings.ask.timeout_ms.map(Duration::from_millis),
        );
        let depth = link.as_ref().map_or(0, |link| link.depth);
        let yield_tool: Option<Arc<dyn Tool>> = link
            .as_ref()
            .and_then(|link| link.spec.output.as_ref())
            .zip(yield_state.as_ref())
            .map(|(output, state)| {
                Arc::new(YieldTool::new(output, state.clone())) as Arc<dyn Tool>
            });
        let shape_base = ShapeBase {
            config: config.clone(),
            cwd: cwd.clone(),
            project,
            builtin,
            host_tools: host,
            host_tool_factories: host_tools.clone(),
            host_sections: validated_host_sections,
            depth,
            yield_tool,
        };
        let (prompt_shape, nested, shape_diagnostics) = shape(&shape_base, root_agent);
        session_tools
            .context
            .reset(nested.root, nested.names, &nested.startup);
        let tools = prompt_shape.tools.clone();
        let diagnostics: Vec<_> = shape_diagnostics
            .into_iter()
            .chain(skill_diagnostics)
            .chain(commands::reserved_skill_diagnostics(&skills))
            .chain(summary_diagnostics)
            .chain(section_diagnostics)
            .chain(inherited.agent_diagnostics.iter().cloned())
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
        let mut tool_hooks: Vec<Arc<dyn ToolHooks>> = Vec::new();
        if let Some(child) = &child {
            tool_hooks.push(child.budget.clone());
        }
        tool_hooks.extend(inherited.tool_hooks.iter().cloned());
        let hooks = Arc::new_cyclic(|this: &Weak<SessionHooks>| {
            jobs.attach(this.clone() as Weak<dyn JobHost>);
            spawn_tool.attach(this.clone());
            SessionHooks {
                events,
                inherited: inherited.clone(),
                session_id: session_id.clone(),
                shape_base: Mutex::new(shape_base),
                agent_spec: Mutex::new(root_agent.cloned()),
                path,
                tools: session_tools.clone(),
                questions,
                shutdown: AtomicBool::new(false),
                journal,
                live: Mutex::new(LiveRun::default()),
                selection: watch::Sender::new(Selection {
                    model: model.clone(),
                    thinking,
                }),
                activity: watch::Sender::new(Activity::default()),
                queue_order: tokio::sync::Mutex::new(()),
                quiet: Mutex::new(Vec::new()),
                selection_order: tokio::sync::Mutex::new(()),
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
                        scheme_docs: session_tools.schemes.prompt_docs(),
                        skills,
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
                child,
            }
        });
        let agent = Arc::new(Agent::new(AgentConfig {
            thinking,
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
        if hooks.child.is_some() {
            tokio::spawn(children::park_when_idle(
                Arc::downgrade(&hooks),
                hooks.activity.subscribe(),
                jobs.subscribe(),
            ));
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
            Some(Resumption::Requeue(queued)) => {
                let mut prompts = Vec::new();
                for (kind, message) in queued {
                    match kind {
                        SubmissionKind::Prompt | SubmissionKind::Mail => prompts.push(message),
                        SubmissionKind::Steer => agent.steer(message),
                        SubmissionKind::FollowUp => agent.follow_up(message),
                        SubmissionKind::Quiet => enqueue(agent, &session.hooks, kind, message),
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
    pub fn path(&self) -> &Path {
        &self.hooks.path
    }

    /// The header id (UUIDv7), known before the session file exists.
    pub fn id(&self) -> &str {
        &self.hooks.session_id
    }

    /// The spawn link of a child session; `None` for root sessions and forks.
    pub fn spawn_link(&self) -> Option<&store::SpawnLink> {
        self.hooks.child.as_ref().map(|child| &child.link)
    }

    /// The current root persona or the definition this child was spawned with.
    pub fn agent(&self) -> Option<AgentSpec> {
        self.hooks
            .agent_spec
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn model(&self) -> Model {
        self.hooks.model()
    }

    pub fn thinking(&self) -> ThinkingLevel {
        self.hooks.selection().thinking
    }

    pub fn config(&self) -> &EffectiveConfig {
        &self.inherited.config
    }

    /// Sets the file-wide session name; newlines become spaces and blank names clear it.
    /// May be called while a run is active; this entry contributes no context message.
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

    /// The skills loaded at start; the prompt's catalog and `/name` commands use these.
    pub fn skills(&self) -> &[Skill] {
        &self.hooks.prompt.sources.skills
    }

    /// Agent and team definitions loaded at start; forks share them.
    pub fn agents(&self) -> &[AgentDef] {
        &self.inherited.agents
    }

    /// Start-time context, skill, prompt, and scheme diagnostics, also logged as warnings.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The background jobs `read proc://` lists (promoted, retained and rebuilt,
    /// finished ones included), ascending.
    pub fn jobs(&self) -> Vec<JobInfo> {
        self.hooks.tools.runner.jobs.list()
    }

    /// Stops job `id` (`bg_N` or `agent_N`), like `write proc://<id>/kill`; false when unknown
    /// or no longer running.
    pub fn kill_job(&self, id: &str) -> bool {
        JobId::parse(id).is_some_and(|id| {
            self.hooks
                .tools
                .runner
                .jobs
                .kill(id, crate::jobs::Killer::Host)
        })
    }

    /// Opens a command's flushed raw output, including commands still in the foreground.
    /// A handle remains readable after the command's spool is unlinked.
    pub fn command_output(&self, call_id: &str) -> Option<crate::jobs::CommandOutput> {
        self.hooks.tools.runner.jobs.command_output(call_id)
    }

    /// Requests promotion of a foreground command. A command that finishes first stays foreground.
    pub fn background(&self, call_id: &str) -> bool {
        self.hooks.tools.runner.jobs.background(call_id)
    }

    /// This session's URL schemes: recognize with `exact`, resolve files with `resolve`,
    /// or read UTF-8 text from either file or virtual targets with `read`.
    pub fn schemes(&self) -> &Schemes {
        &self.hooks.tools.schemes
    }

    /// The commands a host may offer, in whatever form it picks: built-ins, then this
    /// session's skills.
    pub fn commands(&self) -> Vec<CommandInfo> {
        commands::catalog(&self.hooks.prompt.sources.skills)
    }

    /// Runs a built-in command under its policy: `model`/`thinking` alongside a turn,
    /// `compact`/`shake`/`navigate` only when idle (`Error::Busy` otherwise).
    pub async fn run_command(&self, command: Command) -> Result<(), Error> {
        match command {
            Command::Compact { focus } => self.compact(focus).await,
            Command::Shake { mode } => self.shake(mode).await,
            Command::Model { model } => {
                let choice = self
                    .inherited
                    .config
                    .settings
                    .resolve_model(&self.inherited.models, &model)?;
                match choice.model {
                    Some(model) => {
                        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
                        detached(
                            async move { hooks.select_model(&agent, model, choice.thinking).await },
                        )
                        .await
                    }
                    None => self
                        .set_thinking(
                            choice
                                .thinking
                                .expect("a selector without a model names a thinking level"),
                        )
                        .await
                        .map(drop),
                }
            }
            Command::Thinking { level } => self.set_thinking(level).await.map(drop),
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

    /// Stops the active run (`CancelReason::User`). Stopping an active run pauses
    /// automatic delivery of job results until the user's next input; an idle Stop
    /// changes nothing.
    pub fn abort(&self) {
        if self.hooks.activity.borrow().running {
            self.hooks.set_auto_paused(true);
            self.hooks.stop_requested.store(true, Ordering::Release);
        }
        self.hooks.cancel_maintenance(false);
        self.agent.cancel(CancelReason::User);
    }

    /// Stops the session as a host does before exiting: the active run, and any run started
    /// later, is cancelled with `CancelReason::Shutdown`, so nothing is recorded as aborted
    /// and the run resumes when the session is next opened. Then every command job is
    /// killed. Returns once the run settled and every job task finished.
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
    pub async fn set_model(&self, model: Model) -> Result<(), Error> {
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move { hooks.select_model(&agent, model, None).await }).await
    }

    /// Switches the root persona from the next request, applying its model selector when
    /// present. Changing the system prompt and tools is cache-heavy on models without
    /// mid-conversation system-message and tool changes.
    pub async fn set_agent(&self, name: Option<&str>) -> Result<(), Error> {
        if self.hooks.child.is_some() {
            return Err(Error::InvalidInput(
                "Child sessions keep the agent they were spawned with".into(),
            ));
        }
        let spec = name
            .map(|name| {
                let spec = self
                    .inherited
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
                let (model_entries, level) = hooks.model_entries(&current, &model, choice.thinking);
                entries.extend(model_entries);
                (model, level)
            });
            hooks.append(entries).await?;
            if let Some((model, level)) = selection {
                hooks.apply_selection(&agent, model, level);
            }
            let (new_shape, nested, diagnostics) = {
                let base = hooks
                    .shape_base
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                shape(&base, spec.as_ref())
            };
            for diagnostic in diagnostics {
                warn!("{diagnostic}");
            }
            let tools = new_shape.tools.clone();
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
            agent.set_tools(tools);
            *hooks
                .agent_spec
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = spec;
            hooks.prompt.rebuild();
            hooks.emit_context();
            Ok(())
        })
        .await
    }

    /// Changes a host section from the next request. Hosts pass sections again on reopen.
    /// This is cache-heavy on models without mid-conversation system messages.
    pub async fn set_section(&self, name: &str, body: Option<String>) -> Result<(), Error> {
        if self.hooks.child.is_some() {
            return Err(Error::InvalidInput(
                "Child sessions keep the prompt and tools they were spawned with".into(),
            ));
        }
        if !crate::context::prompt::valid_section_name(name) {
            return Err(Error::InvalidInput(format!(
                "Invalid system prompt section name: {name}"
            )));
        }
        let _order = self.hooks.selection_order.lock().await;
        let replace = |sections: &mut Vec<(String, Option<String>)>| {
            if let Some((_, value)) = sections.iter_mut().find(|(key, _)| key == name) {
                *value = body.clone();
            } else {
                sections.push((name.to_owned(), body.clone()));
            }
        };
        replace(
            &mut self
                .hooks
                .shape_base
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .host_sections,
        );
        replace(
            &mut self
                .hooks
                .prompt
                .shape
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .host_sections,
        );
        self.hooks.prompt.rebuild();
        Ok(())
    }

    /// Replaces host tool factories from the next request, still honoring the persona's
    /// allowlist. Hosts pass tools again on reopen. Changes are cache-heavy on models
    /// without mid-conversation tool changes.
    pub async fn set_tools(&self, tools: Vec<ToolFactory>) -> Result<(), Error> {
        if self.hooks.child.is_some() {
            return Err(Error::InvalidInput(
                "Child sessions keep the prompt and tools they were spawned with".into(),
            ));
        }
        let _order = self.hooks.selection_order.lock().await;
        let host = tools
            .iter()
            .map(|factory| factory(&self.hooks.tools))
            .collect();
        let spec = self.agent();
        let (new_shape, _, diagnostics) = {
            let mut base = self
                .hooks
                .shape_base
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            base.host_tools = host;
            base.host_tool_factories = tools;
            shape(&base, spec.as_ref())
        };
        for diagnostic in diagnostics {
            warn!("{diagnostic}");
        }
        let tools = new_shape.tools;
        self.hooks
            .prompt
            .shape
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tools = tools.clone();
        self.agent.set_tools(tools);
        self.hooks.prompt.rebuild();
        Ok(())
    }

    /// Sets the thinking level from the next request, clamped to the model; returns the
    /// level applied.
    pub async fn set_thinking(&self, level: ThinkingLevel) -> Result<ThinkingLevel, Error> {
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move { hooks.select_thinking(&agent, level).await }).await
    }

    /// Settles the pending question of tool call `call_id`.
    pub fn answer(&self, call_id: &str, answers: Vec<Answer>) -> Result<(), Error> {
        self.hooks.questions.answer(call_id, answers)
    }

    /// Dismisses the pending question of tool call `call_id`; the run is cancelled.
    pub fn dismiss(&self, call_id: &str) -> Result<(), Error> {
        self.hooks.questions.dismiss(call_id)?;
        self.abort();
        Ok(())
    }

    /// The current estimated context size and effective compaction limits.
    pub fn context_usage(&self) -> Option<ContextUsage> {
        self.hooks.context_usage()
    }

    /// The entries after `from` (all with `None`), the live state and an event receiver,
    /// all taken at one point of the append order: no entry is missed or duplicated.
    /// Never waits on a write in flight.
    pub async fn attach(&self, from: Option<u64>) -> Attach {
        // Entries publish under the journal lock, live updates relay under the live lock
        // and job rows announce under the registry lock: subscribing inside all three
        // splits every event into before (in the snapshot) or after (received).
        self.hooks.journal.read(|view| {
            let live = self.hooks.live();
            let activity = *self.hooks.activity.borrow();
            let (queued_steering, queued_follow_up) = self.agent.queued();
            let (jobs, events) = self
                .hooks
                .tools
                .runner
                .jobs
                .announced_with(|| self.hooks.events.subscribe());
            let start = from.map_or(0, |seq| {
                view.entries.partition_point(|entry| entry.seq <= seq)
            });
            Attach {
                missed: view.entries[start..].to_vec(),
                live: LiveSnapshot {
                    run_id: live.run_id.clone(),
                    maintenance: activity.maintenance,
                    running: activity.running,
                    model: self.model(),
                    thinking: self.thinking(),
                    agent: self.agent().map(|spec| spec.name),
                    context: self.hooks.context_usage_for(&view.projection),
                    auto_paused: self.hooks.auto_paused.load(Ordering::Acquire),
                    streaming: live.streaming.as_ref().and_then(Accumulator::snapshot),
                    pending_tool_calls: live.tool_calls.clone(),
                    queued_steering,
                    queued_follow_up,
                    messages: view.projection.messages.clone(),
                    pending_questions: self.hooks.questions.pending(),
                    jobs,
                    usage: view
                        .entries
                        .iter()
                        .filter_map(|seq| seq.entry.usage())
                        .fold(Usage::default(), |mut total, usage| {
                            total += usage;
                            total
                        }),
                },
                events,
            }
        })
    }

    /// Copies the root→`entry_id` path into a new session file that records this one as
    /// its parent. The fork runs with this session's resolved configuration. Forking at a
    /// user message copies the path to its parent and returns the message's parts to edit
    /// and resubmit. A live fork shares this session's child-run slots; a fork of a child
    /// also keeps its agent prompt, without `ask` or host sections. Reopening the fork with
    /// `Axo::open_session` resolves it as an independent root.
    pub async fn fork(&self, entry_id: &EntryId) -> Result<Fork, Error> {
        let Selection { model, thinking } = self.hooks.selection();
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
        copy_for_fork(
            &parent_dir,
            &child_dir,
            self.hooks.tools.schemes.artifacts().live(),
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
            } => Some((run_id, outcome, reason)),
            RecoveryPlan::Park { run_id }
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
                    })
                    .await?,
            );
        }

        let projection = project(&entries, None);
        let mut inherited = (*self.inherited).clone();
        inherited.agent = self.agent();
        {
            let base = self
                .hooks
                .shape_base
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            inherited.host_tools = base.host_tool_factories.clone();
            inherited.host_sections = base.host_sections.clone();
        }
        let session = Session::start(SessionSetup {
            inherited: Arc::new(inherited),
            model,
            thinking,
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
