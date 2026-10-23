//! A conversation with one agent in one working directory, saved as it happens.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{
    Agent, AgentConfig, AgentContext, AgentEvent, AgentMessage, AgentSnapshot, CancelReason,
    HookError, LoopHooks, Persist, PersistError, PrepareRequestContext, Replay, RequestUpdate,
    RunOutcome, Tool, TurnContext, TurnDecision, TurnFinish, TurnUpdate,
};
use axo_ai::thinking::clamp_level;
use axo_ai::{
    Accumulator, AssistantMessage, Message, Model, StopReason, Streamer, SystemMessage,
    ThinkingLevel, ToolCall, ToolResultMessage, ToolSpec, Usage, UserBlock, UserMessage, now_ms,
};
use futures::FutureExt;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use crate::compaction::{self, Limits};
use crate::config::{AxoDir, CompactionSettings, ProjectAccess, Settings, SummaryRequest};
use crate::context::files::{ContextFile, NestedContext, context_file_names, load_context_files};
use crate::context::prompt::{PromptInputs, Sections, build_sections, system_message};
use crate::context::skills::{Skill, expand_skill_command, load_skills};
use crate::context::{home_agents_dir, project_root};
use crate::error::Error;
use crate::exec::ExecEnv;
use crate::recovery::{self, RecoveryPlan, plan_recovery};
use crate::store::{
    self, Entry, EntryBase, EntryId, Header, Projection, SeqEntry, SessionStore, SessionWriter,
    SubmissionKind, project,
};
use crate::tools::ask::{Answer, PendingQuestions, Question};
use crate::tools::{self, SessionTools};

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShakeMode {
    #[default]
    Elide,
    Images,
    Thinking,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NavigateOptions {
    pub summarize: bool,
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
}

/// A prompt with an optional client id that makes resubmission idempotent.
#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub text: String,
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
    /// The projected conversation at this point in the append order, from the log-owned cache.
    pub messages: Vec<AgentMessage>,
    /// Unanswered `ask` calls by tool-call id. A question raised while attaching can
    /// also arrive as a `QuestionPending` event (duplicated, never missed).
    pub pending_questions: Vec<(String, Vec<Question>)>,
}

/// A new session copied from a point of another.
pub struct Fork {
    pub session: Session,
    /// The text of the user message forked at, for the client to edit and resubmit.
    pub editable_text: Option<String>,
}

/// Everything `Axo` resolved for a new, opened or forked session.
pub(crate) struct SessionSetup {
    pub(crate) dir: AxoDir,
    pub(crate) cwd: PathBuf,
    /// User settings merged with the project's.
    pub(crate) settings: Settings,
    pub(crate) streamer: Arc<dyn Streamer>,
    pub(crate) model: Model,
    /// Already clamped to the model.
    pub(crate) thinking: ThinkingLevel,
    pub(crate) exec: Arc<dyn ExecEnv>,
    pub(crate) ask_enabled: bool,
    pub(crate) project_access: ProjectAccess,
    pub(crate) host_sections: Vec<(String, Option<String>)>,
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
    settings: Settings,
    streamer: Arc<dyn Streamer>,
    ask_enabled: bool,
    // Prompt inputs, kept to rebuild the prompt when the exec environment or date changes.
    dir: AxoDir,
    cwd: PathBuf,
    project_access: ProjectAccess,
    tools: Vec<Arc<dyn Tool>>,
    context_files: Vec<ContextFile>,
    skills: Vec<Skill>,
    host_sections: Vec<(String, Option<String>)>,
    prompt: Mutex<PromptState>,
}

/// The last prompt build and the exec-environment note and local date it used.
struct PromptState {
    note: Option<String>,
    date: String,
    /// `None` until the first build.
    sections: Option<Sections>,
}

fn today() -> String {
    jiff::Zoned::now().date().to_string()
}

/// The session's loop hooks, durable log and event relay.
struct SessionHooks {
    events: broadcast::Sender<SessionEvent>,
    /// System-message diffs not yet in the transcript: they go in before the next turn of
    /// an active run, or ahead of the next prompt.
    system: Mutex<Vec<AgentMessage>>,
    /// Every append, and every event relay, happens under this lock, so `attach` sees one
    /// consistent point.
    log: tokio::sync::Mutex<Log>,
    /// A run is active or about to start; guards against a second one. Changed only
    /// while holding the log lock.
    running: watch::Sender<bool>,
    maintenance: Maintenance,
}

enum MaintenanceOperation {
    Compact,
    Shake(ShakeMode),
    Navigate {
        target: Option<EntryId>,
        sections: Option<Sections>,
    },
}

/// Immutable operation inputs plus synchronous cancellation for Drop and dismiss.
struct Maintenance {
    streamer: Arc<dyn Streamer>,
    settings: CompactionSettings,
    prompts: compaction::summary::SummaryPrompts,
    session_id: String,
    path: PathBuf,
    artifacts_dir: PathBuf,
    tools: Arc<SessionTools>,
    tool_specs: Vec<ToolSpec>,
    skill_paths: Vec<PathBuf>,
    cancel: Mutex<Option<CancellationToken>>,
    shutdown: AtomicBool,
}

struct Log {
    writer: Box<dyn SessionWriter>,
    entries: Vec<SeqEntry>,
    projection: Projection,
    ids: HashSet<String>,
    model: Model,
    thinking: ThinkingLevel,
    /// The open run's id, set by its `run_start` (or by recovery).
    run_id: Option<String>,
    /// Submitted user messages not yet saved as `message` entries, with their
    /// submission ids. Matched by equality: identical messages are interchangeable.
    pending: Vec<(AgentMessage, String)>,
    // The live state as relayed so far; the agent's own copy updates after `persist`
    // returns, so `attach` reads these instead.
    streaming: Option<Accumulator>,
    tool_calls: Vec<ToolCall>,
    maintenance: Option<MaintenanceKind>,
    revision: u64,
}

impl Log {
    /// A base for the next entry: a fresh id, parented on the last entry (the current
    /// position, through a trailing `leaf` entry).
    fn next_base(&self) -> EntryBase {
        let parent = self
            .entries
            .last()
            .and_then(|last| last.entry.id())
            .map(|id| EntryId(id.to_owned()));
        EntryBase::new(EntryId::generate(|id| self.ids.contains(id)), parent)
    }
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
    Continue(Vec<(SubmissionKind, AgentMessage)>),
    /// No run was open: queue steers and follow-ups, and start a run with the prompts.
    Requeue(Vec<(SubmissionKind, AgentMessage)>),
    RecoverResponse {
        response: AssistantMessage,
        model: Model,
        results: Vec<ToolResultMessage>,
        queued: Vec<(SubmissionKind, AgentMessage)>,
    },
}

/// How `run_to_settle` starts its run.
#[allow(clippy::large_enum_variant)] // One owned startup value; boxing would add an allocation.
enum RunRequest {
    Prompt(Vec<AgentMessage>),
    Continue,
    RecoverResponse {
        response: AssistantMessage,
        model: Model,
        results: Vec<ToolResultMessage>,
    },
}

/// The user messages of requeued `submission` entries, registered in `log.pending` so
/// their `message` entries carry the submission ids.
fn requeued(log: &mut Log, requeue: Vec<SeqEntry>) -> Vec<(SubmissionKind, AgentMessage)> {
    let mut queued = Vec::new();
    for seq in requeue {
        if let Entry::Submission {
            submission_id,
            kind,
            content,
            ..
        } = seq.entry
        {
            let message = AgentMessage::Llm(Message::User(UserMessage {
                content,
                timestamp: now_ms(),
            }));
            log.pending.push((message.clone(), submission_id));
            queued.push((kind, message));
        }
    }
    queued
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
    agent.set_messages(hooks.log.lock().await.projection.messages.clone());
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
        log: &mut Log,
        kind: MaintenanceKind,
        standalone: bool,
        cancel: CancellationToken,
    ) -> Result<(), Error> {
        if log.maintenance.is_some() || (standalone && *self.running.borrow()) {
            return Err(Error::Busy);
        }
        if self.maintenance.shutdown.load(Ordering::Acquire) {
            return Err(Error::MaintenanceAborted);
        }
        log.maintenance = Some(kind);
        *self
            .maintenance
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(cancel);
        if standalone {
            self.running.send_replace(true);
        }
        let _ = self.events.send(SessionEvent::MaintenanceStart { kind });
        Ok(())
    }

    async fn end_maintenance(&self, standalone: bool, error: Option<String>, aborted: bool) {
        let mut log = self.log.lock().await;
        *self
            .maintenance
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        if let Some(kind) = log.maintenance.take() {
            if standalone {
                self.running.send_replace(false);
            }
            let _ = self.events.send(SessionEvent::MaintenanceEnd {
                kind,
                error,
                aborted,
            });
        }
    }

    fn check_commit(
        &self,
        log: &Log,
        revision: u64,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        if cancel.is_cancelled() || self.maintenance.shutdown.load(Ordering::Acquire) {
            return Err(Error::MaintenanceAborted);
        }
        if log.revision != revision {
            return Err(Error::ContextChanged);
        }
        Ok(())
    }

    fn restore(&self, log: &Log) {
        self.maintenance
            .tools
            .ledger
            .restore(&log.projection.messages);
        self.maintenance
            .tools
            .context
            .restore(&log.projection.messages);
    }

    fn candidate(
        log: &mut Log,
        pending: &[Entry],
        before: u64,
        limits: &Limits,
        require_trigger: bool,
    ) -> Result<u64, Error> {
        // Projection owns no raw entries. Borrow a prospective suffix without duplicating
        // the log; remove it before any await, publication or durable write.
        let len = log.entries.len();
        let seq = log.entries.last().map_or(0, |entry| entry.seq);
        log.entries
            .extend(pending.iter().enumerate().map(|(at, entry)| SeqEntry {
                seq: seq + at as u64 + 1,
                entry: entry.clone(),
            }));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let projected = project(&log.entries, None);
            compaction::validate_projection(&projected, before, limits, require_trigger)
        }));
        log.entries.truncate(len);
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    async fn save_usage(&self, model: &Model, usage: Usage, note: String) -> Result<(), Error> {
        if usage == Usage::default() || self.maintenance.shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut log = self.log.lock().await;
        if self.maintenance.shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        let base = log.next_base();
        self.write(
            &mut log,
            Entry::Usage {
                base,
                kind: "compaction".into(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                usage,
                note: Some(note),
            },
        )
        .await?;
        Ok(())
    }

    async fn elide(
        &self,
        mode: compaction::elide::ElisionMode,
        cancel: &CancellationToken,
    ) -> Result<bool, Error> {
        let (messages, sources, revision, before) = {
            let log = self.log.lock().await;
            let sources = log
                .projection
                .sources
                .iter()
                .filter_map(|source| {
                    let entry = &log.entries[source.entry_index].entry;
                    matches!(entry, Entry::Message { .. } | Entry::CustomMessage { .. })
                        .then(|| {
                            entry
                                .id()
                                .map(|id| (EntryId(id.to_owned()), source.messages.clone()))
                        })
                        .flatten()
                })
                .collect::<Vec<_>>();
            (
                log.projection.messages.clone(),
                sources,
                log.revision,
                compaction::estimate(&log.projection),
            )
        };
        let elision = compaction::elide::prepare(
            &messages,
            &sources,
            &self.maintenance.artifacts_dir,
            &self.maintenance.skill_paths,
            mode,
        )?;
        let Some(elision) = elision.filter(|elision| !elision.edits.is_empty()) else {
            return Ok(false);
        };
        let mut log = self.log.lock().await;
        self.check_commit(&log, revision, cancel)?;
        let mut parent = log.next_base().parent_id;
        let mut reserved_ids = HashSet::new();
        let pending: Vec<_> = elision
            .edits
            .into_iter()
            .map(|(target_id, replacement)| {
                let id = EntryId::generate(|id| log.ids.contains(id) || reserved_ids.contains(id));
                reserved_ids.insert(id.0.clone());
                let base = EntryBase::new(id.clone(), parent.take());
                parent = Some(id);
                Entry::ContextEdit {
                    base,
                    target_id,
                    replacement: Some(replacement),
                }
            })
            .collect();
        if let compaction::elide::ElisionMode::Clearing { limits } = mode {
            let Ok(after) = Self::candidate(&mut log, &pending, before, &limits, true) else {
                return Ok(false);
            };
            if limits.trigger < 20_000 || after > limits.trigger - 20_000 || after >= before {
                return Ok(false);
            }
        }
        compaction::elide::write_artifacts(&self.maintenance.artifacts_dir, &elision.artifacts)
            .await?;
        self.write_all(&mut log, pending).await?;
        Ok(true)
    }

    async fn compact_inner(
        &self,
        automatic: bool,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<bool, Error> {
        let limits = {
            let log = self.log.lock().await;
            let limits = Limits::resolve(&log.model, &self.maintenance.settings)?;
            if automatic && !force && compaction::estimate(&log.projection) <= limits.trigger {
                return Ok(false);
            }
            limits
        };
        if automatic
            && self
                .elide(compaction::elide::ElisionMode::Clearing { limits }, cancel)
                .await?
        {
            return Ok(true);
        }
        let mut method = if force {
            SummaryRequest::Separate
        } else {
            self.maintenance.settings.summary_request
        };
        let mut last_error = String::new();
        for _ in 0..3 {
            let (prepared, model, thinking, revision, checkpoint, system) = {
                let log = self.log.lock().await;
                self.check_commit(&log, log.revision, cancel)?;
                let checkpoint = log.next_base();
                let prepared = compaction::prepare_compaction(
                    &log.projection,
                    &log.entries,
                    &limits,
                    checkpoint.id.clone(),
                )?;
                let system = axo_ai::transcript::replay_system_message(
                    log.projection
                        .messages
                        .iter()
                        .filter_map(|message| match message {
                            AgentMessage::Llm(Message::System(system)) => Some(system),
                            _ => None,
                        }),
                );
                (
                    prepared,
                    log.model.clone(),
                    log.thinking,
                    log.revision,
                    checkpoint.id,
                    system,
                )
            };
            let context = compaction::summary::SummaryContext {
                streamer: self.maintenance.streamer.as_ref(),
                model: &model,
                thinking,
                session_id: &self.maintenance.session_id,
                reply: limits.reply,
                prompts: &self.maintenance.prompts,
            };
            match compaction::summary::generate(&context, &prepared.summary, method, cancel).await {
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
                        let mut log = self.log.lock().await;
                        self.check_commit(&log, revision, cancel)?;
                        let base = EntryBase::new(checkpoint, log.next_base().parent_id);
                        let entry = Entry::Compaction {
                            base,
                            summary,
                            first_kept_entry_id: prepared.first_kept_entry_id,
                            preserved_user_ids: prepared.preserved_user_ids,
                            tokens_before: prepared.tokens_before,
                            details: Some(prepared.files.details()),
                            usage: Some(generated.usage.clone()),
                            from_hook: None,
                            system_message: system,
                        };
                        Self::candidate(
                            &mut log,
                            std::slice::from_ref(&entry),
                            prepared.tokens_before,
                            &limits,
                            automatic,
                        )?;
                        self.write(&mut log, entry).await?;
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

    async fn automatic(&self, force: bool, cancel: &CancellationToken) -> Result<bool, Error> {
        if !self.maintenance.settings.enabled {
            return Ok(false);
        }
        {
            let mut log = self.log.lock().await;
            let limits = Limits::resolve(&log.model, &self.maintenance.settings)?;
            if !force && compaction::estimate(&log.projection) <= limits.trigger {
                return Ok(false);
            }
            self.reserve(&mut log, MaintenanceKind::Compact, false, cancel.clone())?;
        }
        let result = std::panic::AssertUnwindSafe(self.compact_inner(true, force, cancel))
            .catch_unwind()
            .await;
        let (error, aborted) = match &result {
            Ok(Ok(_)) => (None, false),
            Ok(Err(error)) => (Some(error.to_string()), cancel.is_cancelled()),
            Err(_) => (Some("Maintenance task panicked".into()), false),
        };
        self.end_maintenance(false, error, aborted).await;
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    async fn sync_sections(
        &self,
        log: &mut Log,
        current: &Sections,
    ) -> Result<Option<AgentMessage>, Error> {
        let mut effective: Option<Sections> = None;
        for message in &log.projection.messages {
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
            &axo_ai::transcript::replay_tools(log.projection.messages.iter().filter_map(
                |message| match message {
                    AgentMessage::Llm(Message::System(system)) => Some(system),
                    _ => None,
                },
            )),
            &self.maintenance.tool_specs,
        );
        let system = system_message(effective.as_ref(), current);
        if system.is_none() && changes.tools_added.is_empty() && changes.tools_removed.is_empty() {
            return Ok(None);
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
        let message = AgentMessage::Llm(Message::System(system));
        let base = log.next_base();
        self.write(
            log,
            Entry::Message {
                base,
                message: message.clone(),
                submission_id: None,
            },
        )
        .await?;
        Ok(Some(message))
    }

    async fn navigate_inner(
        &self,
        target: Option<EntryId>,
        sections: Option<Sections>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let (prepared, model, thinking, revision, from_id, limits) = {
            let log = self.log.lock().await;
            if let Some(target) = &target
                && !log.ids.contains(target.as_str())
            {
                return Err(Error::EntryNotFound(target.to_string()));
            }
            (
                compaction::prepare_branch(&log.projection, &log.entries, target.as_ref()),
                log.model.clone(),
                log.thinking,
                log.revision,
                log.entries
                    .last()
                    .and_then(|seq| seq.entry.id())
                    .unwrap_or("root")
                    .to_owned(),
                Limits::resolve(&log.model, &self.maintenance.settings)?,
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
        let mut log = self.log.lock().await;
        if let Err(error) = self.check_commit(&log, revision, cancel) {
            drop(log);
            if let Some((summary, _)) = generated {
                self.save_usage(&model, summary.usage, error.to_string())
                    .await?;
            }
            return Err(error);
        }
        let base = EntryBase::new(EntryId::generate(|id| log.ids.contains(id)), target);
        let entry = match generated {
            Some((summary, files)) => Entry::BranchSummary {
                base,
                from_id,
                summary: compaction::history_summary(&summary.text, &self.maintenance.path, &files),
                details: Some(files.details()),
                usage: Some(summary.usage),
                from_hook: None,
            },
            None => Entry::Leaf { base },
        };
        let usage = match &entry {
            Entry::BranchSummary { usage, .. } => usage.clone(),
            _ => None,
        };
        if let Err(error) = self.write(&mut log, entry).await {
            drop(log);
            if let Some(usage) = usage {
                self.save_usage(&model, usage, error.to_string()).await?;
            }
            return Err(error);
        }
        let model = log.projection.model.clone();
        let thinking = log.projection.thinking;
        self.record_selection(&mut log, model.as_deref(), thinking)
            .await?;
        self.take_system();
        if let Some(sections) = sections {
            self.sync_sections(&mut log, &sections).await?;
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
                MaintenanceOperation::Compact => {
                    self.compact_inner(false, false, &cancel).await?;
                }
                MaintenanceOperation::Shake(mode) => {
                    let mode = match mode {
                        ShakeMode::Elide => compaction::elide::ElisionMode::Elide,
                        ShakeMode::Images => compaction::elide::ElisionMode::Images,
                        ShakeMode::Thinking => compaction::elide::ElisionMode::Thinking,
                    };
                    self.elide(mode, &cancel).await?;
                }
                MaintenanceOperation::Navigate { target, sections } => {
                    self.navigate_inner(target, sections, &cancel).await?
                }
            }
            Ok::<(), Error>(())
        })
        .catch_unwind()
        .await;
        agent.set_messages(self.log.lock().await.projection.messages.clone());
        let (error, aborted) = match &result {
            Ok(Ok(())) => (None, false),
            Ok(Err(error)) => (
                Some(error.to_string()),
                matches!(error, Error::MaintenanceAborted),
            ),
            Err(_) => (Some("Maintenance task panicked".into()), false),
        };
        self.end_maintenance(true, error, aborted).await;
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn failure_state(
        &self,
        log: &Log,
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

    fn checkpoint_completed(&self, log: &Log, state: &FailureState) -> bool {
        let Ok(limits) = Limits::resolve(&log.model, &self.maintenance.settings) else {
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

    async fn omit_response(
        &self,
        log: &mut Log,
        at: usize,
        response: &AssistantMessage,
    ) -> Result<(), Error> {
        let path = log.projection.path();
        let mut targets = Vec::new();
        for index in path.iter().copied().filter(|&index| index >= at) {
            if index > at
                && matches!(
                    &log.entries[index].entry,
                    Entry::Message {
                        message: AgentMessage::Llm(Message::Assistant(_)),
                        ..
                    }
                )
            {
                break;
            }
            let entry = &log.entries[index].entry;
            if (index == at
                || matches!(entry,
                    Entry::Message { message: AgentMessage::Llm(Message::ToolResult(result)), .. }
                        if response.tool_calls().any(|call| call.id == result.tool_call_id)))
                && let Some(id) = entry.id()
                && !log.projection.is_omitted(id)
            {
                targets.push(EntryId(id.to_owned()));
            }
        }
        let mut parent = log.next_base().parent_id;
        let mut reserved_ids = HashSet::new();
        let edits = targets
            .into_iter()
            .map(|target_id| {
                let id = EntryId::generate(|id| log.ids.contains(id) || reserved_ids.contains(id));
                reserved_ids.insert(id.0.clone());
                let base = EntryBase::new(id.clone(), parent.take());
                parent = Some(id);
                Entry::ContextEdit {
                    base,
                    target_id,
                    replacement: None,
                }
            })
            .collect();
        self.write_all(log, edits).await
    }

    async fn context(&self, tools: &[Arc<dyn Tool>]) -> AgentContext {
        AgentContext {
            messages: self.log.lock().await.projection.messages.clone(),
            tools: tools.to_vec(),
        }
    }

    fn take_system(&self) -> Vec<AgentMessage> {
        std::mem::take(&mut *self.system.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Appends `entry` and records it without refreshing or broadcasting it.
    async fn append(&self, log: &mut Log, entry: Entry) -> Result<SeqEntry, Error> {
        if log.maintenance.is_some() && self.maintenance.shutdown.load(Ordering::Acquire) {
            return Err(Error::MaintenanceAborted);
        }
        let appended = log.writer.append(entry).await?;
        if let Some(id) = appended.entry.id() {
            log.ids.insert(id.to_owned());
        }
        if !matches!(
            &appended.entry,
            Entry::Submission { .. } | Entry::Usage { .. }
        ) {
            log.revision = log.revision.wrapping_add(1);
        }
        log.entries.push(appended.clone());
        Ok(appended)
    }

    fn refresh(&self, log: &mut Log, appended: &Entry) {
        log.projection.apply(&log.entries);
        if matches!(
            appended,
            Entry::ContextEdit { .. }
                | Entry::Compaction { .. }
                | Entry::BranchSummary { .. }
                | Entry::Leaf { .. }
        ) {
            self.restore(log);
        }
    }

    /// Appends `entry`, refreshes derived state and broadcasts it.
    async fn write(&self, log: &mut Log, entry: Entry) -> Result<SeqEntry, Error> {
        let appended = self.append(log, entry).await?;
        self.refresh(log, &appended.entry);
        let _ = self.events.send(SessionEvent::Entry(appended.clone()));
        Ok(appended)
    }

    async fn write_all(&self, log: &mut Log, entries: Vec<Entry>) -> Result<(), Error> {
        let mut appended = Vec::with_capacity(entries.len());
        let mut result = Ok(());
        for entry in entries {
            match self.append(log, entry).await {
                Ok(entry) => appended.push(entry),
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        if let Some(last) = appended.last() {
            self.refresh(log, &last.entry);
        }
        for entry in appended {
            let _ = self.events.send(SessionEvent::Entry(entry));
        }
        result
    }

    /// Closes the run with `run_end` (none after a shutdown) and releases the session
    /// for the next run.
    async fn settle(
        &self,
        result: Result<RunOutcome, axo_agent::Error>,
    ) -> Result<RunOutcome, Error> {
        let mut log = self.log.lock().await;
        self.running.send_replace(false);
        log.streaming = None;
        log.tool_calls.clear();
        if let Some(run_id) = log.run_id.take() {
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
                let base = log.next_base();
                self.write(
                    &mut log,
                    Entry::RunEnd {
                        base,
                        run_id,
                        outcome,
                        reason,
                    },
                )
                .await?;
            }
        }
        result.map_err(Error::Agent)
    }

    /// Appends `model_change` / `thinking_level_change` when the current branch records a
    /// different selection than the live one in `log` (pi `_recordSelection`), so reopening
    /// restores what is live.
    async fn record_selection(
        &self,
        log: &mut Log,
        model: Option<&str>,
        thinking: Option<ThinkingLevel>,
    ) -> Result<(), Error> {
        if model != Some(format!("{}/{}", log.model.provider, log.model.id).as_str()) {
            let base = log.next_base();
            let entry = Entry::ModelChange {
                base,
                provider: log.model.provider.clone(),
                model_id: log.model.id.clone(),
            };
            self.write(log, entry).await?;
        }
        if thinking != Some(log.thinking) {
            let base = log.next_base();
            let entry = Entry::ThinkingLevelChange {
                base,
                thinking_level: log.thinking,
            };
            self.write(log, entry).await?;
        }
        Ok(())
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
            .automatic(false, cancel)
            .await
            .map_err(|error| HookError(error.to_string()))?
        {
            Ok(Some(RequestUpdate {
                context: Some(self.context(&request.context.tools).await),
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
            {
                let mut log = self.log.lock().await;
                self.check_commit(&log, log.revision, cancel)?;
                if !recovery::served_by(response, &log.model) {
                    let state = self.failure_state(&log, response, turn.model)?;
                    let unusable = state.hidden || recovery::foreign_unusable(response, &log.model);
                    if unusable {
                        self.omit_response(&mut log, state.at, response).await?;
                        return Ok(TurnFinish {
                            decision: Some(TurnDecision::Continue),
                            context: Some(AgentContext {
                                messages: log.projection.messages.clone(),
                                tools: turn.context.tools.clone(),
                            }),
                        });
                    }
                    return Ok(TurnFinish::default());
                }
            }
            let pressure = recovery::is_pressure(response, turn.model);
            let silent_success = pressure
                && response.stop_reason == StopReason::Stop
                && response.tool_calls().next().is_none();
            if silent_success {
                let changed = self.automatic(true, cancel).await?;
                let context = if changed {
                    Some(self.context(&turn.context.tools).await)
                } else {
                    None
                };
                return Ok(TurnFinish {
                    decision: None,
                    context,
                });
            }
            let faulty =
                pressure || matches!(response.stop_reason, StopReason::Error | StopReason::Length);
            if faulty {
                let (delay, shrink, retry) = {
                    let mut log = self.log.lock().await;
                    self.check_commit(&log, log.revision, cancel)?;
                    let state = self.failure_state(&log, response, turn.model)?;
                    let switched = !recovery::served_by(response, &log.model);
                    if switched {
                        self.omit_response(&mut log, state.at, response).await?;
                        (None, false, true)
                    } else if pressure {
                        if !self.maintenance.settings.enabled || state.overflow_used {
                            return Ok(TurnFinish::default());
                        }
                        let completed = self.checkpoint_completed(&log, &state);
                        self.omit_response(&mut log, state.at, response).await?;
                        (None, !(state.hidden && completed), true)
                    } else if let Some(delay) = axo_ai::retry_delay(response, state.retries) {
                        self.omit_response(&mut log, state.at, response).await?;
                        let _ = self.events.send(SessionEvent::Retry {
                            attempt: state.retries + 1,
                            delay_ms: delay.as_millis() as u64,
                        });
                        (Some(delay), false, true)
                    } else {
                        (None, false, false)
                    }
                };
                if retry {
                    if let Some(delay) = delay {
                        tokio::select! {
                            _ = cancel.cancelled() => return Err(Error::MaintenanceAborted),
                            _ = tokio::time::sleep(delay) => {}
                        }
                    }
                    if shrink {
                        self.automatic(true, cancel).await?;
                    }
                    return Ok(TurnFinish {
                        decision: Some(TurnDecision::Continue),
                        context: Some(self.context(&turn.context.tools).await),
                    });
                }
                return Ok(TurnFinish::default());
            }
            if response.tool_calls().next().is_none() {
                let changed = self.automatic(false, cancel).await?;
                let context = if changed {
                    Some(self.context(&turn.context.tools).await)
                } else {
                    None
                };
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
}

#[async_trait]
impl Persist for SessionHooks {
    /// Saves the run's start and every finished message before the loop goes on, then
    /// relays the event onto the session channel.
    async fn persist(&self, event: &mut AgentEvent) -> Result<(), PersistError> {
        let mut log = self.log.lock().await;
        match &*event {
            // A resumed run keeps the id of the run it continues.
            AgentEvent::AgentStart if log.run_id.is_none() => {
                let run_id = Uuid::now_v7().to_string();
                let base = log.next_base();
                self.write(
                    &mut log,
                    Entry::RunStart {
                        base,
                        run_id: run_id.clone(),
                    },
                )
                .await
                .map_err(|err| PersistError(err.to_string()))?;
                log.run_id = Some(run_id);
            }
            AgentEvent::MessageUpdate { event } => {
                log.streaming
                    .get_or_insert_with(Accumulator::new)
                    .apply(event);
            }
            AgentEvent::MessageEnd { message } => {
                log.streaming = None;
                let submission_id = log
                    .pending
                    .iter()
                    .position(|(pending, _)| pending == message)
                    .map(|at| log.pending.remove(at).1);
                let base = log.next_base();
                self.write(
                    &mut log,
                    Entry::Message {
                        base,
                        message: message.clone(),
                        submission_id,
                    },
                )
                .await
                .map_err(|err| PersistError(err.to_string()))?;
            }
            AgentEvent::ToolExecutionStart {
                call_id,
                name,
                args,
            } => log.tool_calls.push(ToolCall {
                id: call_id.clone(),
                name: name.clone(),
                arguments: args.clone(),
                arguments_error: None,
            }),
            AgentEvent::ToolExecutionEnd { call_id, .. } => {
                log.tool_calls.retain(|call| &call.id != call_id);
            }
            AgentEvent::AgentEnd { .. } => {
                log.streaming = None;
                log.tool_calls.clear();
            }
            _ => {}
        }
        let _ = self.events.send(SessionEvent::Agent(event.clone()));
        Ok(())
    }
}

impl Session {
    /// Builds the tools, loads context files and skills, resolves the exec environment
    /// once for its prompt note, restores the transcript and the read record, records
    /// model, thinking and prompt changes, and runs recovery. A resumed run, or requeued
    /// prompts, run in a spawned task.
    pub(crate) async fn start(setup: SessionSetup) -> Result<Session, Error> {
        let SessionSetup {
            dir,
            cwd,
            settings,
            streamer,
            model,
            thinking,
            exec,
            ask_enabled,
            project_access,
            host_sections,
            store,
            session_id,
            writer,
            entries,
            projection,
            recovery,
        } = setup;

        let path = writer.planned_path().to_path_buf();
        let spool_dir = path.with_extension("").join("bash");
        let artifacts_dir = path.with_extension("").join("context");
        let (events, _) = broadcast::channel(1024);
        let questions = Arc::new(PendingQuestions::new(events.clone()));
        let project = project_access != ProjectAccess::Ignore;
        let home_agents = settings.home_agents_dir.then(home_agents_dir).flatten();
        let (names, name_diagnostics) = context_file_names(&settings.context_files);
        let (context_files, file_diagnostics) =
            load_context_files(&dir, home_agents.as_deref(), &cwd, project, &names);
        let context = NestedContext::new(
            (project && settings.nested_context_files).then(|| project_root(&cwd)),
            names,
            &context_files,
        );
        let (skills, skill_diagnostics) = load_skills(
            &dir,
            home_agents.as_deref(),
            &cwd,
            project && settings.project.files.skills,
        );
        let (summary_prompts, summary_diagnostics) = compaction::summary::SummaryPrompts::load(
            &dir,
            &cwd,
            project && settings.project.files.axo_dir,
        );
        for diagnostic in name_diagnostics
            .iter()
            .chain(&file_diagnostics)
            .chain(&skill_diagnostics)
            .chain(&summary_diagnostics)
        {
            warn!("{diagnostic}");
        }

        let session_tools = SessionTools::new(
            cwd.clone(),
            exec.clone(),
            spool_dir.clone(),
            questions.clone(),
            settings.ask.timeout_ms.map(Duration::from_millis),
            settings.bash.ci,
            context,
        );
        let tools = tools::builtin(&session_tools, ask_enabled);

        let ids = entries
            .iter()
            .filter_map(|entry| entry.entry.id())
            .map(str::to_owned)
            .collect();
        let hooks = Arc::new(SessionHooks {
            events,
            system: Mutex::new(Vec::new()),
            log: tokio::sync::Mutex::new(Log {
                writer,
                entries,
                projection: projection.clone(),
                ids,
                model: model.clone(),
                thinking,
                run_id: None,
                pending: Vec::new(),
                streaming: None,
                tool_calls: Vec::new(),
                maintenance: None,
                revision: 0,
            }),
            running: watch::channel(false).0,
            maintenance: Maintenance {
                streamer: streamer.clone(),
                settings: settings.compaction.clone(),
                prompts: summary_prompts,
                session_id: session_id.clone(),
                path: path.clone(),
                artifacts_dir,
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
        });
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

        let session = Session {
            agent,
            hooks,
            tool_state: session_tools,
            questions,
            exec,
            id: session_id,
            path,
            store,
            settings,
            streamer,
            ask_enabled,
            dir,
            cwd,
            project_access,
            tools,
            context_files,
            skills,
            host_sections,
            prompt: Mutex::new(PromptState {
                note: None,
                date: String::new(),
                sections: None,
            }),
        };
        let resolved = session
            .exec
            .resolve(&session.cwd, &CancellationToken::new(), &|_| {})
            .await;
        session.rebuild_prompt(resolved.prompt_note, today());

        let resume = {
            let mut log = session.hooks.log.lock().await;
            session
                .hooks
                .record_selection(&mut log, projection.model.as_deref(), projection.thinking)
                .await?;
            let mut messages = projection.messages;
            session.sync_prompt(&mut log, &mut messages).await?;
            let resume = match recovery {
                Some(policy) => {
                    session
                        .recover(&mut log, policy, &spool_dir, &mut messages)
                        .await?
                }
                None => None,
            };
            // After recovery's results, so a replayed `write` finds the file it saw.
            session.tool_state.ledger.restore(&messages);
            session.tool_state.context.restore(&messages);
            session.agent.set_messages(messages);
            resume
        };

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

    /// Appends the system message that moves `messages`' effective prompt to the current
    /// sections, if they differ.
    async fn sync_prompt(
        &self,
        log: &mut Log,
        messages: &mut Vec<AgentMessage>,
    ) -> Result<(), Error> {
        let current = self
            .prompt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sections
            .clone();
        if let Some(current) = current
            && let Some(message) = self.hooks.sync_sections(log, &current).await?
        {
            messages.push(message);
        }
        Ok(())
    }

    /// Acts on the recovery plan; returns what to run when a run resumes or requeued
    /// input is left.
    async fn recover(
        &self,
        log: &mut Log,
        policy: RecoveryPolicy,
        spool_dir: &Path,
        messages: &mut Vec<AgentMessage>,
    ) -> Result<Option<Resumption>, Error> {
        let replay_of = |name: &str| {
            self.tools
                .iter()
                .find(|tool| tool.name() == name)
                .map_or(Replay::Never, |tool| tool.replay())
        };
        let (run_id, outcome, reason) = match plan_recovery(
            &log.entries,
            &log.projection,
            &log.model,
            &replay_of,
            spool_dir,
        ) {
            RecoveryPlan::None => return Ok(None),
            RecoveryPlan::Requeue { requeue } => {
                if policy == RecoveryPolicy::MarkInterrupted {
                    return Ok(None);
                }
                let queued = requeued(log, requeue);
                if queued
                    .iter()
                    .any(|(kind, _)| matches!(kind, SubmissionKind::Prompt))
                {
                    self.hooks.running.send_replace(true);
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
                        let state = self.hooks.failure_state(log, &response, &log.model)?;
                        let completed = self.hooks.checkpoint_completed(log, &state);
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
                            let base = log.next_base();
                            self.hooks
                                .write(
                                    log,
                                    Entry::RunEnd {
                                        base,
                                        run_id,
                                        outcome: store::RunOutcome::Done,
                                        reason: None,
                                    },
                                )
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
                    let model = log.model.clone();
                    let base = log.next_base();
                    self.hooks
                        .write(
                            log,
                            Entry::RunResumed {
                                base,
                                run_id: run_id.clone(),
                                attempt,
                            },
                        )
                        .await?;
                    log.run_id = Some(run_id);
                    self.hooks.running.send_replace(true);
                    return Ok(Some(Resumption::RecoverResponse {
                        response,
                        model,
                        results,
                        queued: requeued(log, requeue),
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
                for (call, spool) in calls {
                    let mut text = format!(
                        "Tool {} was interrupted and may have partially run.",
                        call.name
                    );
                    if let Some(spool) = spool {
                        text.push_str(" Output so far: ");
                        text.push_str(&spool.display().to_string());
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
                    let base = log.next_base();
                    let entry = Entry::Message {
                        base,
                        message: message.clone(),
                        submission_id: None,
                    };
                    self.hooks.write(log, entry).await?;
                    messages.push(message);
                }
                if policy == RecoveryPolicy::MarkInterrupted {
                    (run_id, store::RunOutcome::Interrupted, None)
                } else {
                    let base = log.next_base();
                    let entry = Entry::RunResumed {
                        base,
                        run_id: run_id.clone(),
                        attempt,
                    };
                    self.hooks.write(log, entry).await?;
                    log.run_id = Some(run_id);
                    self.hooks.running.send_replace(true);
                    return Ok(Some(Resumption::Continue(requeued(log, requeue))));
                }
            }
        };
        let base = log.next_base();
        let entry = Entry::RunEnd {
            base,
            run_id,
            outcome,
            reason,
        };
        self.hooks.write(log, entry).await?;
        Ok(None)
    }

    /// Rebuilds the prompt sections unless `note` and `date` are unchanged; returns the
    /// system message announcing the change (every section on the first build).
    fn rebuild_prompt(&self, note: Option<String>, date: String) -> Option<SystemMessage> {
        let mut state = self.prompt.lock().unwrap_or_else(PoisonError::into_inner);
        if state.sections.is_some() && state.note == note && state.date == date {
            return None;
        }
        let (sections, diagnostics) = build_sections(&PromptInputs {
            dir: &self.dir,
            cwd: &self.cwd,
            project_prompts: self.project_access != ProjectAccess::Ignore
                && self.settings.project.files.axo_dir,
            tools: &self.tools,
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

    fn refresh_prompt(&self, note: Option<String>) {
        if let Some(message) = self.rebuild_prompt(note, today()) {
            self.hooks
                .system
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(AgentMessage::Llm(Message::System(message)));
        }
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
        &self.skills
    }

    /// Expands a `/skill args` invocation, saves the submission, then runs the prompt;
    /// returns when the run settles. `Error::Busy` while a run is active (use
    /// `steer`/`follow_up`).
    pub async fn prompt(&self, text: &str) -> Result<RunOutcome, Error> {
        match self
            .prompt_with(PromptRequest {
                text: text.to_owned(),
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
        let text = expand_skill_command(&request.text, &self.skills);
        let (submission, message) = {
            let mut log = self.hooks.log.lock().await;
            if let Some(request_id) = &request.request_id
                && let Some(existing) = log.entries.iter().find(|seq| {
                    matches!(&seq.entry, Entry::Submission { request_id: Some(id), .. } if id == request_id)
                })
            {
                return Ok(PromptResult::Duplicate(existing.clone()));
            }
            if *self.hooks.running.borrow() {
                return Err(Error::Busy);
            }
            let submitted = self
                .submit(&mut log, SubmissionKind::Prompt, text, request.request_id)
                .await?;
            self.hooks.running.send_replace(true);
            submitted
        };
        let note = self
            .prompt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .note
            .clone();
        self.refresh_prompt(note);
        let mut messages = self.hooks.take_system();
        messages.push(message);
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

    /// Saves a `submission` entry for `text` and returns it with the user message it
    /// becomes; that message's `message` entry will carry the submission id.
    async fn submit(
        &self,
        log: &mut Log,
        kind: SubmissionKind,
        text: String,
        request_id: Option<String>,
    ) -> Result<(SeqEntry, AgentMessage), Error> {
        let content = vec![UserBlock::Text { text }];
        let submission_id = Uuid::now_v7().to_string();
        let base = log.next_base();
        let entry = Entry::Submission {
            base,
            submission_id: submission_id.clone(),
            kind,
            content: content.clone(),
            request_id,
        };
        let submission = self.hooks.write(log, entry).await?;
        let message = AgentMessage::Llm(Message::User(UserMessage {
            content,
            timestamp: now_ms(),
        }));
        log.pending.push((message.clone(), submission_id));
        Ok((submission, message))
    }

    /// Saves and queues a user message delivered after the current tool calls finish.
    pub async fn steer(&self, text: &str) -> Result<(), Error> {
        let mut log = self.hooks.log.lock().await;
        let (_, message) = self
            .submit(&mut log, SubmissionKind::Steer, text.to_owned(), None)
            .await?;
        self.agent.steer(message);
        Ok(())
    }

    /// Saves and queues a user message delivered when the run would otherwise end.
    pub async fn follow_up(&self, text: &str) -> Result<(), Error> {
        let mut log = self.hooks.log.lock().await;
        let (_, message) = self
            .submit(&mut log, SubmissionKind::FollowUp, text.to_owned(), None)
            .await?;
        self.agent.follow_up(message);
        Ok(())
    }

    /// Stops the active run (`CancelReason::User`).
    pub async fn abort(&self) {
        self.hooks.cancel_maintenance(false);
        self.agent.cancel(CancelReason::User);
    }

    /// Stops the session as a host does before exiting: the active run, and any run started
    /// later, is cancelled with `CancelReason::Shutdown`, so nothing is recorded as aborted
    /// and the run resumes when the session is next opened. Returns once the run settled.
    pub async fn shutdown(&self) {
        // Record CancelReason::Shutdown before cancelling the shared maintenance token.
        self.agent.shutdown();
        self.hooks.cancel_maintenance(true);
        let _ = self
            .hooks
            .running
            .subscribe()
            .wait_for(|running| !*running)
            .await;
    }

    /// Switches the model from the next request; the thinking level is clamped to it.
    pub async fn set_model(&self, model: Model) -> Result<(), Error> {
        let mut log = self.hooks.log.lock().await;
        if log.maintenance.is_some() {
            return Err(Error::Busy);
        }
        let level = clamp_level(&model, log.thinking);
        let base = log.next_base();
        self.hooks
            .write(
                &mut log,
                Entry::ModelChange {
                    base,
                    provider: model.provider.clone(),
                    model_id: model.id.clone(),
                },
            )
            .await?;
        self.agent.set_model(model.clone());
        log.model = model;
        if level != log.thinking {
            let base = log.next_base();
            self.hooks
                .write(
                    &mut log,
                    Entry::ThinkingLevelChange {
                        base,
                        thinking_level: level,
                    },
                )
                .await?;
            self.agent.set_thinking(level);
            log.thinking = level;
        }
        Ok(())
    }

    /// Sets the thinking level from the next request, clamped to the model; returns the
    /// level applied.
    pub async fn set_thinking(&self, level: ThinkingLevel) -> Result<ThinkingLevel, Error> {
        let mut log = self.hooks.log.lock().await;
        if log.maintenance.is_some() {
            return Err(Error::Busy);
        }
        let level = clamp_level(&log.model, level);
        if level != log.thinking {
            let base = log.next_base();
            let entry = Entry::ThinkingLevelChange {
                base,
                thinking_level: level,
            };
            self.hooks.write(&mut log, entry).await?;
            self.agent.set_thinking(level);
            log.thinking = level;
        }
        Ok(level)
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
    /// all taken at one point of the append order: nothing is missed or duplicated.
    pub async fn attach(&self, from: Option<u64>) -> Attach {
        let log = self.hooks.log.lock().await;
        let events = self.hooks.events.subscribe();
        let start = from.map_or(0, |seq| {
            log.entries.partition_point(|entry| entry.seq <= seq)
        });
        let mut agent = self.agent.snapshot();
        agent.running = *self.hooks.running.borrow();
        agent.streaming = log.streaming.as_ref().and_then(Accumulator::snapshot);
        agent.pending_tool_calls = log.tool_calls.clone();
        Attach {
            missed: log.entries[start..].to_vec(),
            live: LiveSnapshot {
                run_id: log.run_id.clone(),
                maintenance: log.maintenance,
                agent,
                messages: log.projection.messages.clone(),
                pending_questions: self.questions.pending(),
            },
            events,
        }
    }

    async fn start_maintenance(
        &self,
        kind: MaintenanceKind,
        operation: MaintenanceOperation,
    ) -> Result<(), Error> {
        let cancel = CancellationToken::new();
        {
            let mut log = self.hooks.log.lock().await;
            self.hooks.reserve(&mut log, kind, true, cancel.clone())?;
        }
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

    /// Summarizes eligible history without submitting an ordinary user turn.
    pub async fn compact(&self) -> Result<(), Error> {
        self.start_maintenance(MaintenanceKind::Compact, MaintenanceOperation::Compact)
            .await
    }

    /// Removes eligible blocks into recoverable session artifacts, without model calls.
    pub async fn shake(&self, mode: ShakeMode) -> Result<(), Error> {
        self.start_maintenance(MaintenanceKind::Shake, MaintenanceOperation::Shake(mode))
            .await
    }

    /// Moves the session's position to `target` (`None` = before the first entry). Without
    /// `options.summarize` this appends a `leaf` entry; with it, the abandoned branch is
    /// summarized first and a `branch_summary` entry at `target` replaces the leaf. The next
    /// prompt continues from there with the live model and thinking level, recorded on the
    /// branch when it implies others.
    pub async fn navigate(
        &self,
        target: Option<EntryId>,
        options: NavigateOptions,
    ) -> Result<(), Error> {
        if options.summarize {
            let sections = self
                .prompt
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .sections
                .clone();
            return self
                .start_maintenance(
                    MaintenanceKind::BranchSummary,
                    MaintenanceOperation::Navigate { target, sections },
                )
                .await;
        }
        let mut log = self.hooks.log.lock().await;
        if *self.hooks.running.borrow() {
            return Err(Error::Busy);
        }
        if let Some(target) = &target
            && !log.ids.contains(target.as_str())
        {
            return Err(Error::EntryNotFound(target.to_string()));
        }
        let base = EntryBase::new(EntryId::generate(|id| log.ids.contains(id)), target);
        self.hooks.write(&mut log, Entry::Leaf { base }).await?;
        let projection = log.projection.clone();
        self.hooks
            .record_selection(&mut log, projection.model.as_deref(), projection.thinking)
            .await?;
        // Pending diffs were relative to the old position; `sync_prompt` recomputes them.
        self.hooks.take_system();
        let mut messages = projection.messages;
        self.sync_prompt(&mut log, &mut messages).await?;
        self.tool_state.ledger.restore(&messages);
        self.tool_state.context.restore(&messages);
        self.agent.set_messages(messages);
        Ok(())
    }

    /// Copies the root→`entry_id` path into a new session file that records this one as
    /// its parent. Forking at a user message copies the path to its parent and returns
    /// the message's text to edit and resubmit.
    pub async fn fork(&self, entry_id: &EntryId) -> Result<Fork, Error> {
        let (path, editable_text, model, thinking) = {
            let log = self.hooks.log.lock().await;
            let index: HashMap<&str, &SeqEntry> = log
                .entries
                .iter()
                .filter_map(|seq| seq.entry.id().map(|id| (id, seq)))
                .collect();
            let target = index
                .get(entry_id.as_str())
                .ok_or_else(|| Error::EntryNotFound(entry_id.to_string()))?;
            let (next, editable_text) = match &target.entry {
                Entry::Message {
                    message: AgentMessage::Llm(Message::User(user)),
                    base,
                    ..
                } => (
                    base.parent_id.as_ref().map(EntryId::as_str),
                    Some(
                        user.content
                            .iter()
                            .filter_map(|block| match block {
                                UserBlock::Text { text } => Some(text.as_str()),
                                UserBlock::Image { .. } => None,
                            })
                            .collect::<String>(),
                    ),
                ),
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
            (path, editable_text, log.model.clone(), log.thinking)
        };

        let header = Header::new(
            self.cwd.clone(),
            Some(self.path.clone()),
            Some(entry_id.clone()),
        );
        let session_id = header.id.clone();
        let mut writer = self.store.create(header).await?;
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
            dir: self.dir.clone(),
            cwd: self.cwd.clone(),
            settings: self.settings.clone(),
            streamer: self.streamer.clone(),
            model,
            thinking,
            exec: self.exec.clone(),
            ask_enabled: self.ask_enabled,
            project_access: self.project_access,
            host_sections: self.host_sections.clone(),
            store: self.store.clone(),
            session_id,
            writer,
            entries,
            projection,
            recovery: None,
        })
        .await?;
        Ok(Fork {
            session,
            editable_text,
        })
    }

    /// Drops the cached project environment and resolves it again now. When its prompt
    /// note or the date changed, the prompt diff reaches the model before its next request.
    pub async fn reload_exec_env(&self) {
        self.exec.invalidate();
        let resolved = self
            .exec
            .resolve(&self.cwd, &CancellationToken::new(), &|_| {})
            .await;
        self.refresh_prompt(resolved.prompt_note);
    }
}

/// Dropping a session shuts it down; its run task then settles and releases the writer
/// lock.
impl Drop for Session {
    fn drop(&mut self) {
        self.agent.shutdown();
        self.hooks.cancel_maintenance(true);
    }
}
