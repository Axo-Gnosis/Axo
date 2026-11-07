//! A conversation with one agent in one working directory, saved as it happens.

pub(crate) mod journal;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{
    Agent, AgentConfig, AgentContext, AgentEvent, AgentMessage, AgentSnapshot, CancelReason,
    CustomMessage, HookError, LoopHooks, MessageOrigin, Persist, PersistError,
    PrepareRequestContext, QueuedMessage, Replay, RequestUpdate, RunOutcome, Tool, TurnContext,
    TurnDecision, TurnFinish, TurnUpdate,
};
use axo_ai::thinking::clamp_level;
use axo_ai::{
    Accumulator, AssistantMessage, Message, Model, Models, StopReason, Streamer, SystemMessage,
    ThinkingLevel, ToolCall, ToolResultMessage, ToolSpec, Usage, UserBlock, UserMessage, now_ms,
};
use futures::FutureExt;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use self::journal::{Journal, View};
use crate::commands::{self, Command, CommandInfo};
use crate::compaction::{self, Limits};
use crate::config::{CompactionSettings, EffectiveConfig, ProjectAccess, SummaryRequest};
use crate::context::files::{ContextFile, NestedContext, context_file_names, load_context_files};
use crate::context::prompt::{PromptInputs, Sections, build_sections, system_message};
use crate::context::skills::{Skill, load_skills};
use crate::context::{Diagnostic, home_agents_dir, project_root};
use crate::error::Error;
use crate::exec::ExecEnv;
use crate::exec::host::ProcessSupervisor;
use crate::input::{Prepared, PromptInput, PromptPart, prepare, render};
use crate::jobs::{
    DeliveryKey, JobHost, JobInfo, JobRegistry, job_name, parse_job_id, render_results,
};
use crate::recovery::{self, RecoveryPlan, plan_recovery};
use crate::schemes::artifact::Artifacts;
use crate::schemes::dir::DirScheme;
use crate::schemes::proc::ProcScheme;
use crate::schemes::skill::SkillScheme;
use crate::schemes::{ARTIFACTS_DIR, LOCAL_DIR, Scheme, SchemeContext, Schemes};
use crate::store::{
    self, CompactionReason, Entry, EntryBase, EntryId, Header, Projection, SeqEntry, SessionStore,
    SessionWriter, SubmissionKind, project,
};
use crate::tools::ask::{Answer, PendingQuestions, Question};
use crate::tools::{self, SessionTools, ToolFactory};

pub struct SessionOptions {
    /// Ignored by `Axo::open_session`, which uses the cwd in the session header.
    pub cwd: PathBuf,
    /// `provider/model-id`; falls back to the session's last model, then `defaultModel`.
    pub model: Option<String>,
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
            thinking: None,
            ask_enabled: false,
            project_access: None,
            exec_env: None,
            sections: Vec::new(),
            tools: Vec::new(),
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
    /// The background jobs changed; the full list, as [`Session::jobs`] returns it.
    Jobs {
        jobs: Vec<JobInfo>,
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
    pub agent: AgentSnapshot,
    /// The projected conversation at this point in the append order, from the journal's view.
    pub messages: Vec<AgentMessage>,
    /// Unanswered `ask` calls by tool-call id. A question raised while attaching can
    /// also arrive as a `QuestionPending` event (duplicated, never missed).
    pub pending_questions: Vec<(String, Vec<Question>)>,
    /// The background jobs, as [`Session::jobs`] returns them.
    pub jobs: Vec<JobInfo>,
}

/// A new session copied from a point of another.
pub struct Fork {
    pub session: Session,
    /// The parts of the user message forked at, for the client to edit and resubmit.
    pub editable: Option<Vec<PromptPart>>,
}

/// Everything `Axo` resolved for a new, opened or forked session.
pub(crate) struct SessionSetup {
    pub(crate) cwd: PathBuf,
    /// Resolved once by `Axo` for this session; a fork inherits its parent's.
    pub(crate) config: Arc<EffectiveConfig>,
    /// Where bundled docs are materialized for the `docs` section and `axo://`; `None`
    /// omits both.
    pub(crate) docs_dir: Option<PathBuf>,
    pub(crate) streamer: Arc<dyn Streamer>,
    /// Where `/model` looks up models.
    pub(crate) models: Arc<Models>,
    pub(crate) model: Model,
    /// Already clamped to the model.
    pub(crate) thinking: ThinkingLevel,
    pub(crate) exec: Arc<dyn ExecEnv>,
    /// Runs every local command in its own supervised container (process group or cgroup).
    pub(crate) supervisor: Arc<ProcessSupervisor>,
    pub(crate) ask_enabled: bool,
    pub(crate) host_sections: Vec<(String, Option<String>)>,
    pub(crate) host_tools: Vec<ToolFactory>,
    pub(crate) host_schemes: Vec<Arc<dyn Scheme>>,
    pub(crate) store: Arc<dyn SessionStore>,
    /// The header id, forwarded to providers as the session id.
    pub(crate) session_id: String,
    pub(crate) writer: Box<dyn SessionWriter>,
    /// Entries already in the session; empty for a new one.
    pub(crate) entries: Vec<SeqEntry>,
    /// `project(&entries, None)`.
    pub(crate) projection: Projection,
    /// Recovery to run on `entries`; `None` for new sessions and forks.
    pub(crate) recovery: Option<RecoveryPolicy>,
}

pub struct Session {
    agent: Arc<Agent>,
    hooks: Arc<SessionHooks>,
    /// The tools' shared state; its read record follows the current branch.
    tool_state: Arc<SessionTools>,
    questions: Arc<PendingQuestions>,
    exec: Arc<dyn ExecEnv>,
    /// The header id (UUIDv7).
    id: String,
    /// The session file (created lazily on the first submission).
    path: PathBuf,
    // Kept to fork.
    store: Arc<dyn SessionStore>,
    config: Arc<EffectiveConfig>,
    streamer: Arc<dyn Streamer>,
    models: Arc<Models>,
    ask_enabled: bool,
    host_tools: Vec<ToolFactory>,
    host_schemes: Vec<Arc<dyn Scheme>>,
}

/// The last prompt build and the exec-environment note and local date it used.
struct PromptState {
    note: Option<String>,
    date: String,
    /// `None` until the first build.
    sections: Option<Sections>,
}

/// The session's system prompt: its inputs and its last build.
struct SessionPrompt {
    cwd: PathBuf,
    /// The session's resolved config; prompt overrides are read from it.
    config: Arc<EffectiveConfig>,
    docs_dir: Option<PathBuf>,
    tools: Vec<Arc<dyn Tool>>,
    /// Fixed at start: `Schemes` cannot change after `Schemes::new`.
    scheme_docs: Vec<String>,
    context_files: Vec<ContextFile>,
    skills: Vec<Skill>,
    host_sections: Vec<(String, Option<String>)>,
    state: Mutex<PromptState>,
}

impl SessionPrompt {
    fn state(&self) -> std::sync::MutexGuard<'_, PromptState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Rebuilds the prompt sections unless `note` and `date` are unchanged; returns the
    /// system message announcing the change (every section on the first build).
    fn rebuild(&self, note: Option<String>, date: String) -> Option<SystemMessage> {
        let mut state = self.state();
        if state.sections.is_some() && state.note == note && state.date == date {
            return None;
        }
        let (sections, diagnostics) = build_sections(&PromptInputs {
            prompts: &self.config.prompts,
            docs_dir: self.docs_dir.as_deref(),
            cwd: &self.cwd,
            tools: &self.tools,
            scheme_docs: &self.scheme_docs,
            context_files: &self.context_files,
            skills: &self.skills,
            dev_shell: note.as_deref(),
            date: &date,
            host_sections: &self.host_sections,
        });
        for diagnostic in &diagnostics {
            warn!("{diagnostic}");
        }
        let message = system_message(state.sections.as_ref(), &sections);
        *state = PromptState {
            note,
            date,
            sections: Some(sections),
        };
        message
    }

    /// The exec-environment note of the last build.
    fn note(&self) -> Option<String> {
        self.state().note.clone()
    }

    /// The last build's sections; `None` before the first.
    fn sections(&self) -> Option<Sections> {
        self.state().sections.clone()
    }
}

fn today() -> String {
    jiff::Zoned::now().date().to_string()
}

/// The session's loop hooks, journal and event relay.
struct SessionHooks {
    events: broadcast::Sender<SessionEvent>,
    /// System-message diffs not yet in the transcript: they go in before the next turn of
    /// an active run, or ahead of the next prompt.
    system: Mutex<Vec<AgentMessage>>,
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
    maintenance: Maintenance,
    /// The session's command jobs; they append their own records through the journal.
    jobs: Arc<JobRegistry>,
    /// Prompts between their fast-path admission and their run start; idle-wake delivery
    /// holds off meanwhile, so pending results ride the prompt instead of making it busy.
    preparing: AtomicUsize,
    prompt: SessionPrompt,
    /// Set when a run ends other than `Done` or the user stops an active one; finished
    /// job results then wait for the user's next input instead of starting a run.
    auto_paused: AtomicBool,
    /// Set once in `Session::start`; weak, since the agent holds these hooks.
    agent: OnceLock<Weak<Agent>>,
    /// These hooks, for tasks that deliver job results.
    this: OnceLock<Weak<SessionHooks>>,
}

/// The open run's live state as relayed so far. The agent's own copy updates after
/// `persist` returns, so `attach` reads this instead.
#[derive(Default)]
struct LiveRun {
    /// The open run's id, set by its `run_start` (or by recovery).
    run_id: Option<String>,
    streaming: Option<Accumulator>,
    tool_calls: Vec<ToolCall>,
}

/// Who owns the session. A run, standalone maintenance or a navigation sets `running`;
/// maintenance also names itself. Only compare-and-set transitions claim it, so two
/// owners never both win.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Activity {
    running: bool,
    maintenance: Option<MaintenanceKind>,
}

/// The live model and thinking level; `model_change` / `thinking_level_change` entries
/// record each change in the order it took effect.
#[derive(Clone)]
struct Selection {
    model: Model,
    thinking: ThinkingLevel,
}

/// A `running` claim outside a run; dropping it releases the session (or starts pending
/// delivery).
struct Claim<'a>(&'a SessionHooks);

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.release_or_deliver();
    }
}

/// Counts one prompt in [`SessionHooks::preparing`]; the last to drop re-triggers
/// delivery that was held off meanwhile.
struct Preparing(Arc<SessionHooks>);

impl Drop for Preparing {
    fn drop(&mut self) {
        if self.0.preparing.fetch_sub(1, Ordering::AcqRel) == 1 {
            JobHost::results_ready(&*self.0);
        }
    }
}

enum MaintenanceOperation {
    Compact {
        focus: Option<String>,
    },
    Shake(ShakeMode),
    Navigate {
        target: Option<EntryId>,
        sections: Option<Sections>,
        focus: Option<String>,
    },
}

/// Immutable operation inputs plus synchronous cancellation for Drop and dismiss.
struct Maintenance {
    streamer: Arc<dyn Streamer>,
    settings: CompactionSettings,
    prompts: compaction::summary::SummaryPrompts,
    session_id: String,
    path: PathBuf,
    tools: Arc<SessionTools>,
    tool_specs: Vec<ToolSpec>,
    skill_paths: Vec<PathBuf>,
    cancel: Mutex<Option<CancellationToken>>,
    shutdown: AtomicBool,
}

struct FailureState {
    at: usize,
    retries: u32,
    overflow_used: bool,
    hidden: bool,
}

/// What `Session::start` runs after recovery.
#[allow(clippy::large_enum_variant)] // One owned startup value; boxing would add an allocation.
enum Resumption {
    /// Continue the open run; requeued input goes through the agent's queues.
    Continue(Vec<(SubmissionKind, QueuedMessage)>),
    /// No run was open: queue steers and follow-ups, and start a run with the prompts.
    Requeue(Vec<(SubmissionKind, QueuedMessage)>),
    RecoverResponse {
        response: AssistantMessage,
        model: Model,
        results: Vec<ToolResultMessage>,
        queued: Vec<(SubmissionKind, QueuedMessage)>,
    },
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
            let outcome = if hooks.maintenance.shutdown.load(Ordering::Acquire) {
                RunOutcome::Aborted(CancelReason::Shutdown)
            } else {
                RunOutcome::Failed("Agent task panicked".into())
            };
            let _ = hooks.settle(Ok(outcome)).await;
            std::panic::resume_unwind(panic)
        }
    }
}

impl SessionHooks {
    fn live(&self) -> MutexGuard<'_, LiveRun> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn selection(&self) -> Selection {
        self.selection.borrow().clone()
    }

    fn model(&self) -> Model {
        self.selection.borrow().model.clone()
    }

    fn cancel_maintenance(&self, shutdown: bool) {
        if shutdown {
            self.maintenance.shutdown.store(true, Ordering::Release);
        }
        if let Some(cancel) = self
            .maintenance
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            cancel.cancel();
        }
    }

    fn reserve(
        &self,
        kind: MaintenanceKind,
        standalone: bool,
        cancel: CancellationToken,
    ) -> Result<(), Error> {
        let mut reserved = Ok(());
        self.activity.send_if_modified(|activity| {
            if activity.maintenance.is_some() || (standalone && activity.running) {
                reserved = Err(Error::Busy);
                return false;
            }
            if self.maintenance.shutdown.load(Ordering::Acquire) {
                reserved = Err(Error::MaintenanceAborted);
                return false;
            }
            activity.maintenance = Some(kind);
            activity.running |= standalone;
            *self
                .maintenance
                .cancel
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(cancel);
            true
        });
        reserved?;
        let _ = self.events.send(SessionEvent::MaintenanceStart { kind });
        Ok(())
    }

    fn end_maintenance(&self, standalone: bool, error: Option<String>, aborted: bool) {
        *self
            .maintenance
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        let mut ended = None;
        self.activity.send_if_modified(|activity| {
            ended = activity.maintenance.take();
            ended.is_some()
        });
        if let Some(kind) = ended {
            let _ = self.events.send(SessionEvent::MaintenanceEnd {
                kind,
                error,
                aborted,
            });
            if standalone {
                self.release_or_deliver();
            }
        }
    }

    /// The agent and these hooks, while the session lives.
    fn owners(&self) -> Option<(Arc<Agent>, Arc<SessionHooks>)> {
        Some((
            self.agent.get().and_then(Weak::upgrade)?,
            self.this.get().and_then(Weak::upgrade)?,
        ))
    }

    /// Whether finished job results may start a run of their own.
    fn may_deliver(&self) -> bool {
        !self.auto_paused.load(Ordering::Acquire)
            && !self.maintenance.shutdown.load(Ordering::Acquire)
            && self.preparing.load(Ordering::Acquire) == 0
            && self.activity.borrow().maintenance.is_none()
    }

    /// Claims the idle session; false while a run, maintenance or navigation owns it.
    fn claim_idle(&self) -> bool {
        self.activity.send_if_modified(|activity| {
            let idle = !activity.running && activity.maintenance.is_none();
            activity.running |= idle;
            idle
        })
    }

    /// For the owner of `running`: starts a run delivering every pending job result;
    /// false when nothing is pending.
    fn start_delivery(&self, agent: Arc<Agent>, hooks: Arc<SessionHooks>) -> bool {
        let Some(delivery) = self.reserve_delivery() else {
            return false;
        };
        let messages = self.run_input(Some(delivery), None);
        tokio::spawn(async move {
            if let Err(err) = run_to_settle(agent, hooks, RunRequest::Prompt(messages)).await {
                warn!(%err, "job result delivery run failed");
            }
        });
        true
    }

    /// Ends the busy state its caller owns, unless pending job results start a delivery
    /// run first: then `running` stays set and the run settles like any other. Called
    /// wherever the session would go idle.
    fn release_or_deliver(&self) {
        if self.may_deliver()
            && let Some((agent, hooks)) = self.owners()
            && self.start_delivery(agent, hooks)
        {
            return;
        }
        self.activity
            .send_modify(|activity| activity.running = false);
        // A result that became pending meanwhile found `running` set and left its
        // delivery to this transition.
        self.deliver_if_idle();
    }

    /// Starts delivery when the session is idle and pending results may start a run.
    /// While a run or maintenance owns the session, its steering hook or its idle
    /// transition delivers instead.
    fn deliver_if_idle(&self) {
        let Some((agent, hooks)) = self.owners() else {
            return;
        };
        while self.may_deliver() && self.jobs.has_pending() && self.claim_idle() {
            if self.start_delivery(agent.clone(), hooks.clone()) {
                return;
            }
            self.activity
                .send_modify(|activity| activity.running = false);
        }
    }

    /// Rebuilds the prompt for `note` and today's date; a change goes out as a pending
    /// system diff.
    fn refresh_prompt(&self, note: Option<String>) {
        if let Some(message) = self.prompt.rebuild(note, today()) {
            self.system
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(AgentMessage::Llm(Message::System(message)));
        }
    }

    /// The opening messages of a run: the prompt refreshed to today's date (exec-env note
    /// kept), pending system diffs, then `delivery`, then `input`.
    fn run_input(
        &self,
        delivery: Option<QueuedMessage>,
        input: Option<QueuedMessage>,
    ) -> Vec<QueuedMessage> {
        self.refresh_prompt(self.prompt.note());
        let mut messages: Vec<QueuedMessage> =
            self.take_system().into_iter().map(Into::into).collect();
        messages.extend(delivery);
        messages.extend(input);
        messages
    }

    /// Reserves every pending job result for one queued `job_result` message; `None`
    /// when nothing is pending.
    fn reserve_delivery(&self) -> Option<QueuedMessage> {
        if !self.jobs.has_pending() {
            return None;
        }
        let id = format!("delivery-{}", Uuid::now_v7());
        let results = self.jobs.reserve(DeliveryKey::Input(id.clone()));
        if results.is_empty() {
            return None;
        }
        let jobs: Vec<String> = results.iter().map(|r| job_name(r.id)).collect();
        let message = AgentMessage::Custom(CustomMessage {
            custom_type: "job_result".into(),
            content: vec![UserBlock::Text {
                text: format!("Background job results:\n\n{}", render_results(&results)),
            }],
            display: true,
            details: Some(serde_json::json!({ "jobs": jobs })),
            timestamp: now_ms(),
        });
        Some(QueuedMessage {
            message,
            id: Some(id),
        })
    }

    /// Fails once maintenance was cancelled or the session shuts down.
    fn check_live(&self, cancel: &CancellationToken) -> Result<(), Error> {
        if cancel.is_cancelled() || self.maintenance.shutdown.load(Ordering::Acquire) {
            return Err(Error::MaintenanceAborted);
        }
        Ok(())
    }

    /// Refuses writes once a shutdown aborts the running maintenance.
    fn check_writable(&self) -> Result<(), Error> {
        if self.activity.borrow().maintenance.is_some()
            && self.maintenance.shutdown.load(Ordering::Acquire)
        {
            return Err(Error::MaintenanceAborted);
        }
        Ok(())
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
        if appended.iter().any(|seq| {
            matches!(
                seq.entry,
                Entry::ContextEdit { .. }
                    | Entry::Compaction { .. }
                    | Entry::BranchSummary { .. }
                    | Entry::Leaf { .. }
            )
        }) {
            let view = self.journal.view();
            self.maintenance
                .tools
                .ledger
                .restore(&view.projection.messages);
            self.maintenance
                .tools
                .context
                .restore(&view.projection.messages);
        }
    }

    /// Validates `pending` as the next entries after `view` without writing them.
    fn candidate(
        view: &View,
        pending: &[Entry],
        before: u64,
        limits: &Limits,
        require_trigger: bool,
    ) -> Result<u64, Error> {
        // ponytail: copies the entries once per maintenance commit; projecting over a
        // borrowed prefix plus suffix would avoid it if commits ever become frequent.
        let mut entries = view.entries.clone();
        let seq = entries.last().map_or(0, |entry| entry.seq);
        let mut parent = entries
            .last()
            .and_then(|last| last.entry.id())
            .map(|id| EntryId(id.to_owned()));
        for (at, entry) in pending.iter().enumerate() {
            let mut entry = entry.clone();
            // The journal assigns the real ids and parents; the projection needs a chain.
            if let Some(base) = entry.base_mut() {
                if base.id.0.is_empty() {
                    base.id = EntryId(format!("candidate-{at}"));
                }
                base.parent_id = parent.take();
                parent = Some(base.id.clone());
            }
            entries.push(SeqEntry {
                seq: seq + at as u64 + 1,
                entry,
            });
        }
        let projected = project(&entries, None);
        compaction::validate_projection(&projected, before, limits, require_trigger)
    }

    async fn save_usage(&self, model: &Model, usage: Usage, note: String) -> Result<(), Error> {
        if usage == Usage::default() || self.maintenance.shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        self.append(vec![Entry::Usage {
            base: journal::base(),
            kind: "compaction".into(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            usage,
            note: Some(note),
        }])
        .await?;
        Ok(())
    }

    async fn elide(
        &self,
        mode: compaction::elide::ElisionMode,
        cancel: &CancellationToken,
    ) -> Result<bool, Error> {
        let (artifacts, pending, revision) = {
            let view = self.journal.view();
            let sources = view
                .projection
                .sources
                .iter()
                .filter_map(|source| {
                    let entry = &view.entries[source.entry_index].entry;
                    matches!(entry, Entry::Message { .. } | Entry::CustomMessage { .. })
                        .then(|| {
                            entry
                                .id()
                                .map(|id| (EntryId(id.to_owned()), source.messages.clone()))
                        })
                        .flatten()
                })
                .collect::<Vec<_>>();
            let before = compaction::estimate(&view.projection);
            let elision = compaction::elide::prepare(
                &view.projection.messages,
                &sources,
                self.maintenance.tools.schemes.artifacts(),
                &self.maintenance.skill_paths,
                mode,
            )?;
            let Some(elision) = elision.filter(|elision| !elision.edits.is_empty()) else {
                return Ok(false);
            };
            self.check_live(cancel)?;
            let pending: Vec<_> = elision
                .edits
                .into_iter()
                .map(|(target_id, replacement)| Entry::ContextEdit {
                    base: journal::base(),
                    target_id,
                    replacement: Some(replacement),
                })
                .collect();
            if let compaction::elide::ElisionMode::Clearing { limits } = mode {
                let Ok(after) = Self::candidate(&view, &pending, before, &limits, true) else {
                    return Ok(false);
                };
                if limits.trigger < 20_000 || after > limits.trigger - 20_000 || after >= before {
                    return Ok(false);
                }
            }
            (elision.artifacts, pending, view.revision)
        };
        compaction::elide::write_artifacts(
            self.maintenance.tools.schemes.artifacts().dir(),
            &artifacts,
        )
        .await?;
        self.check_live(cancel)?;
        self.append_if(revision, pending).await?;
        Ok(true)
    }

    async fn compact_inner(
        &self,
        reason: CompactionReason,
        focus: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<bool, Error> {
        let automatic = reason != CompactionReason::Manual;
        let limits = Limits::resolve(&self.model(), &self.maintenance.settings)?;
        if reason == CompactionReason::Threshold
            && compaction::estimate(&self.journal.view().projection) <= limits.trigger
        {
            return Ok(false);
        }
        if automatic
            && self
                .elide(compaction::elide::ElisionMode::Clearing { limits }, cancel)
                .await?
        {
            return Ok(true);
        }
        let mut method = if reason == CompactionReason::Overflow {
            SummaryRequest::Separate
        } else {
            self.maintenance.settings.summary_request
        };
        let mut last_error = String::new();
        for _ in 0..3 {
            self.check_live(cancel)?;
            let Selection { model, thinking } = self.selection();
            let (prepared, revision, checkpoint, system) = {
                let view = self.journal.view();
                // Named by the summary before it is written: the journal keeps this id.
                let checkpoint = EntryId::generate(|id| view.has(id));
                let prepared = compaction::prepare_compaction(
                    &view.projection,
                    &view.entries,
                    &limits,
                    checkpoint.clone(),
                )?;
                let system = axo_ai::transcript::replay_system_message(
                    view.projection
                        .messages
                        .iter()
                        .filter_map(|message| match message {
                            AgentMessage::Llm(Message::System(system)) => Some(system),
                            _ => None,
                        }),
                );
                (prepared, view.revision, checkpoint, system)
            };
            let context = compaction::summary::SummaryContext {
                streamer: self.maintenance.streamer.as_ref(),
                model: &model,
                thinking,
                session_id: &self.maintenance.session_id,
                reply: limits.reply,
                prompts: &self.maintenance.prompts,
            };
            match compaction::summary::generate(&context, &prepared.summary, method, focus, cancel)
                .await
            {
                Err(failure) => {
                    self.save_usage(&model, failure.usage, failure.message.clone())
                        .await?;
                    if failure.fatal || failure.aborted {
                        return Err(if failure.aborted {
                            Error::MaintenanceAborted
                        } else {
                            Error::Compaction(failure.message)
                        });
                    }
                    last_error = failure.message;
                    method = SummaryRequest::Separate;
                }
                Ok(generated) => {
                    let summary = compaction::history_summary(
                        &generated.text,
                        &self.maintenance.path,
                        &prepared.files,
                    );
                    let committed: Result<(), Error> = async {
                        self.check_live(cancel)?;
                        let entry = Entry::Compaction {
                            base: journal::base_with_id(checkpoint),
                            summary,
                            first_kept_entry_id: prepared.first_kept_entry_id,
                            preserved_user_ids: prepared.preserved_user_ids,
                            tokens_before: prepared.tokens_before,
                            details: Some(prepared.files.details()),
                            usage: Some(generated.usage.clone()),
                            from_hook: None,
                            reason: Some(reason),
                            focus: focus.map(str::to_owned),
                            system_message: system,
                        };
                        {
                            let view = self.journal.view();
                            if view.revision != revision {
                                return Err(Error::ContextChanged);
                            }
                            Self::candidate(
                                &view,
                                std::slice::from_ref(&entry),
                                prepared.tokens_before,
                                &limits,
                                automatic,
                            )?;
                        }
                        self.append_if(revision, vec![entry]).await?;
                        Ok(())
                    }
                    .await;
                    match committed {
                        Ok(()) => return Ok(true),
                        Err(error) => {
                            self.save_usage(&model, generated.usage, error.to_string())
                                .await?;
                            if !matches!(error, Error::Compaction(_) | Error::ContextChanged) {
                                return Err(error);
                            }
                            last_error = error.to_string();
                            method = SummaryRequest::Separate;
                        }
                    }
                }
            }
        }
        Err(Error::Compaction(last_error))
    }

    async fn automatic(
        &self,
        reason: CompactionReason,
        cancel: &CancellationToken,
    ) -> Result<bool, Error> {
        if !self.maintenance.settings.enabled {
            return Ok(false);
        }
        let limits = Limits::resolve(&self.model(), &self.maintenance.settings)?;
        if reason == CompactionReason::Threshold
            && compaction::estimate(&self.journal.view().projection) <= limits.trigger
        {
            return Ok(false);
        }
        self.reserve(MaintenanceKind::Compact, false, cancel.clone())?;
        let result = std::panic::AssertUnwindSafe(self.compact_inner(reason, None, cancel))
            .catch_unwind()
            .await;
        let (error, aborted) = match &result {
            Ok(Ok(_)) => (None, false),
            Ok(Err(error)) => (Some(error.to_string()), cancel.is_cancelled()),
            Err(_) => (Some("Maintenance task panicked".into()), false),
        };
        self.end_maintenance(false, error, aborted);
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    /// Appends the system message that moves the transcript's effective prompt to
    /// `current`, if they differ.
    async fn sync_sections(&self, current: &Sections) -> Result<(), Error> {
        let message = {
            let view = self.journal.view();
            let messages = &view.projection.messages;
            let mut effective: Option<Sections> = None;
            for message in messages {
                if let AgentMessage::Llm(Message::System(system)) = message {
                    let Some(patch) = &system.sections else {
                        continue;
                    };
                    let sections = effective.get_or_insert_with(Sections::new);
                    for (name, text) in patch {
                        match text {
                            Some(text) => {
                                sections.insert(name.clone(), text.clone());
                            }
                            None => {
                                sections.shift_remove(name);
                            }
                        }
                    }
                }
            }
            let changes = axo_ai::transcript::tool_state_changes(
                &axo_ai::transcript::replay_tools(messages.iter().filter_map(
                    |message| match message {
                        AgentMessage::Llm(Message::System(system)) => Some(system),
                        _ => None,
                    },
                )),
                &self.maintenance.tool_specs,
            );
            let system = system_message(effective.as_ref(), current);
            if system.is_none()
                && changes.tools_added.is_empty()
                && changes.tools_removed.is_empty()
            {
                return Ok(());
            }
            let mut system = system.unwrap_or_else(|| SystemMessage {
                content: Vec::new(),
                sections: None,
                tools_added: Vec::new(),
                tools_removed: Vec::new(),
                timestamp: now_ms(),
            });
            system.tools_added = changes.tools_added;
            system.tools_removed = changes.tools_removed;
            AgentMessage::Llm(Message::System(system))
        };
        self.append(vec![Entry::Message {
            base: journal::base(),
            message,
            submission_id: None,
            admitted_jobs: Vec::new(),
            delivered_jobs: Vec::new(),
        }])
        .await?;
        Ok(())
    }

    async fn navigate_inner(
        &self,
        target: Option<EntryId>,
        sections: Option<Sections>,
        focus: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let Selection { model, thinking } = self.selection();
        let limits = Limits::resolve(&model, &self.maintenance.settings)?;
        let (prepared, revision, from_id) = {
            let view = self.journal.view();
            if let Some(target) = &target
                && !view.has(target.as_str())
            {
                return Err(Error::EntryNotFound(target.to_string()));
            }
            (
                compaction::prepare_branch(&view.projection, &view.entries, target.as_ref()),
                view.revision,
                view.entries
                    .last()
                    .and_then(|seq| seq.entry.id())
                    .unwrap_or("root")
                    .to_owned(),
            )
        };
        let generated = if let Some((input, files)) = prepared {
            let context = compaction::summary::SummaryContext {
                streamer: self.maintenance.streamer.as_ref(),
                model: &model,
                thinking,
                session_id: &self.maintenance.session_id,
                reply: limits.reply,
                prompts: &self.maintenance.prompts,
            };
            let mut candidate = None;
            for action in 0..3 {
                match compaction::summary::generate(
                    &context,
                    &input,
                    SummaryRequest::Separate,
                    focus.as_deref(),
                    cancel,
                )
                .await
                {
                    Ok(summary) => {
                        candidate = Some((summary, files));
                        break;
                    }
                    Err(failure) => {
                        self.save_usage(&model, failure.usage, failure.message.clone())
                            .await?;
                        if failure.fatal || failure.aborted || action == 2 {
                            return Err(if failure.aborted {
                                Error::MaintenanceAborted
                            } else {
                                Error::Compaction(failure.message)
                            });
                        }
                    }
                }
            }
            candidate
        } else {
            None
        };
        let base = journal::base_at(target);
        let (entry, usage) = match generated {
            Some((summary, files)) => (
                Entry::BranchSummary {
                    base,
                    from_id,
                    summary: compaction::history_summary(
                        &summary.text,
                        &self.maintenance.path,
                        &files,
                    ),
                    details: Some(files.details()),
                    usage: Some(summary.usage.clone()),
                    from_hook: None,
                    focus,
                },
                Some(summary.usage),
            ),
            None => (Entry::Leaf { base }, None),
        };
        let committed = match self.check_live(cancel) {
            Ok(()) => self.append_if(revision, vec![entry]).await.map(drop),
            Err(error) => Err(error),
        };
        if let Err(error) = committed {
            if let Some(usage) = usage {
                self.save_usage(&model, usage, error.to_string()).await?;
            }
            return Err(error);
        }
        self.record_current_selection().await?;
        self.take_system();
        if let Some(sections) = sections {
            self.sync_sections(&sections).await?;
        }
        Ok(())
    }

    async fn standalone(
        self: Arc<Self>,
        agent: Arc<Agent>,
        operation: MaintenanceOperation,
        cancel: CancellationToken,
    ) -> Result<(), Error> {
        let result = std::panic::AssertUnwindSafe(async {
            match operation {
                MaintenanceOperation::Compact { focus } => {
                    self.compact_inner(CompactionReason::Manual, focus.as_deref(), &cancel)
                        .await?;
                }
                MaintenanceOperation::Shake(mode) => {
                    let mode = match mode {
                        ShakeMode::Elide => compaction::elide::ElisionMode::Elide,
                        ShakeMode::Images => compaction::elide::ElisionMode::Images,
                        ShakeMode::Thinking => compaction::elide::ElisionMode::Thinking,
                    };
                    self.elide(mode, &cancel).await?;
                }
                MaintenanceOperation::Navigate {
                    target,
                    sections,
                    focus,
                } => {
                    self.navigate_inner(target, sections, focus, &cancel)
                        .await?
                }
            }
            Ok::<(), Error>(())
        })
        .catch_unwind()
        .await;
        agent.set_messages(self.journal.view().projection.messages.clone());
        let (error, aborted) = match &result {
            Ok(Ok(())) => (None, false),
            Ok(Err(error)) => (
                Some(error.to_string()),
                matches!(error, Error::MaintenanceAborted),
            ),
            Err(_) => (Some("Maintenance task panicked".into()), false),
        };
        self.end_maintenance(true, error, aborted);
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn failure_state(
        log: &View,
        response: &AssistantMessage,
        model: &Model,
    ) -> Result<FailureState, Error> {
        let path = log.projection.path();
        let at = path
            .iter()
            .rev()
            .copied()
            .find(|&at| {
                matches!(&log.entries[at].entry,
                    Entry::Message { message: AgentMessage::Llm(Message::Assistant(saved)), .. }
                        if saved == response
                )
            })
            .ok_or_else(|| {
                Error::InvalidHistory("Completed response is missing from session history".into())
            })?;
        let hidden = |index: usize| {
            log.entries[index]
                .entry
                .id()
                .is_some_and(|id| log.projection.is_omitted(id))
        };
        let mut retries = 0;
        let mut overflow = false;
        let mut selected = None;
        for &index in path {
            if index == at {
                break;
            }
            match &log.entries[index].entry {
                Entry::RunStart { .. }
                | Entry::Message {
                    message: AgentMessage::Llm(Message::User(_)),
                    ..
                } => {
                    retries = 0;
                    overflow = false;
                }
                Entry::ModelChange {
                    provider, model_id, ..
                } => {
                    let next = (provider.as_str(), model_id.as_str());
                    if selected != Some(next) {
                        retries = 0;
                        overflow = false;
                    }
                    selected = Some(next);
                }
                Entry::Message {
                    message: AgentMessage::Llm(Message::Assistant(previous)),
                    ..
                } => {
                    if previous.provider != model.provider || previous.model != model.id {
                        retries = 0;
                        overflow = false;
                    } else if hidden(index) {
                        if recovery::is_pressure(previous, model) {
                            overflow = true;
                        } else if axo_ai::is_retryable_error(previous) {
                            retries += 1;
                        }
                    } else if !matches!(
                        previous.stop_reason,
                        StopReason::Length | StopReason::Error | StopReason::Aborted
                    ) {
                        retries = 0;
                        overflow = false;
                    }
                }
                _ => {}
            }
        }
        Ok(FailureState {
            at,
            retries,
            overflow_used: overflow,
            hidden: hidden(at),
        })
    }

    fn checkpoint_completed(&self, log: &View, state: &FailureState, model: &Model) -> bool {
        let Ok(limits) = Limits::resolve(model, &self.maintenance.settings) else {
            return false;
        };
        let end = if state.hidden { state.at } else { state.at + 1 };
        let prefix = project(&log.entries[..end], None);
        let before = compaction::estimate(&prefix);
        let after = compaction::estimate(&log.projection);
        let checkpoint = log.projection.path().iter().any(|&index| {
            index > state.at && matches!(log.entries[index].entry, Entry::Compaction { .. })
        });
        let clearing = log.projection.path().iter().any(|&index| {
            index > state.at
                && matches!(
                    log.entries[index].entry,
                    Entry::ContextEdit {
                        replacement: Some(_),
                        ..
                    }
                )
        });
        after < before
            && ((checkpoint && after <= limits.trigger)
                || (clearing && limits.trigger >= 20_000 && after <= limits.trigger - 20_000))
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

    fn context(&self, tools: &[Arc<dyn Tool>]) -> AgentContext {
        AgentContext {
            messages: self.journal.view().projection.messages.clone(),
            tools: tools.to_vec(),
        }
    }

    fn take_system(&self) -> Vec<AgentMessage> {
        std::mem::take(&mut *self.system.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Closes the run with `run_end` (none after a shutdown) and releases the session
    /// for the next run, or starts delivering pending job results. Reservations and
    /// admissions the run never committed are released first; a run that did not end
    /// `Done`, or a failed `run_end`, pauses automatic delivery until the user acts.
    async fn settle(
        &self,
        result: Result<RunOutcome, axo_agent::Error>,
    ) -> Result<RunOutcome, Error> {
        self.jobs.release_uncommitted();
        if !matches!(result, Ok(RunOutcome::Done)) {
            self.auto_paused.store(true, Ordering::Release);
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
                        run_id,
                        outcome,
                        reason,
                    }])
                    .await
                    .map(drop);
            }
        }
        if closed.is_err() {
            self.auto_paused.store(true, Ordering::Release);
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
    ) -> Result<(), Error> {
        let queued = {
            // Queued under the selection lock, ordered with `Session::set_model`.
            let selection = self.selection.borrow();
            let mut entries = Vec::new();
            if model
                != Some(format!("{}/{}", selection.model.provider, selection.model.id).as_str())
            {
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
            if entries.is_empty() {
                return Ok(());
            }
            self.append(entries)
        };
        queued.await?;
        Ok(())
    }

    /// [`SessionHooks::record_selection`] against the current position.
    async fn record_current_selection(&self) -> Result<(), Error> {
        let (model, thinking) = {
            let view = self.journal.view();
            (view.projection.model.clone(), view.projection.thinking)
        };
        self.record_selection(model.as_deref(), thinking).await
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
        request: PrepareRequestContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<Option<RequestUpdate>, HookError> {
        if self
            .automatic(CompactionReason::Threshold, cancel)
            .await
            .map_err(|error| HookError(error.to_string()))?
        {
            Ok(Some(RequestUpdate {
                context: Some(self.context(&request.context.tools)),
                ..RequestUpdate::default()
            }))
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
                let state = Self::failure_state(&self.journal.view(), response, turn.model)?;
                let unusable = state.hidden || recovery::foreign_unusable(response, &model);
                if unusable {
                    self.omit_response(state.at, response).await?;
                    return Ok(TurnFinish {
                        decision: Some(TurnDecision::Continue),
                        context: Some(self.context(&turn.context.tools)),
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
                let context = changed.then(|| self.context(&turn.context.tools));
                return Ok(TurnFinish {
                    decision: None,
                    context,
                });
            }
            let faulty =
                pressure || matches!(response.stop_reason, StopReason::Error | StopReason::Length);
            if faulty {
                self.check_live(cancel)?;
                let model = self.model();
                // `Some((delay, shrink))` retries after omitting the response.
                let (at, retries, retry) = {
                    let view = self.journal.view();
                    let state = Self::failure_state(&view, response, turn.model)?;
                    let retry = if !recovery::served_by(response, &model) {
                        Some((None, false))
                    } else if pressure {
                        if !self.maintenance.settings.enabled || state.overflow_used {
                            return Ok(TurnFinish::default());
                        }
                        let completed = self.checkpoint_completed(&view, &state, &model);
                        Some((None, !(state.hidden && completed)))
                    } else {
                        axo_ai::retry_delay(response, state.retries)
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
                    decision: Some(TurnDecision::Continue),
                    context: Some(self.context(&turn.context.tools)),
                });
            }
            if response.tool_calls().next().is_none() {
                let changed = self.automatic(CompactionReason::Threshold, cancel).await?;
                let context = changed.then(|| self.context(&turn.context.tools));
                return Ok(TurnFinish {
                    decision: None,
                    context,
                });
            }
            Ok(TurnFinish::default())
        }
        .await;
        result.map_err(|error| HookError(error.to_string()))
    }

    async fn prepare_next_turn(
        &self,
        _turn: TurnContext<'_>,
        _cancel: &CancellationToken,
    ) -> Option<TurnUpdate> {
        let messages = self.take_system();
        (!messages.is_empty()).then(|| TurnUpdate {
            messages,
            ..TurnUpdate::default()
        })
    }

    /// Finished job results reach an active run at each steering drain point. They do
    /// not set the steer signal, so they never promote `bash` or interrupt `wait`.
    async fn steering(&self) -> Vec<QueuedMessage> {
        self.reserve_delivery().into_iter().collect()
    }
}

#[async_trait]
impl Persist for SessionHooks {
    /// Saves the run's start and every finished message before the loop goes on, then
    /// relays the event onto the session channel. Only the writes await, and they await
    /// only the journal task; live state changes under a lock that is never held across
    /// an await.
    async fn persist(&self, event: &mut AgentEvent) -> Result<(), PersistError> {
        match &*event {
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
                let mut live = self.live();
                live.streaming
                    .get_or_insert_with(Accumulator::new)
                    .apply(update);
                // Relayed under the live lock: see `Session::attach`.
                let _ = self.events.send(SessionEvent::Agent(event.clone()));
                return Ok(());
            }
            AgentEvent::MessageEnd { message, origin } => {
                // Cleared before the entry lands: an attach in between sees neither and
                // receives the entry.
                self.live().streaming = None;
                // Receipts: a queued delivery or a `wait` result commits the job results
                // reserved under its key; a promoting `bash` result admits its job, and
                // carries the job's `job_start` in the same batch.
                let (submission_id, key, started) = match origin {
                    MessageOrigin::Input(Some(id)) => {
                        let key = DeliveryKey::Input(id.clone());
                        if self.jobs.reserved_ids(&key).is_empty() {
                            (Some(id.clone()), None, None)
                        } else {
                            (None, Some(key), None)
                        }
                    }
                    MessageOrigin::Input(None) | MessageOrigin::Run => (None, None, None),
                    MessageOrigin::Tool(invocation) => (
                        None,
                        Some(DeliveryKey::Invocation(*invocation)),
                        self.jobs.take_start(*invocation),
                    ),
                };
                let delivered_jobs = key
                    .as_ref()
                    .map(|key| self.jobs.reserved_ids(key))
                    .unwrap_or_default();
                let admitted = started
                    .as_ref()
                    .filter(|started| started.admits)
                    .map(|started| started.id);
                let mut batch = Vec::with_capacity(2);
                // The start first: a receipt never names a job whose start is not on disk.
                batch.extend(started.map(|started| started.entry));
                batch.push(Entry::Message {
                    base: journal::base(),
                    message: message.clone(),
                    submission_id,
                    admitted_jobs: admitted.map(job_name).into_iter().collect(),
                    delivered_jobs,
                });
                match self.append(batch).await {
                    Ok(_) => {
                        if let Some(key) = &key {
                            self.jobs.commit(key);
                        }
                        if let Some(id) = admitted {
                            self.jobs.admitted(id);
                        }
                    }
                    Err(err) => {
                        if let Some(key) = &key {
                            self.jobs.release(key);
                        }
                        if let Some(id) = admitted {
                            self.jobs.admission_failed(id);
                        }
                        return Err(PersistError(err.to_string()));
                    }
                }
            }
            AgentEvent::ToolExecutionStart {
                call_id,
                name,
                args,
            } => {
                let mut live = self.live();
                live.tool_calls.push(ToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    arguments: args.clone(),
                    arguments_error: None,
                });
                let _ = self.events.send(SessionEvent::Agent(event.clone()));
                return Ok(());
            }
            AgentEvent::ToolExecutionEnd { call_id, .. } => {
                let mut live = self.live();
                live.tool_calls.retain(|call| &call.id != call_id);
                let _ = self.events.send(SessionEvent::Agent(event.clone()));
                return Ok(());
            }
            AgentEvent::AgentEnd { .. } => {
                let mut live = self.live();
                live.streaming = None;
                live.tool_calls.clear();
                let _ = self.events.send(SessionEvent::Agent(event.clone()));
                return Ok(());
            }
            _ => {}
        }
        let _ = self.events.send(SessionEvent::Agent(event.clone()));
        Ok(())
    }
}

impl JobHost for SessionHooks {
    fn results_ready(&self) {
        self.deliver_if_idle();
    }
}

/// Sends a `SessionEvent::Jobs` snapshot on each registry change; ends once the registry
/// is gone. Weak, so the relay never keeps a closed session's jobs alive.
async fn relay_jobs(
    registry: Weak<JobRegistry>,
    mut changed: watch::Receiver<u64>,
    events: broadcast::Sender<SessionEvent>,
) {
    while changed.changed().await.is_ok() {
        let Some(live) = registry.upgrade() else {
            return;
        };
        let jobs = live.list().iter().map(|job| job.info()).collect();
        drop(live);
        let _ = events.send(SessionEvent::Jobs { jobs });
    }
}

/// Loads the skills a session in `cwd` sees and builds its file-backed schemes over them:
/// `artifact://` and `local://` in the side folders beside the session file `path`,
/// `skill://`, `axo://` docs when `docs_dir` is given, `proc://` over `jobs` when given,
/// then `host` schemes. Open sessions and `Axo::schemes` both build them here, so given
/// the same effective config a file-backed URL resolves alike whether its session is open
/// or not; only open sessions get `proc://` (`Axo::schemes` passes no `jobs`), since a
/// closed session has no live jobs.
pub(crate) fn session_schemes(
    config: &EffectiveConfig,
    docs_dir: Option<&Path>,
    cx: SchemeContext,
    host: &[Arc<dyn Scheme>],
    jobs: Option<Arc<JobRegistry>>,
) -> Result<(Schemes, Vec<Skill>, Vec<Diagnostic>), Error> {
    let settings = &config.settings;
    let home_agents = settings.home_agents_dir.then(home_agents_dir).flatten();
    let (skills, diagnostics) = load_skills(
        &config.skill_roots,
        home_agents.as_deref(),
        &cx.cwd,
        config.access != ProjectAccess::Ignore && settings.project.files.skills,
    );
    let mut builtins: Vec<Arc<dyn Scheme>> = vec![
        Arc::new(SkillScheme::new(&skills)),
        Arc::new(DirScheme::local(cx.session_dir.join(LOCAL_DIR))),
    ];
    if let Some(docs_dir) = docs_dir {
        builtins.push(Arc::new(DirScheme::docs(docs_dir.to_path_buf())));
    }
    if let Some(jobs) = jobs {
        builtins.push(Arc::new(ProcScheme { jobs }));
    }
    let artifacts = Arc::new(Artifacts::new(cx.session_dir.join(ARTIFACTS_DIR)));
    let schemes = Schemes::new(cx, artifacts, builtins, host)?;
    Ok((schemes, skills, diagnostics))
}

impl Session {
    /// Builds the tools, loads context files and skills, resolves the exec environment
    /// once for its prompt note, restores the transcript and the read record, records
    /// model, thinking and prompt changes, and runs recovery. A resumed run, or requeued
    /// prompts, run in a spawned task.
    pub(crate) async fn start(setup: SessionSetup) -> Result<Session, Error> {
        let SessionSetup {
            cwd,
            config,
            docs_dir,
            streamer,
            models,
            model,
            thinking,
            exec,
            supervisor,
            ask_enabled,
            host_sections,
            host_tools,
            host_schemes,
            store,
            session_id,
            writer,
            entries,
            projection,
            recovery,
        } = setup;

        let path = writer.planned_path().to_path_buf();
        let (events, _) = broadcast::channel(1024);
        let questions = Arc::new(PendingQuestions::new(events.clone()));
        let settings = &config.settings;
        let project = config.access != ProjectAccess::Ignore;
        let home_agents = settings.home_agents_dir.then(home_agents_dir).flatten();
        let (names, name_diagnostics) = context_file_names(&settings.context_files);
        let (context_files, file_diagnostics) = load_context_files(
            &config.context_files,
            home_agents.as_deref(),
            &cwd,
            project,
            &names,
        );
        let context = NestedContext::new(
            (project && settings.nested_context_files).then(|| project_root(&cwd)),
            names,
            &context_files,
        );
        let (jobs, unfinished) = JobRegistry::new(session_id.clone(), &entries);
        let (schemes, skills, skill_diagnostics) = session_schemes(
            &config,
            docs_dir.as_deref(),
            SchemeContext {
                session_id: session_id.clone(),
                session_dir: path.with_extension(""),
                cwd: cwd.clone(),
            },
            &host_schemes,
            Some(jobs.clone()),
        )?;
        let (summary_prompts, summary_diagnostics) =
            compaction::summary::SummaryPrompts::load(&config.prompts);
        for diagnostic in name_diagnostics
            .iter()
            .chain(&file_diagnostics)
            .chain(&skill_diagnostics)
            .chain(&commands::reserved_skill_diagnostics(&skills))
            .chain(&summary_diagnostics)
        {
            warn!("{diagnostic}");
        }

        let session_tools = Arc::new(SessionTools {
            cwd: cwd.clone(),
            exec: exec.clone(),
            ledger: Default::default(),
            context,
            schemes,
            questions: questions.clone(),
            ask_timeout: settings.ask.timeout_ms.map(Duration::from_millis),
            ci_env: settings.bash.ci,
            supervisor,
            jobs: jobs.clone(),
        });
        let host: Vec<Arc<dyn Tool>> = host_tools
            .iter()
            .map(|factory| factory(&session_tools))
            .collect();
        let tools = tools::merge_tools(tools::builtin(&session_tools, ask_enabled), &host);

        let journal = Journal::start(writer, entries, projection.clone(), events.clone());
        let hooks = Arc::new(SessionHooks {
            events,
            system: Mutex::new(Vec::new()),
            journal,
            live: Mutex::new(LiveRun::default()),
            selection: watch::Sender::new(Selection {
                model: model.clone(),
                thinking,
            }),
            activity: watch::Sender::new(Activity::default()),
            queue_order: tokio::sync::Mutex::new(()),
            maintenance: Maintenance {
                streamer: streamer.clone(),
                settings: settings.compaction.clone(),
                prompts: summary_prompts,
                session_id: session_id.clone(),
                path: path.clone(),
                tools: session_tools.clone(),
                tool_specs: tools
                    .iter()
                    .map(|tool| ToolSpec {
                        name: tool.name().to_owned(),
                        description: tool.description().to_owned(),
                        parameters: tool.parameters().clone(),
                    })
                    .collect(),
                skill_paths: skills.iter().map(|skill| skill.path.clone()).collect(),
                cancel: Mutex::new(None),
                shutdown: AtomicBool::new(false),
            },
            jobs,
            preparing: AtomicUsize::new(0),
            prompt: SessionPrompt {
                cwd,
                config: config.clone(),
                docs_dir,
                tools: tools.clone(),
                scheme_docs: session_tools.schemes.prompt_docs(),
                context_files,
                skills,
                host_sections,
                state: Mutex::new(PromptState {
                    note: None,
                    date: String::new(),
                    sections: None,
                }),
            },
            auto_paused: AtomicBool::new(false),
            agent: OnceLock::new(),
            this: OnceLock::new(),
        });
        hooks.jobs.attach(
            Arc::downgrade(&hooks) as Weak<dyn JobHost>,
            hooks.journal.downgrade(),
        );
        let _ = hooks.this.set(Arc::downgrade(&hooks));
        tokio::spawn(relay_jobs(
            Arc::downgrade(&hooks.jobs),
            hooks.jobs.subscribe(),
            hooks.events.clone(),
        ));
        let agent = Arc::new(Agent::new(AgentConfig {
            thinking,
            tools: tools.clone(),
            hooks: hooks.clone(),
            persist: Some(hooks.clone()),
            steering_mode: settings.steering_mode,
            follow_up_mode: settings.follow_up_mode,
            session_id: Some(session_id.clone()),
            ..AgentConfig::new(streamer.clone(), model)
        }));
        let _ = hooks.agent.set(Arc::downgrade(&agent));

        let session = Session {
            agent,
            hooks,
            tool_state: session_tools,
            questions,
            exec,
            id: session_id,
            path,
            store,
            config,
            streamer,
            models,
            ask_enabled,
            host_tools,
            host_schemes,
        };
        let resolved = session
            .exec
            .resolve(
                &session.hooks.prompt.cwd,
                &CancellationToken::new(),
                &|_| {},
            )
            .await;
        session.hooks.prompt.rebuild(resolved.prompt_note, today());

        // Nothing else runs on the session yet: these writes go in order.
        session
            .hooks
            .record_selection(projection.model.as_deref(), projection.thinking)
            .await?;
        session.sync_prompt().await?;
        if recovery.is_some() {
            session.reconcile_jobs(unfinished).await?;
        }
        let resume = match recovery {
            Some(policy) => {
                session
                    .recover(policy, session.tool_state.schemes.artifacts().dir())
                    .await?
            }
            None => None,
        };
        {
            // After recovery's results, so a replayed `write` finds the file it saw.
            let messages = &session.hooks.journal.view().projection.messages;
            session.tool_state.ledger.restore(messages);
            session.tool_state.context.restore(messages);
            session.agent.set_messages(messages.clone());
        }

        let agent = &session.agent;
        let request = match resume {
            None => None,
            Some(Resumption::RecoverResponse {
                response,
                model,
                results,
                queued,
            }) => {
                for (kind, message) in queued {
                    match kind {
                        SubmissionKind::FollowUp => agent.follow_up(message),
                        SubmissionKind::Prompt | SubmissionKind::Steer => agent.steer(message),
                    }
                }
                Some(RunRequest::RecoverResponse {
                    response,
                    model,
                    results,
                })
            }
            Some(Resumption::Continue(queued)) => {
                let has_turns = agent
                    .messages()
                    .iter()
                    .any(|message| !matches!(message, AgentMessage::Llm(Message::System(_))));
                // With no turn to continue, the requeued input starts the run instead.
                let mut first = Vec::new();
                for (kind, message) in queued {
                    match kind {
                        _ if !has_turns => first.push(message),
                        SubmissionKind::FollowUp => agent.follow_up(message),
                        SubmissionKind::Prompt | SubmissionKind::Steer => agent.steer(message),
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
                        SubmissionKind::Prompt => prompts.push(message),
                        SubmissionKind::Steer => agent.steer(message),
                        SubmissionKind::FollowUp => agent.follow_up(message),
                    }
                }
                (!prompts.is_empty()).then_some(RunRequest::Prompt(prompts))
            }
        };
        if let Some(request) = request {
            let run = run_to_settle(session.agent.clone(), session.hooks.clone(), request);
            tokio::spawn(async move {
                if let Err(err) = run.await {
                    warn!(%err, "resumed run failed");
                }
            });
        }
        Ok(session)
    }

    /// Ends each admitted job a closed session left unfinished: writes its `JobEnd` once
    /// and makes the interrupted result pending. Delivery waits for the next prompt, a
    /// resumed run's steering, or `wait`; opening never starts a run for it.
    async fn reconcile_jobs(&self, unfinished: Vec<u64>) -> Result<(), Error> {
        let artifacts = self.tool_state.schemes.artifacts();
        for id in unfinished {
            let Some((job, _)) = self.hooks.jobs.get(id) else {
                continue;
            };
            let text = if artifacts.find(job.artifact).is_some() {
                format!(
                    "Command was interrupted when the session closed; it may have partially run. Output so far: artifact://{}",
                    job.artifact
                )
            } else {
                "Command was interrupted when the session closed; it may have partially run. Its output is unavailable.".to_owned()
            };
            self.hooks
                .append(vec![Entry::JobEnd {
                    base: journal::base(),
                    job_id: job_name(id),
                    text: text.clone(),
                    is_error: true,
                }])
                .await?;
            self.hooks.jobs.recovered_end(id, text);
        }
        Ok(())
    }

    /// Appends the system message that moves the transcript's effective prompt to the
    /// current sections, if they differ.
    async fn sync_prompt(&self) -> Result<(), Error> {
        if let Some(current) = self.hooks.prompt.sections() {
            self.hooks.sync_sections(&current).await?;
        }
        Ok(())
    }

    /// The user messages of requeued `submission` entries, rendered from their parts
    /// (skills re-expanded, images re-read) and queued under their submission ids so
    /// their `message` entries carry them.
    async fn requeued(&self, requeue: Vec<SeqEntry>) -> Vec<(SubmissionKind, QueuedMessage)> {
        let mut queued = Vec::new();
        for seq in requeue {
            if let Entry::Submission {
                submission_id,
                kind,
                parts,
                ..
            } = seq.entry
            {
                let message = AgentMessage::Llm(Message::User(UserMessage {
                    content: render(&parts, &self.hooks.prompt.skills, &self.tool_state).await,
                    timestamp: now_ms(),
                }));
                queued.push((
                    kind,
                    QueuedMessage {
                        message,
                        id: Some(submission_id),
                    },
                ));
            }
        }
        queued
    }

    /// Acts on the recovery plan; returns what to run when a run resumes or requeued
    /// input is left.
    async fn recover(
        &self,
        policy: RecoveryPolicy,
        artifacts_dir: &Path,
    ) -> Result<Option<Resumption>, Error> {
        let log = self.hooks.journal.view();
        let model = self.hooks.model();
        let replay_of = |call: &ToolCall| {
            self.hooks
                .prompt
                .tools
                .iter()
                .find(|tool| tool.name() == call.name)
                .map_or(Replay::Never, |tool| tool.replay(call))
        };
        let (run_id, outcome, reason) = match plan_recovery(
            &log.entries,
            &log.projection,
            &model,
            &replay_of,
            artifacts_dir,
        ) {
            RecoveryPlan::None => return Ok(None),
            RecoveryPlan::Requeue { requeue } => {
                if policy == RecoveryPolicy::MarkInterrupted {
                    return Ok(None);
                }
                let queued = self.requeued(requeue).await;
                if queued
                    .iter()
                    .any(|(kind, _)| matches!(kind, SubmissionKind::Prompt))
                {
                    self.hooks
                        .activity
                        .send_modify(|activity| activity.running = true);
                }
                return Ok(Some(Resumption::Requeue(queued)));
            }
            RecoveryPlan::Finish {
                run_id,
                outcome,
                reason,
            } => (run_id, outcome, reason),
            RecoveryPlan::RecoverResponse {
                run_id,
                attempt,
                response_id,
                requeue,
            } => {
                if policy == RecoveryPolicy::MarkInterrupted {
                    (run_id, store::RunOutcome::Interrupted, None)
                } else {
                    let at = log
                        .entries
                        .iter()
                        .position(|seq| seq.entry.id() == Some(response_id.as_str()))
                        .ok_or_else(|| Error::EntryNotFound(response_id.to_string()))?;
                    let Entry::Message {
                        message: AgentMessage::Llm(Message::Assistant(response)),
                        ..
                    } = &log.entries[at].entry
                    else {
                        return Err(Error::InvalidHistory(
                            "Recovery response is not an assistant".into(),
                        ));
                    };
                    let response = response.clone();
                    // A silent successful Stop may have committed its maintenance
                    // checkpoint just before the crash, but not its run_end.
                    if response.stop_reason == StopReason::Stop
                        && response.tool_calls().next().is_none()
                        && requeue.is_empty()
                    {
                        let state = SessionHooks::failure_state(&log, &response, &model)?;
                        let completed = self.hooks.checkpoint_completed(&log, &state, &model);
                        let later_input = log.projection.path().iter().any(|&index| {
                            index > at
                                && matches!(
                                    &log.entries[index].entry,
                                    Entry::Message {
                                        message: AgentMessage::Llm(Message::User(_))
                                            | AgentMessage::Custom(_),
                                        ..
                                    }
                                )
                        });
                        if !state.hidden && completed && !later_input {
                            self.hooks
                                .append(vec![Entry::RunEnd {
                                    base: journal::base(),
                                    run_id,
                                    outcome: store::RunOutcome::Done,
                                    reason: None,
                                }])
                                .await?;
                            return Ok(None);
                        }
                    }
                    let results = log.entries[at + 1..]
                        .iter()
                        .take_while(|seq| {
                            !matches!(
                                &seq.entry,
                                Entry::Message {
                                    message: AgentMessage::Llm(Message::Assistant(_)),
                                    ..
                                }
                            )
                        })
                        .filter_map(|seq| match &seq.entry {
                            Entry::Message {
                                message: AgentMessage::Llm(Message::ToolResult(result)),
                                ..
                            } if response
                                .tool_calls()
                                .any(|call| call.id == result.tool_call_id) =>
                            {
                                Some(result.clone())
                            }
                            _ => None,
                        })
                        .collect();
                    drop(log);
                    self.hooks
                        .append(vec![Entry::RunResumed {
                            base: journal::base(),
                            run_id: run_id.clone(),
                            attempt,
                        }])
                        .await?;
                    self.hooks.live().run_id = Some(run_id);
                    self.hooks
                        .activity
                        .send_modify(|activity| activity.running = true);
                    return Ok(Some(Resumption::RecoverResponse {
                        response,
                        model,
                        results,
                        queued: self.requeued(requeue).await,
                    }));
                }
            }
            RecoveryPlan::Park { run_id } => (
                run_id,
                store::RunOutcome::Interrupted,
                Some("restart_limit".to_owned()),
            ),
            RecoveryPlan::Resume {
                run_id,
                attempt,
                interrupted,
                rerun,
                requeue,
            } => {
                let mut calls = interrupted;
                if policy == RecoveryPolicy::MarkInterrupted {
                    // Calls that would have been rerun are not run either.
                    calls.extend(rerun.into_iter().map(|call| (call, None)));
                }
                for (call, artifact) in calls {
                    let mut text = format!(
                        "Tool {} was interrupted and may have partially run.",
                        call.name
                    );
                    if let Some(n) = artifact {
                        text.push_str(&format!(" Output so far: artifact://{n}"));
                    }
                    let message = AgentMessage::Llm(Message::ToolResult(ToolResultMessage {
                        tool_call_id: call.id,
                        tool_name: call.name,
                        content: vec![UserBlock::Text { text }],
                        details: None,
                        is_error: true,
                        timestamp: now_ms(),
                        duration_ms: None,
                    }));
                    self.hooks
                        .append(vec![Entry::Message {
                            base: journal::base(),
                            message,
                            submission_id: None,
                            admitted_jobs: Vec::new(),
                            delivered_jobs: Vec::new(),
                        }])
                        .await?;
                }
                if policy == RecoveryPolicy::MarkInterrupted {
                    (run_id, store::RunOutcome::Interrupted, None)
                } else {
                    self.hooks
                        .append(vec![Entry::RunResumed {
                            base: journal::base(),
                            run_id: run_id.clone(),
                            attempt,
                        }])
                        .await?;
                    self.hooks.live().run_id = Some(run_id);
                    self.hooks
                        .activity
                        .send_modify(|activity| activity.running = true);
                    return Ok(Some(Resumption::Continue(self.requeued(requeue).await)));
                }
            }
        };
        self.hooks
            .append(vec![Entry::RunEnd {
                base: journal::base(),
                run_id,
                outcome,
                reason,
            }])
            .await?;
        Ok(None)
    }

    /// The session file. With lazy creation it exists only after the first submission.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The header id (UUIDv7), known before the session file exists.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The skills loaded at start; the prompt's catalog and `/name` commands use these.
    pub fn skills(&self) -> &[Skill] {
        &self.hooks.prompt.skills
    }

    /// The background jobs `read proc://` lists (promoted, retained and rebuilt,
    /// finished ones included), ascending.
    pub fn jobs(&self) -> Vec<JobInfo> {
        self.hooks
            .jobs
            .list()
            .iter()
            .map(|job| job.info())
            .collect()
    }

    /// Stops job `id` (`bg_N`), like `write proc://bg_N/kill`; false when it is unknown
    /// or no longer running.
    pub fn kill_job(&self, id: &str) -> bool {
        parse_job_id(id).is_some_and(|id| self.hooks.jobs.kill(id))
    }

    /// This session's URL schemes, for hosts resolving URLs they display: recognize with
    /// `exact`, then `resolve`, and `read` a `Target::Virtual`. A fork resolves to its own files.
    pub fn schemes(&self) -> &Schemes {
        &self.tool_state.schemes
    }

    /// The commands a host may offer, in whatever form it picks: built-ins, then this
    /// session's skills.
    pub fn commands(&self) -> Vec<CommandInfo> {
        commands::catalog(&self.hooks.prompt.skills)
    }

    /// Runs a built-in command under its policy: `model`/`thinking` alongside a turn,
    /// `compact`/`shake`/`navigate` only when idle (`Error::Busy` otherwise).
    pub async fn run_command(&self, command: Command) -> Result<(), Error> {
        match command {
            Command::Compact { focus } => self.compact(focus).await,
            Command::Shake { mode } => self.shake(mode).await,
            Command::Model { model } => {
                let model = self
                    .models
                    .find(&model)
                    .ok_or_else(|| Error::ModelNotFound(model.clone()))?
                    .clone();
                self.set_model(model).await
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

    /// Expands a `/skill args` invocation, saves the submission and runs the prompt;
    /// returns when the run settles. `Error::Busy` while a run is active (use
    /// `steer`/`follow_up`). Prompt text never runs a built-in command: hosts that offer
    /// typed commands parse them with `Command::parse` and call `run_command`.
    pub async fn prompt(&self, input: impl Into<PromptInput>) -> Result<RunOutcome, Error> {
        match self
            .prompt_with(PromptRequest {
                input: input.into(),
                request_id: None,
            })
            .await?
        {
            PromptResult::Settled { outcome, .. } => Ok(outcome),
            PromptResult::Duplicate(_) => unreachable!("a prompt without request id is new"),
        }
    }

    /// `prompt`, deduplicated by `request_id`: a request id that was already submitted
    /// returns its earlier submission and starts nothing, even while that run is active.
    pub async fn prompt_with(&self, request: PromptRequest) -> Result<PromptResult, Error> {
        let request_id = request.request_id.as_deref();
        // Fast path: a duplicate or busy prompt does no file or image work.
        if let Some(existing) = self.admit(request_id).await? {
            return Ok(PromptResult::Duplicate(existing));
        }
        self.hooks.preparing.fetch_add(1, Ordering::AcqRel);
        let preparing = Preparing(self.hooks.clone());
        let (Prepared { parts, saved }, content) = self.prepare(request.input.parts).await?;
        let (entry, message) = submission(
            SubmissionKind::Prompt,
            parts,
            content,
            request.request_id.clone(),
        );
        let mut entry = Some(entry);
        let mut duplicate = None;
        let mut queued = None;
        self.hooks.activity.send_if_modified(|activity| {
            // Checked again: another prompt may have been submitted while this one prepared.
            duplicate = submitted(&self.hooks.journal.view(), request_id);
            if duplicate.is_some() || activity.running {
                return false;
            }
            activity.running = true;
            // Queued while claiming, so a racing prompt's barrier (`busy_or_duplicate`)
            // lands after it. No maintenance runs here: the session was idle.
            queued = entry
                .take()
                .map(|entry| self.hooks.journal.append(vec![entry]));
            true
        });
        let Some(queued) = queued else {
            return match duplicate {
                Some(existing) => Ok(PromptResult::Duplicate(existing)),
                None => self
                    .busy_or_duplicate(request_id)
                    .await
                    .map(PromptResult::Duplicate),
            };
        };
        let submission = match queued.await {
            Ok(mut appended) => appended.remove(0),
            Err(error) => {
                self.hooks.release_or_deliver();
                return Err(error);
            }
        };
        saved.keep();
        drop(preparing);
        self.hooks.auto_paused.store(false, Ordering::Release);
        // Pending job results precede the user's message.
        let messages = self
            .hooks
            .run_input(self.hooks.reserve_delivery(), Some(message));
        // Spawned so that dropping this future leaves the run to settle on its own.
        let run = tokio::spawn(run_to_settle(
            self.agent.clone(),
            self.hooks.clone(),
            RunRequest::Prompt(messages),
        ));
        let outcome = match run.await {
            Ok(result) => result?,
            Err(join) => std::panic::resume_unwind(join.into_panic()),
        };
        Ok(PromptResult::Settled {
            submission,
            outcome,
        })
    }

    /// `Some(existing)` when `request_id` was already submitted; `Error::Busy` while a run is active.
    async fn admit(&self, request_id: Option<&str>) -> Result<Option<SeqEntry>, Error> {
        if let Some(existing) = submitted(&self.hooks.journal.view(), request_id) {
            return Ok(Some(existing));
        }
        if !self.hooks.activity.borrow().running {
            return Ok(None);
        }
        self.busy_or_duplicate(request_id).await.map(Some)
    }

    /// While something else owns the session: the submission of `request_id` if the
    /// owner is a prompt that carries it, else `Error::Busy`.
    async fn busy_or_duplicate(&self, request_id: Option<&str>) -> Result<SeqEntry, Error> {
        let Some(request_id) = request_id else {
            return Err(Error::Busy);
        };
        // An empty batch: once it returns, every submission queued before it is written
        // (or failed).
        let _ = self.hooks.journal.append(Vec::new()).await;
        submitted(&self.hooks.journal.view(), Some(request_id)).ok_or(Error::Busy)
    }

    /// Validates `parts`, saves handed-over content, and renders the model blocks; runs
    /// before anything is written so file and image work never stalls a running turn.
    async fn prepare(&self, parts: Vec<PromptPart>) -> Result<(Prepared, Vec<UserBlock>), Error> {
        let prepared = prepare(parts, &self.tool_state).await?;
        let content = render(&prepared.parts, &self.hooks.prompt.skills, &self.tool_state).await;
        Ok((prepared, content))
    }

    /// Saves and queues a user message delivered after the current tool calls finish.
    pub async fn steer(&self, input: impl Into<PromptInput>) -> Result<(), Error> {
        self.queue(SubmissionKind::Steer, input.into()).await
    }

    /// Saves and queues a user message delivered when the run would otherwise end.
    pub async fn follow_up(&self, input: impl Into<PromptInput>) -> Result<(), Error> {
        self.queue(SubmissionKind::FollowUp, input.into()).await
    }

    /// `steer`/`follow_up`: saves the submission and hands its message to the agent's
    /// queue for `kind`. `saved` files stay only once the entry is written.
    async fn queue(&self, kind: SubmissionKind, input: PromptInput) -> Result<(), Error> {
        let (Prepared { parts, saved }, content) = self.prepare(input.parts).await?;
        let (entry, message) = submission(kind, parts, content, None);
        // Awaits only the journal, which never waits on the session.
        let _order = self.hooks.queue_order.lock().await;
        self.hooks.append(vec![entry]).await?;
        saved.keep();
        self.hooks.auto_paused.store(false, Ordering::Release);
        match kind {
            SubmissionKind::FollowUp => self.agent.follow_up(message),
            SubmissionKind::Prompt | SubmissionKind::Steer => self.agent.steer(message),
        }
        Ok(())
    }

    /// Stops the active run (`CancelReason::User`). Stopping an active run pauses
    /// automatic delivery of job results until the user's next input; an idle Stop
    /// changes nothing.
    pub async fn abort(&self) {
        if self.hooks.activity.borrow().running {
            self.hooks.auto_paused.store(true, Ordering::Release);
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
        self.hooks.jobs.shutdown().await;
    }

    /// Switches the model from the next request; the thinking level is clamped to it.
    pub async fn set_model(&self, model: Model) -> Result<(), Error> {
        if self.hooks.activity.borrow().maintenance.is_some() {
            return Err(Error::Busy);
        }
        let mut queued = None;
        // Queued under the selection lock, so the journal records changes in the order
        // they took effect.
        self.hooks.selection.send_modify(|selection| {
            let level = clamp_level(&model, selection.thinking);
            let mut entries = vec![Entry::ModelChange {
                base: journal::base(),
                provider: model.provider.clone(),
                model_id: model.id.clone(),
            }];
            if level != selection.thinking {
                entries.push(Entry::ThinkingLevelChange {
                    base: journal::base(),
                    thinking_level: level,
                });
                self.agent.set_thinking(level);
            }
            queued = Some(self.hooks.append(entries));
            self.agent.set_model(model.clone());
            *selection = Selection {
                model,
                thinking: level,
            };
        });
        if let Some(queued) = queued {
            queued.await?;
        }
        Ok(())
    }

    /// Sets the thinking level from the next request, clamped to the model; returns the
    /// level applied.
    pub async fn set_thinking(&self, level: ThinkingLevel) -> Result<ThinkingLevel, Error> {
        if self.hooks.activity.borrow().maintenance.is_some() {
            return Err(Error::Busy);
        }
        let mut applied = level;
        let mut queued = None;
        self.hooks.selection.send_if_modified(|selection| {
            applied = clamp_level(&selection.model, level);
            if applied == selection.thinking {
                return false;
            }
            queued = Some(self.hooks.append(vec![Entry::ThinkingLevelChange {
                base: journal::base(),
                thinking_level: applied,
            }]));
            self.agent.set_thinking(applied);
            selection.thinking = applied;
            true
        });
        if let Some(queued) = queued {
            queued.await?;
        }
        Ok(applied)
    }

    /// Settles the pending question of tool call `call_id`.
    pub fn answer(&self, call_id: &str, answers: Vec<Answer>) -> Result<(), Error> {
        self.questions.answer(call_id, answers)
    }

    /// Dismisses the pending question of tool call `call_id`; the run is cancelled.
    pub fn dismiss(&self, call_id: &str) -> Result<(), Error> {
        self.questions.dismiss(call_id)?;
        self.hooks.cancel_maintenance(false);
        self.agent.cancel(CancelReason::User);
        Ok(())
    }

    /// The entries after `from` (all with `None`), the live state and an event receiver,
    /// all taken at one point of the append order: no entry is missed or duplicated.
    /// Never waits on a write in flight.
    pub async fn attach(&self, from: Option<u64>) -> Attach {
        let activity = *self.hooks.activity.borrow();
        let mut agent = self.agent.snapshot();
        agent.running = activity.running;
        // Entries publish, and live updates relay, under these two locks: subscribing
        // inside both splits every event into before (in the snapshot) or after
        // (received).
        self.hooks.journal.read(|view| {
            let live = self.hooks.live();
            let events = self.hooks.events.subscribe();
            let start = from.map_or(0, |seq| {
                view.entries.partition_point(|entry| entry.seq <= seq)
            });
            agent.streaming = live.streaming.as_ref().and_then(Accumulator::snapshot);
            agent.pending_tool_calls = live.tool_calls.clone();
            Attach {
                missed: view.entries[start..].to_vec(),
                live: LiveSnapshot {
                    run_id: live.run_id.clone(),
                    maintenance: activity.maintenance,
                    agent,
                    messages: view.projection.messages.clone(),
                    pending_questions: self.questions.pending(),
                    jobs: self.jobs(),
                },
                events,
            }
        })
    }

    async fn start_maintenance(
        &self,
        kind: MaintenanceKind,
        operation: MaintenanceOperation,
    ) -> Result<(), Error> {
        let cancel = CancellationToken::new();
        self.hooks.reserve(kind, true, cancel.clone())?;
        let task = tokio::spawn(self.hooks.clone().standalone(
            self.agent.clone(),
            operation,
            cancel,
        ));
        match task.await {
            Ok(result) => result,
            Err(join) => std::panic::resume_unwind(join.into_panic()),
        }
    }

    /// Summarizes eligible history without submitting an ordinary user turn; `focus` tells
    /// the summary what to prioritize (blank focus counts as none).
    pub async fn compact(&self, focus: Option<String>) -> Result<(), Error> {
        self.start_maintenance(
            MaintenanceKind::Compact,
            MaintenanceOperation::Compact {
                focus: normalize_focus(focus),
            },
        )
        .await
    }

    /// Removes eligible blocks into recoverable session artifacts, without model calls.
    pub async fn shake(&self, mode: ShakeMode) -> Result<(), Error> {
        self.start_maintenance(MaintenanceKind::Shake, MaintenanceOperation::Shake(mode))
            .await
    }

    /// Moves the session's position to `target` (`None` = before the first entry). Without
    /// `options.summarize` this appends a `leaf` entry; with it, the abandoned branch is
    /// summarized first (steered by `options.focus`) and a `branch_summary` entry at `target`
    /// replaces the leaf. The next prompt continues from there with the live model and
    /// thinking level, recorded on the branch when it implies others.
    pub async fn navigate(
        &self,
        target: Option<EntryId>,
        options: NavigateOptions,
    ) -> Result<(), Error> {
        let focus = normalize_focus(options.focus);
        if options.summarize {
            let sections = self.hooks.prompt.sections();
            return self
                .start_maintenance(
                    MaintenanceKind::BranchSummary,
                    MaintenanceOperation::Navigate {
                        target,
                        sections,
                        focus,
                    },
                )
                .await;
        }
        if focus.is_some() {
            return Err(Error::InvalidInput(
                "navigate focus requires summarize".into(),
            ));
        }
        // Owns the session like a run, so no prompt starts from the old position.
        if !self.hooks.claim_idle() {
            return Err(Error::Busy);
        }
        let _claim = Claim(&self.hooks);
        if let Some(target) = &target
            && !self.hooks.journal.view().has(target.as_str())
        {
            return Err(Error::EntryNotFound(target.to_string()));
        }
        self.hooks
            .append(vec![Entry::Leaf {
                base: journal::base_at(target),
            }])
            .await?;
        self.hooks.record_current_selection().await?;
        // Pending diffs were relative to the old position; `sync_prompt` recomputes them.
        self.hooks.take_system();
        self.sync_prompt().await?;
        self.agent
            .set_messages(self.hooks.journal.view().projection.messages.clone());
        Ok(())
    }

    /// Copies the root→`entry_id` path into a new session file that records this one as
    /// its parent. The fork runs with this session's resolved configuration. Forking at a
    /// user message copies the path to its parent and returns the message's parts to edit
    /// and resubmit.
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
                            text: user
                                .content
                                .iter()
                                .filter_map(|block| match block {
                                    UserBlock::Text { text } => Some(text.as_str()),
                                    UserBlock::Image { .. } => None,
                                })
                                .collect(),
                        }]
                    });
                    (base.parent_id.as_ref().map(EntryId::as_str), Some(parts))
                }
                entry => (entry.id(), None),
            };
            let path = next
                .and_then(|id| {
                    log.entries
                        .iter()
                        .rposition(|seq| seq.entry.id() == Some(id))
                })
                .map_or_else(Vec::new, |at| {
                    store::project::path_to(&log.entries, at)
                        .into_iter()
                        .map(|index| log.entries[index].entry.clone())
                        .collect()
                });
            (path, editable)
        };

        let header = Header::new(
            self.hooks.prompt.cwd.clone(),
            Some(self.path.clone()),
            Some(entry_id.clone()),
        );
        let session_id = header.id.clone();
        let mut writer = self.store.create(header).await?;
        let parent_dir = self.path.with_extension("");
        let child_dir = writer.planned_path().with_extension("");
        crate::schemes::copy_for_fork(&parent_dir, &child_dir, self.hooks.jobs.live_spools()).await;
        let mut entries = Vec::with_capacity(path.len() + 1);
        for entry in path {
            entries.push(writer.append(entry).await?);
        }
        // A run open at the fork point continues nowhere: close it so opening the fork
        // does not try to recover it.
        let fork_projection = project(&entries, None);
        let open = match plan_recovery(
            &entries,
            &fork_projection,
            &model,
            &|_| Replay::Never,
            Path::new(""),
        ) {
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
                        run_id,
                        outcome,
                        reason,
                    })
                    .await?,
            );
        }

        let projection = project(&entries, None);
        let session = Session::start(SessionSetup {
            cwd: self.hooks.prompt.cwd.clone(),
            // The parent's resolved config: a fork never samples the configuration source.
            config: self.config.clone(),
            docs_dir: self.hooks.prompt.docs_dir.clone(),
            streamer: self.streamer.clone(),
            models: self.models.clone(),
            model,
            thinking,
            exec: self.exec.clone(),
            supervisor: self.hooks.maintenance.tools.supervisor.clone(),
            ask_enabled: self.ask_enabled,
            host_sections: self.hooks.prompt.host_sections.clone(),
            host_tools: self.host_tools.clone(),
            host_schemes: self.host_schemes.clone(),
            store: self.store.clone(),
            session_id,
            writer,
            entries,
            projection,
            recovery: None,
        })
        .await?;
        Ok(Fork { session, editable })
    }

    /// Drops the cached project environment and resolves it again now. When its prompt
    /// note or the date changed, the prompt diff reaches the model before its next request.
    pub async fn reload_exec_env(&self) {
        self.exec.invalidate();
        let resolved = self
            .exec
            .resolve(&self.hooks.prompt.cwd, &CancellationToken::new(), &|_| {})
            .await;
        self.hooks.refresh_prompt(resolved.prompt_note);
    }
}

/// Dropping a session shuts it down: its run task then settles and releases the writer
/// lock, and every command job's container is killed.
impl Drop for Session {
    fn drop(&mut self) {
        self.agent.shutdown();
        self.hooks.cancel_maintenance(true);
        self.hooks.jobs.kill_all();
    }
}

/// The `submission` entry for `parts` and the user message of `content`, queued under the
/// submission id so its `message` entry carries it.
fn submission(
    kind: SubmissionKind,
    parts: Vec<PromptPart>,
    content: Vec<UserBlock>,
    request_id: Option<String>,
) -> (Entry, QueuedMessage) {
    let submission_id = Uuid::now_v7().to_string();
    let entry = Entry::Submission {
        base: journal::base(),
        submission_id: submission_id.clone(),
        kind,
        parts,
        request_id,
    };
    let message = AgentMessage::Llm(Message::User(UserMessage {
        content,
        timestamp: now_ms(),
    }));
    (
        entry,
        QueuedMessage {
            message,
            id: Some(submission_id),
        },
    )
}

/// The submission of `request_id` in `view`, if any.
fn submitted(view: &View, request_id: Option<&str>) -> Option<SeqEntry> {
    let request_id = request_id?;
    view.entries
        .iter()
        .find(|seq| {
            matches!(&seq.entry, Entry::Submission { request_id: Some(id), .. } if id == request_id)
        })
        .cloned()
}
