//! A conversation with one agent in one working directory.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{
    Agent, AgentConfig, AgentEvent, AgentMessage, CancelReason, LoopHooks, Persist, PersistError,
    RunOutcome, Tool, TurnContext, TurnUpdate,
};
use axo_ai::{
    Message, Model, Streamer, SystemMessage, ThinkingLevel, UserBlock, UserMessage, now_ms,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use crate::config::{AxoDir, Settings};
use crate::context::files::{ContextFile, load_context_files};
use crate::context::prompt::{PromptInputs, Sections, build_sections, system_message};
use crate::context::skills::{Skill, expand_skill_command, load_skills};
use crate::error::Error;
use crate::exec::ExecEnv;
use crate::tools::ask::{Answer, PendingQuestions, Question};
use crate::tools::{self, SessionTools};

pub struct SessionOptions {
    pub cwd: PathBuf,
    /// `provider/model-id`; falls back to `defaultModel`.
    pub model: Option<String>,
    /// Falls back to `defaultThinkingLevel`; clamped to the model's supported levels.
    pub thinking: Option<ThinkingLevel>,
    /// Register the `ask` tool.
    pub ask_enabled: bool,
    /// Read project `.axo/` files, project context files and project skills.
    pub project_resources: bool,
    /// Overrides the environment provider built from settings.
    pub exec_env: Option<Arc<dyn ExecEnv>>,
    /// Host prompt sections, applied last; `None` removes a section.
    pub sections: Vec<(String, Option<String>)>,
}

impl Default for SessionOptions {
    /// `cwd` is the process's current directory (empty if it cannot be read).
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_default(),
            model: None,
            thinking: None,
            ask_enabled: false,
            project_resources: true,
            exec_env: None,
            sections: Vec::new(),
        }
    }
}

// Boxing `Agent` would not save anything: the broadcast clones the value per receiver.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Agent(AgentEvent),
    /// An `ask` call is waiting for `Session::answer` or `Session::dismiss`.
    QuestionPending {
        call_id: String,
        questions: Vec<Question>,
    },
    QuestionResolved {
        call_id: String,
    },
}

/// Everything `Axo::create_session` resolved for a new session.
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
    pub(crate) project_resources: bool,
    pub(crate) host_sections: Vec<(String, Option<String>)>,
}

pub struct Session {
    agent: Agent,
    hooks: Arc<SessionHooks>,
    questions: Arc<PendingQuestions>,
    exec: Arc<dyn ExecEnv>,
    // Prompt inputs, kept to rebuild the prompt when the exec environment changes.
    dir: AxoDir,
    cwd: PathBuf,
    project_resources: bool,
    tools: Vec<Arc<dyn Tool>>,
    context_files: Vec<ContextFile>,
    skills: Vec<Skill>,
    host_sections: Vec<(String, Option<String>)>,
    prompt: Mutex<PromptState>,
}

/// The last prompt build and the exec-environment note it used.
struct PromptState {
    note: Option<String>,
    /// `None` until the first build.
    sections: Option<Sections>,
}

/// The session's loop hooks and event relay.
struct SessionHooks {
    events: broadcast::Sender<SessionEvent>,
    /// System-message diffs not yet in the transcript: they go in before the next turn of
    /// an active run, or ahead of the next prompt.
    system: Mutex<Vec<AgentMessage>>,
}

impl SessionHooks {
    fn take_system(&self) -> Vec<AgentMessage> {
        std::mem::take(&mut *self.system.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[async_trait]
impl LoopHooks for SessionHooks {
    /// pi `convertToLlm` (`messages.ts:150-196`): a custom message reaches the model as a
    /// user message with its content.
    fn convert_to_llm(&self, messages: &[AgentMessage]) -> Vec<Message> {
        messages
            .iter()
            .map(|message| match message {
                AgentMessage::Llm(message) => message.clone(),
                AgentMessage::Custom(custom) => Message::User(UserMessage {
                    content: custom.content.clone(),
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
    /// Relays every agent event, in loop order, onto the session channel (M4 makes this
    /// the durable write point).
    async fn persist(&self, event: &mut AgentEvent) -> Result<(), PersistError> {
        let _ = self.events.send(SessionEvent::Agent(event.clone()));
        Ok(())
    }
}

fn user_message(text: String) -> AgentMessage {
    AgentMessage::Llm(Message::User(UserMessage {
        content: vec![UserBlock::Text { text }],
        timestamp: now_ms(),
    }))
}

impl Session {
    /// Builds the tools, loads context files and skills, resolves the exec environment
    /// once for its prompt note, and seeds the agent with the first system message.
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
            project_resources,
            host_sections,
        } = setup;

        let (events, _) = broadcast::channel(1024);
        let questions = Arc::new(PendingQuestions::new(events.clone()));
        let session_tools = SessionTools::new(
            cwd.clone(),
            exec.clone(),
            std::env::temp_dir()
                .join("axo-bash")
                .join(Uuid::now_v7().to_string()),
            questions.clone(),
            settings.ask.timeout_ms.map(Duration::from_millis),
            settings.bash.ci,
        );
        let tools = tools::builtin(&session_tools, ask_enabled);

        let (context_files, file_diagnostics) = load_context_files(&dir, &cwd, project_resources);
        let (skills, skill_diagnostics) = load_skills(&dir, &cwd, project_resources);
        for diagnostic in file_diagnostics.iter().chain(&skill_diagnostics) {
            warn!("{diagnostic}");
        }

        let hooks = Arc::new(SessionHooks {
            events,
            system: Mutex::new(Vec::new()),
        });
        let agent = Agent::new(AgentConfig {
            thinking,
            tools: tools.clone(),
            hooks: hooks.clone(),
            persist: Some(hooks.clone()),
            steering_mode: settings.steering_mode,
            follow_up_mode: settings.follow_up_mode,
            session_id: Some(Uuid::now_v7().to_string()),
            ..AgentConfig::new(streamer, model)
        });

        let session = Session {
            agent,
            hooks,
            questions,
            exec,
            dir,
            cwd,
            project_resources,
            tools,
            context_files,
            skills,
            host_sections,
            prompt: Mutex::new(PromptState {
                note: None,
                sections: None,
            }),
        };
        let resolved = session
            .exec
            .resolve(&session.cwd, &CancellationToken::new(), &|_| {})
            .await;
        if let Some(message) = session.rebuild_prompt(resolved.prompt_note) {
            session
                .agent
                .set_messages(vec![AgentMessage::Llm(Message::System(message))]);
        }
        Ok(session)
    }

    /// Rebuilds the prompt sections for `note` unless it is unchanged; returns the system
    /// message announcing the change (every section on the first build).
    fn rebuild_prompt(&self, note: Option<String>) -> Option<SystemMessage> {
        let mut state = self.prompt.lock().unwrap_or_else(PoisonError::into_inner);
        if state.sections.is_some() && state.note == note {
            return None;
        }
        let (sections, diagnostics) = build_sections(&PromptInputs {
            dir: &self.dir,
            cwd: &self.cwd,
            project_resources: self.project_resources,
            tools: &self.tools,
            context_files: &self.context_files,
            skills: &self.skills,
            prompt_note: note.as_deref(),
            host_sections: &self.host_sections,
        });
        for diagnostic in &diagnostics {
            warn!("{diagnostic}");
        }
        let message = system_message(state.sections.as_ref(), &sections);
        *state = PromptState {
            note,
            sections: Some(sections),
        };
        message
    }

    /// Expands a `/skill args` invocation, then runs the prompt; returns when the run
    /// settles. `Error::Busy` while a run is active (use `steer`/`follow_up`).
    pub async fn prompt(&self, text: &str) -> Result<RunOutcome, Error> {
        let text = expand_skill_command(text, &self.skills);
        let system = self.hooks.take_system();
        let mut messages = system.clone();
        messages.push(user_message(text));
        match self.agent.prompt(messages).await {
            Err(axo_agent::Error::Busy) => {
                // The active run picks the diffs up before its next turn instead.
                let mut queue = self
                    .hooks
                    .system
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let newer = std::mem::replace(&mut *queue, system);
                queue.extend(newer);
                Err(Error::Busy)
            }
            result => result.map_err(Error::Agent),
        }
    }

    /// Queues a user message delivered after the current tool calls finish.
    pub fn steer(&self, text: &str) {
        self.agent.steer(user_message(text.to_owned()));
    }

    /// Queues a user message delivered when the run would otherwise end.
    pub fn follow_up(&self, text: &str) {
        self.agent.follow_up(user_message(text.to_owned()));
    }

    /// Stops the active run (`CancelReason::User`).
    pub async fn abort(&self) {
        self.agent.cancel(CancelReason::User);
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

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.hooks.events.subscribe()
    }

    /// Drops the cached project environment and resolves it again now. When its prompt
    /// note changed, the prompt diff reaches the model before its next request.
    pub async fn reload_exec_env(&self) {
        self.exec.invalidate();
        let resolved = self
            .exec
            .resolve(&self.cwd, &CancellationToken::new(), &|_| {})
            .await;
        if let Some(message) = self.rebuild_prompt(resolved.prompt_note) {
            self.hooks
                .system
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(AgentMessage::Llm(Message::System(message)));
        }
    }
}
