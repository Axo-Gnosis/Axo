//! A conversation with one agent in one working directory, saved as it happens.

mod delivery;
pub(crate) mod journal;
mod maintenance;
mod prompt;
mod resume;
mod side_files;

use std::borrow::Cow;
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
    ToolCall, ToolResultMessage, UserBlock,
};
use futures::FutureExt;
use serde_json::Value;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use self::delivery::{Activity, enqueue, relay_jobs};
use self::journal::Journal;
use self::maintenance::Maintenance;
use self::prompt::{PromptState, SessionPrompt};
use self::resume::Resumption;
use self::side_files::{ARTIFACTS_DIR, LOCAL_DIR, copy_for_fork};
use crate::commands::{self, Command, CommandInfo};
use crate::compaction::{self, Limits};
use crate::config::{EffectiveConfig, ProjectAccess, PromptOverride};
use crate::context::agents::{AgentDef, AgentSpec, ContextFiles};
use crate::context::files::{NestedContext, context_file_names, load_context_files};
use crate::context::prompt::PromptSources;
use crate::context::skills::{Skill, load_skills};
use crate::context::{Diagnostic, DiagnosticOrigin, home_agents_dir, project_root};
use crate::error::Error;
use crate::exec::ExecEnv;
use crate::exec::host::ProcessSupervisor;
use crate::input::{PromptInput, PromptPart};
use crate::jobs::{JobHost, JobInfo, JobRegistry, parse_job_id};
use crate::recovery::{self, RecoveryPlan, plan_recovery};
use crate::schemes::artifact::Artifacts;
use crate::schemes::dir::{DirScheme, ensure_bundled_docs};
use crate::schemes::proc::ProcScheme;
use crate::schemes::skill::SkillScheme;
use crate::schemes::{Scheme, SchemeContext, Schemes};
use crate::store::{
    self, CompactionReason, Entry, EntryBase, EntryId, Header, Projection, SeqEntry, SessionStore,
    SessionWriter, SubmissionKind, project,
};
use crate::tools::{self, Answer, PendingQuestions, Question, SessionTools, ToolFactory};

pub struct SessionOptions {
    /// Ignored by `Axo::open_session`, which uses the cwd in the session header.
    pub cwd: PathBuf,
    /// A model selector (`provider/model`, `@role`, either with `:thinking`, or `:thinking` alone);
    /// falls back to the session's last model, then the root agent's `model`, then `defaultModel`.
    pub model: Option<String>,
    /// The root agent, by name, from the agent roots or `agents`. `None` runs without one.
    /// Pass it again when reopening, as with `sections`; forks inherit it.
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
}

/// A new session copied from a point of another.
pub struct Fork {
    pub session: Session,
    /// The parts of the user message forked at, for the client to edit and resubmit.
    pub editable: Option<Vec<PromptPart>>,
}

/// Immutable host inputs inherited by forks and child sessions.
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
    /// Held across a selection change's write and its apply, so changes take effect in
    /// journal order and only once durable.
    selection_order: tokio::sync::Mutex<()>,
    maintenance: Maintenance,
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
        let level = clamp_level(&model, thinking.unwrap_or(current.thinking));
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
        self.append(entries).await?;
        if level != current.thinking {
            agent.set_thinking(level);
        }
        agent.set_model(model.clone());
        self.selection.send_replace(Selection {
            model,
            thinking: level,
        });
        Ok(())
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
        _context: &AgentContext,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<AgentMessage>>, HookError> {
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
                    continue_turn: false,
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
                    continue_turn: false,
                    messages,
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
    ) -> Vec<AgentMessage> {
        self.take_system()
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
    async fn persist(&self, event: &AgentEvent) -> Result<(), PersistError> {
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
        let _ = self.events.send(SessionEvent::Agent(event.clone()));
        Ok(())
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
    name: &str,
    cx: SchemeContext,
    host: &[Arc<dyn Scheme>],
    jobs: Option<Arc<JobRegistry>>,
) -> Result<(Schemes, Vec<Skill>, Vec<Diagnostic>, bool), Error> {
    let settings = &config.settings;
    let home_agents = settings.home_agents_dir.then(home_agents_dir).flatten();
    let (skills, mut diagnostics) = load_skills(
        &config.skill_roots,
        home_agents.as_deref(),
        &cx.cwd,
        config.access != ProjectAccess::Ignore && settings.project.files.skills,
    );
    let mut builtins: Vec<Arc<dyn Scheme>> = vec![
        Arc::new(SkillScheme::new(&skills)),
        Arc::new(DirScheme::local(cx.session_dir.join(LOCAL_DIR))),
    ];
    let docs = docs_dir.is_some_and(|docs_dir| match ensure_bundled_docs(docs_dir, name) {
        Ok(()) => {
            builtins.push(Arc::new(DirScheme::docs(name, docs_dir.to_path_buf())));
            true
        }
        Err(error) => {
            diagnostics.push(Diagnostic {
                origin: Some(DiagnosticOrigin::Path(docs_dir.to_path_buf())),
                message: error.to_string(),
            });
            false
        }
    });
    if let Some(jobs) = jobs {
        builtins.push(Arc::new(ProcScheme { jobs }));
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
        let context_names = match root_agent.and_then(|spec| spec.context_files.as_ref()) {
            Some(ContextFiles::Off) => None,
            Some(ContextFiles::Names(overrides)) => {
                let mut names = settings.context_files.clone();
                names.extend(overrides.iter().map(|(name, on)| (name.clone(), *on)));
                Some(Cow::Owned(names))
            }
            None => Some(Cow::Borrowed(&settings.context_files)),
        };
        let (context_files, context, name_diagnostics, file_diagnostics) = match context_names {
            None => (
                Vec::new(),
                NestedContext::disabled(),
                Vec::new(),
                Vec::new(),
            ),
            Some(context_names) => {
                let home_agents = settings.home_agents_dir.then(home_agents_dir).flatten();
                let (names, name_diagnostics) = context_file_names(&context_names);
                let (context_files, file_diagnostics) = load_context_files(
                    &config.context_files,
                    home_agents.as_deref(),
                    cwd,
                    project,
                    &names,
                );
                let context = NestedContext::new(
                    (project && settings.nested_context_files).then(|| project_root(cwd)),
                    names,
                    &context_files,
                );
                (context_files, context, name_diagnostics, file_diagnostics)
            }
        };
        let (jobs, unfinished) = JobRegistry::new(session_id.clone(), &entries);
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
            Some(jobs.clone()),
        )?;
        let (summary_prompts, summary_diagnostics) =
            compaction::summary::SummaryPrompts::load(&config.prompts, &inherited.harness_name);
        let mut section_diagnostics = Vec::new();
        let agent_sections = root_agent
            .into_iter()
            .flat_map(|spec| &spec.sections)
            .filter(|(_, enabled)| !**enabled)
            .map(|(name, _)| (name.clone(), None));
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
            .cloned();
        let host_sections = agent_sections.chain(validated_host_sections).collect();
        let mut prompts = config.prompts.clone();
        if let Some(spec) = root_agent.filter(|spec| !spec.body.is_empty()) {
            prompts.insert(
                "SYSTEM".into(),
                PromptOverride {
                    origin: spec.path.as_ref().map_or_else(
                        || format!("agent {}", spec.name),
                        |path| path.display().to_string(),
                    ),
                    text: spec.body.clone(),
                },
            );
        }

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
            context,
            schemes,
            runner,
        ));
        let host: Vec<Arc<dyn Tool>> = host_tools
            .iter()
            .map(|factory| factory(&session_tools))
            .collect();
        let mut tools = tools::merge_tools(
            tools::builtin(
                &session_tools,
                inherited.ask_enabled,
                questions.clone(),
                settings.ask.timeout_ms.map(Duration::from_millis),
            ),
            &host,
        );
        let mut tool_diagnostics = Vec::new();
        if let Some(spec) = root_agent
            && let Some(allow) = &spec.tools
        {
            for name in allow {
                if !tools.iter().any(|tool| tool.name() == name) {
                    tool_diagnostics.push(Diagnostic {
                        origin: spec.path.clone().map(DiagnosticOrigin::Path),
                        message: format!("agent \"{}\": unknown tool \"{name}\"", spec.name),
                    });
                }
            }
            tools.retain(|tool| allow.iter().any(|name| name == tool.name()));
        }
        let diagnostics: Vec<_> = name_diagnostics
            .into_iter()
            .chain(file_diagnostics)
            .chain(skill_diagnostics)
            .chain(commands::reserved_skill_diagnostics(&skills))
            .chain(summary_diagnostics)
            .chain(section_diagnostics)
            .chain(inherited.agent_diagnostics.iter().cloned())
            .chain(tool_diagnostics)
            .collect();
        for diagnostic in &diagnostics {
            warn!("{diagnostic}");
        }

        let journal = Journal::start(writer, entries, projection.clone(), events.clone());
        let hooks = Arc::new_cyclic(|this: &Weak<SessionHooks>| {
            jobs.attach(this.clone() as Weak<dyn JobHost>);
            SessionHooks {
                events,
                inherited: inherited.clone(),
                session_id: session_id.clone(),
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
                selection_order: tokio::sync::Mutex::new(()),
                maintenance: Maintenance {
                    settings: settings.compaction.clone(),
                    prompts: summary_prompts,
                    cancel: Mutex::new(None),
                },
                preparing: AtomicUsize::new(0),
                prompt: SessionPrompt {
                    sources: PromptSources {
                        name: inherited.harness_name.clone(),
                        cwd: cwd.clone(),
                        prompts,
                        docs,
                        tools: tools.clone(),
                        scheme_docs: session_tools.schemes.prompt_docs(),
                        context_files,
                        skills,
                        host_sections,
                    },
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
            }
        });
        tokio::spawn(relay_jobs(
            Arc::downgrade(&hooks.tools.runner.jobs),
            hooks.tools.runner.jobs.subscribe(),
            hooks.events.clone(),
        ));
        let agent = Arc::new(Agent::new(AgentConfig {
            thinking,
            tools,
            hooks: hooks.clone(),
            tool_hooks: inherited.tool_hooks.clone(),
            persist: Some(hooks.clone()),
            steering_mode: settings.steering_mode,
            follow_up_mode: settings.follow_up_mode,
            session_id: Some(session_id.clone()),
            ..AgentConfig::new(inherited.streamer.clone(), model)
        }));
        let _ = hooks.agent.set(Arc::downgrade(&agent));

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
            .record_selection(projection.model.as_deref(), projection.thinking)
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
                    if has_turns {
                        enqueue(agent, kind, message);
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

    /// The session file. With lazy creation it exists only after the first submission.
    pub fn path(&self) -> &Path {
        &self.hooks.path
    }

    /// The header id (UUIDv7), known before the session file exists.
    pub fn id(&self) -> &str {
        &self.hooks.session_id
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

    /// Stops job `id` (`bg_N`), like `write proc://bg_N/kill`; false when it is unknown
    /// or no longer running.
    pub fn kill_job(&self, id: &str) -> bool {
        parse_job_id(id).is_some_and(|id| self.hooks.tools.runner.jobs.kill(id))
    }

    /// This session's URL schemes, for hosts resolving URLs they display: recognize with
    /// `exact`, then `resolve`, and `read` a `Target::Virtual`. A fork resolves to its own files.
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
        self.hooks.tools.runner.jobs.shutdown().await;
    }

    /// Switches the model from the next request; the thinking level is clamped to it.
    pub async fn set_model(&self, model: Model) -> Result<(), Error> {
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move { hooks.select_model(&agent, model, None).await }).await
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

    /// The entries after `from` (all with `None`), the live state and an event receiver,
    /// all taken at one point of the append order: no entry is missed or duplicated.
    /// Never waits on a write in flight.
    pub async fn attach(&self, from: Option<u64>) -> Attach {
        let activity = *self.hooks.activity.borrow();
        // Entries publish, and live updates relay, under these two locks: subscribing
        // inside both splits every event into before (in the snapshot) or after
        // (received).
        self.hooks.journal.read(|view| {
            let live = self.hooks.live();
            let (queued_steering, queued_follow_up) = self.agent.queued();
            let events = self.hooks.events.subscribe();
            let start = from.map_or(0, |seq| {
                view.entries.partition_point(|entry| entry.seq <= seq)
            });
            Attach {
                missed: view.entries[start..].to_vec(),
                live: LiveSnapshot {
                    run_id: live.run_id.clone(),
                    maintenance: activity.maintenance,
                    running: activity.running,
                    streaming: live.streaming.as_ref().and_then(Accumulator::snapshot),
                    pending_tool_calls: live.tool_calls.clone(),
                    queued_steering,
                    queued_follow_up,
                    messages: view.projection.messages.clone(),
                    pending_questions: self.hooks.questions.pending(),
                    jobs: self.jobs(),
                },
                events,
            }
        })
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
                        run_id,
                        outcome,
                        reason,
                    })
                    .await?,
            );
        }

        let projection = project(&entries, None);
        let session = Session::start(SessionSetup {
            inherited: self.inherited.clone(),
            model,
            thinking,
            session_id,
            writer,
            entries,
            projection,
            recovery: None,
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
    }
}
