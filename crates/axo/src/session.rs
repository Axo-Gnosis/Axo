//! A conversation with one agent in one working directory, saved as it happens.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{
    Agent, AgentConfig, AgentEvent, AgentMessage, AgentSnapshot, CancelReason, LoopHooks, Persist,
    PersistError, Replay, RunOutcome, Tool, TurnContext, TurnUpdate,
};
use axo_ai::thinking::clamp_level;
use axo_ai::{
    Accumulator, Message, Model, Streamer, SystemMessage, ThinkingLevel, ToolCall,
    ToolResultMessage, UserBlock, UserMessage, now_ms,
};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use crate::config::{AxoDir, ProjectAccess, Settings};
use crate::context::files::{ContextFile, load_context_files};
use crate::context::prompt::{PromptInputs, Sections, build_sections, system_message};
use crate::context::skills::{Skill, expand_skill_command, load_skills};
use crate::error::Error;
use crate::exec::ExecEnv;
use crate::store::{
    self, Entry, EntryBase, EntryId, Header, Projection, RecoveryPlan, SeqEntry, SessionStore,
    SessionWriter, SubmissionKind, plan_recovery, project,
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
    pub agent: AgentSnapshot,
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
}

struct Log {
    writer: Box<dyn SessionWriter>,
    entries: Vec<SeqEntry>,
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

/// What `Session::start` runs after recovery.
enum Resumption {
    /// Continue the open run; requeued input goes through the agent's queues.
    Continue(Vec<(SubmissionKind, AgentMessage)>),
    /// No run was open: queue steers and follow-ups, and start a run with the prompts.
    Requeue(Vec<(SubmissionKind, AgentMessage)>),
}

/// How `run_to_settle` starts its run.
enum RunRequest {
    Prompt(Vec<AgentMessage>),
    Continue,
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
    let result = match request {
        RunRequest::Prompt(messages) => agent.prompt(messages).await,
        RunRequest::Continue => agent.continue_run().await,
    };
    hooks.settle(result).await
}

impl SessionHooks {
    fn take_system(&self) -> Vec<AgentMessage> {
        std::mem::take(&mut *self.system.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Appends `entry`, records it and broadcasts it.
    async fn write(&self, log: &mut Log, entry: Entry) -> Result<SeqEntry, Error> {
        let appended = log.writer.append(entry).await?;
        if let Some(id) = appended.entry.id() {
            log.ids.insert(id.to_owned());
        }
        log.entries.push(appended.clone());
        let _ = self.events.send(SessionEvent::Entry(appended.clone()));
        Ok(appended)
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
        messages
            .into_iter()
            .map(|message| match message {
                AgentMessage::Llm(message) => message,
                AgentMessage::Custom(custom) => Message::User(UserMessage {
                    content: custom.content,
                    timestamp: custom.timestamp,
                }),
            })
            .collect()
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
        let (events, _) = broadcast::channel(1024);
        let questions = Arc::new(PendingQuestions::new(events.clone()));
        let session_tools = SessionTools::new(
            cwd.clone(),
            exec.clone(),
            spool_dir.clone(),
            questions.clone(),
            settings.ask.timeout_ms.map(Duration::from_millis),
            settings.bash.ci,
        );
        let tools = tools::builtin(&session_tools, ask_enabled);

        let project = project_access != ProjectAccess::Ignore;
        let (context_files, file_diagnostics) = load_context_files(&dir, &cwd, project);
        let (skills, skill_diagnostics) =
            load_skills(&dir, &cwd, project && settings.project.files.skills);
        for diagnostic in file_diagnostics.iter().chain(&skill_diagnostics) {
            warn!("{diagnostic}");
        }

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
                ids,
                model: model.clone(),
                thinking,
                run_id: None,
                pending: Vec::new(),
                streaming: None,
                tool_calls: Vec::new(),
            }),
            running: watch::channel(false).0,
        });
        let agent = Arc::new(Agent::new(AgentConfig {
            thinking,
            tools: tools.clone(),
            hooks: hooks.clone(),
            persist: Some(hooks.clone()),
            steering_mode: settings.steering_mode,
            follow_up_mode: settings.follow_up_mode,
            session_id: Some(session_id),
            ..AgentConfig::new(streamer.clone(), model)
        }));

        let session = Session {
            agent,
            hooks,
            tool_state: session_tools,
            questions,
            exec,
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
            session.agent.set_messages(messages);
            resume
        };

        let agent = &session.agent;
        let request = match resume {
            None => None,
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
        let mut effective: Option<Sections> = None;
        for message in messages.iter() {
            if let AgentMessage::Llm(Message::System(SystemMessage {
                sections: Some(patch),
                ..
            })) = message
            {
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
        let message = {
            let state = self.prompt.lock().unwrap_or_else(PoisonError::into_inner);
            state
                .sections
                .as_ref()
                .and_then(|current| system_message(effective.as_ref(), current))
        };
        if let Some(message) = message {
            let message = AgentMessage::Llm(Message::System(message));
            let base = log.next_base();
            let entry = Entry::Message {
                base,
                message: message.clone(),
                submission_id: None,
            };
            self.hooks.write(log, entry).await?;
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
        let (run_id, outcome, reason) = match plan_recovery(&log.entries, &replay_of, spool_dir) {
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
            RecoveryPlan::Finish { run_id } => (run_id, store::RunOutcome::Done, None),
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
        self.agent.cancel(CancelReason::User);
    }

    /// Stops the session as a host does before exiting: the active run, and any run started
    /// later, is cancelled with `CancelReason::Shutdown`, so nothing is recorded as aborted
    /// and the run resumes when the session is next opened. Returns once the run settled.
    pub async fn shutdown(&self) {
        self.agent.shutdown();
        let _ = self
            .hooks
            .running
            .subscribe()
            .wait_for(|running| !*running)
            .await;
    }

    /// Switches the model from the next request; the thinking level is clamped to it.
    pub async fn set_model(&self, model: Model) -> Result<(), Error> {
        let thinking = {
            let mut log = self.hooks.log.lock().await;
            let base = log.next_base();
            let entry = Entry::ModelChange {
                base,
                provider: model.provider.clone(),
                model_id: model.id.clone(),
            };
            self.hooks.write(&mut log, entry).await?;
            self.agent.set_model(model.clone());
            log.model = model;
            log.thinking
        };
        self.set_thinking(thinking).await.map(drop)
    }

    /// Sets the thinking level from the next request, clamped to the model; returns the
    /// level applied.
    pub async fn set_thinking(&self, level: ThinkingLevel) -> Result<ThinkingLevel, Error> {
        let mut log = self.hooks.log.lock().await;
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
                agent,
                pending_questions: self.questions.pending(),
            },
            events,
        }
    }

    /// Moves the session's position to `target` (`None` = before the first entry) with a
    /// `leaf` entry; the next prompt continues from there with the live model and thinking
    /// level, recorded on the branch when it implies others.
    pub async fn navigate(&self, target: Option<EntryId>) -> Result<(), Error> {
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
        let projection = project(&log.entries, None);
        self.hooks
            .record_selection(&mut log, projection.model.as_deref(), projection.thinking)
            .await?;
        // Pending diffs were relative to the old position; `sync_prompt` recomputes them.
        self.hooks.take_system();
        let mut messages = projection.messages;
        self.sync_prompt(&mut log, &mut messages).await?;
        self.tool_state.ledger.restore(&messages);
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
            let (mut next, editable_text) = match &target.entry {
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
            let mut path = Vec::new();
            // The length bound stops a parent cycle.
            while let Some(id) = next
                && path.len() < log.entries.len()
            {
                let Some(seq) = index.get(id) else { break };
                path.push(seq.entry.clone());
                next = seq.entry.parent_id();
            }
            path.reverse();
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
        let open = match plan_recovery(&entries, &|_| Replay::Never, Path::new("")) {
            RecoveryPlan::None | RecoveryPlan::Requeue { .. } => None,
            RecoveryPlan::Finish { run_id } => Some((run_id, store::RunOutcome::Done, None)),
            RecoveryPlan::Park { run_id } | RecoveryPlan::Resume { run_id, .. } => Some((
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
    }
}
