//! The stateful agent (pi `agent/src/agent.ts`): owns the transcript, the queues, the
//! run's cancellation and the event fan-out around the loop in [`crate::run`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use axo_ai::{Accumulator, AssistantMessage, Message, Model, Streamer, ThinkingLevel, ToolCall};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::cancel::CancelReason;
use crate::error::Error;
use crate::event::{AgentEvent, Persist};
use crate::hooks::{LoopHooks, NoHooks};
use crate::message::AgentMessage;
use crate::run::{self, RunStart};
use crate::tool::{ExecutionMode, Tool};

pub struct AgentConfig {
    pub streamer: Arc<dyn Streamer>,
    pub model: Model,
    pub thinking: ThinkingLevel,
    pub tools: Vec<Arc<dyn Tool>>,
    pub hooks: Arc<dyn LoopHooks>,
    /// The ordered, awaited subscriber; see [`Persist`].
    pub persist: Option<Arc<dyn Persist>>,
    pub tool_execution: ExecutionMode,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    /// Forwarded to providers for cache-aware backends.
    pub session_id: Option<String>,
    /// Capacity of the event broadcast; a receiver that falls behind gets `Lagged`.
    pub broadcast_capacity: usize,
}

impl AgentConfig {
    /// A config with pi's defaults: no hooks, no persistence, parallel tools,
    /// one-at-a-time queues, thinking off, broadcast capacity 1024.
    pub fn new(streamer: Arc<dyn Streamer>, model: Model) -> Self {
        Self {
            streamer,
            model,
            thinking: ThinkingLevel::Off,
            tools: Vec::new(),
            hooks: Arc::new(NoHooks),
            persist: None,
            tool_execution: ExecutionMode::Parallel,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            session_id: None,
            broadcast_capacity: 1024,
        }
    }
}

/// How many queued messages one drain point takes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Done,
    Aborted(CancelReason),
    /// The last assistant turn ended with `StopReason::Error`; carries its error message.
    Failed(String),
}

/// A point-in-time view of the agent for late subscribers.
#[derive(Debug, Clone, Default)]
pub struct AgentSnapshot {
    pub running: bool,
    /// The partial assistant message being streamed.
    pub streaming: Option<AssistantMessage>,
    /// Tool calls currently executing.
    pub pending_tool_calls: Vec<ToolCall>,
    pub queued_steering: Vec<AgentMessage>,
    pub queued_follow_up: Vec<AgentMessage>,
}

pub struct Agent {
    pub(crate) streamer: Arc<dyn Streamer>,
    pub(crate) hooks: Arc<dyn LoopHooks>,
    pub(crate) persist: Option<Arc<dyn Persist>>,
    pub(crate) tool_execution: ExecutionMode,
    pub(crate) steering_mode: QueueMode,
    pub(crate) follow_up_mode: QueueMode,
    pub(crate) session_id: Option<String>,
    pub(crate) events: broadcast::Sender<AgentEvent>,
    pub(crate) state: Mutex<State>,
}

/// Mutable agent state. The lock is never held across an `.await`.
pub(crate) struct State {
    pub(crate) messages: Vec<AgentMessage>,
    pub(crate) model: Model,
    pub(crate) thinking: ThinkingLevel,
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) steering: VecDeque<AgentMessage>,
    pub(crate) follow_up: VecDeque<AgentMessage>,
    /// The active run, if any.
    pub(crate) run: Option<ActiveRun>,
    /// The assistant message being streamed.
    pub(crate) streaming: Option<Accumulator>,
    /// Tool calls currently executing.
    pub(crate) pending_tool_calls: Vec<ToolCall>,
}

pub(crate) struct ActiveRun {
    pub(crate) cancel: CancellationToken,
    /// Set by the first `Agent::cancel` of the run.
    pub(crate) reason: Option<CancelReason>,
}

/// Clears the run state when a run ends, including when its future is dropped.
struct RunGuard<'a>(&'a Agent);

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.run = None;
        state.streaming = None;
        state.pending_tool_calls.clear();
    }
}

impl Agent {
    pub fn new(cfg: AgentConfig) -> Self {
        let (events, _) = broadcast::channel(cfg.broadcast_capacity.max(1));
        Self {
            streamer: cfg.streamer,
            hooks: cfg.hooks,
            persist: cfg.persist,
            tool_execution: cfg.tool_execution,
            steering_mode: cfg.steering_mode,
            follow_up_mode: cfg.follow_up_mode,
            session_id: cfg.session_id,
            events,
            state: Mutex::new(State {
                messages: Vec::new(),
                model: cfg.model,
                thinking: cfg.thinking,
                tools: cfg.tools,
                steering: VecDeque::new(),
                follow_up: VecDeque::new(),
                run: None,
                streaming: None,
                pending_tool_calls: Vec::new(),
            }),
        }
    }

    /// Locks the state, recovering from a poisoned lock (no invariant spans a panic).
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers a new run; `Error::Busy` if one is active.
    fn begin_run(state: &mut State) -> Result<CancellationToken, Error> {
        if state.run.is_some() {
            return Err(Error::Busy);
        }
        let cancel = CancellationToken::new();
        state.run = Some(ActiveRun {
            cancel: cancel.clone(),
            reason: None,
        });
        Ok(cancel)
    }

    /// Starts a run with these messages. Returns when the run settles.
    pub async fn prompt(&self, messages: Vec<AgentMessage>) -> Result<RunOutcome, Error> {
        let cancel = Self::begin_run(&mut self.lock())?;
        let _guard = RunGuard(self);
        run::run_loop(self, RunStart::Prompt(messages), cancel).await
    }

    /// Continues from the current transcript: executes the unanswered tool calls of a
    /// trailing assistant message, or requests a new turn after a user/tool-result
    /// message. A trailing assistant message without tool calls is already complete:
    /// returns `Done` without emitting events.
    pub async fn continue_run(&self) -> Result<RunOutcome, Error> {
        let cancel = {
            let mut state = self.lock();
            if state.run.is_some() {
                return Err(Error::Busy);
            }
            let last = state
                .messages
                .iter()
                .rev()
                .find(|message| !matches!(message, AgentMessage::Llm(Message::System(_))));
            match last {
                None => return Err(Error::NothingToContinue),
                Some(AgentMessage::Llm(Message::Assistant(message)))
                    if message.tool_calls().next().is_none() =>
                {
                    return Ok(RunOutcome::Done);
                }
                Some(_) => {}
            }
            Self::begin_run(&mut state)?
        };
        let _guard = RunGuard(self);
        run::run_loop(self, RunStart::Continue, cancel).await
    }

    /// Queues a message injected after the current turn's tool calls finish.
    pub fn steer(&self, m: AgentMessage) {
        self.lock().steering.push_back(m);
    }

    /// Queues a message processed only when the run would otherwise end.
    pub fn follow_up(&self, m: AgentMessage) {
        self.lock().follow_up.push_back(m);
    }

    /// Cancels the active run, if any. The first reason given for a run wins.
    pub fn cancel(&self, reason: CancelReason) {
        let mut state = self.lock();
        if let Some(run) = &mut state.run {
            run.reason.get_or_insert(reason);
            run.cancel.cancel();
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    pub fn messages(&self) -> Vec<AgentMessage> {
        self.lock().messages.clone()
    }

    pub fn set_messages(&self, m: Vec<AgentMessage>) {
        self.lock().messages = m;
    }

    /// Takes effect from the next provider request.
    pub fn set_model(&self, m: Model) {
        self.lock().model = m;
    }

    /// Takes effect from the next provider request.
    pub fn set_thinking(&self, t: ThinkingLevel) {
        self.lock().thinking = t;
    }

    /// Takes effect from the next turn; the change is declared to the model with a
    /// system message.
    pub fn set_tools(&self, t: Vec<Arc<dyn Tool>>) {
        self.lock().tools = t;
    }

    pub fn snapshot(&self) -> AgentSnapshot {
        let state = self.lock();
        AgentSnapshot {
            running: state.run.is_some(),
            streaming: state
                .streaming
                .as_ref()
                .and_then(|acc| acc.message().cloned()),
            pending_tool_calls: state.pending_tool_calls.clone(),
            queued_steering: state.steering.iter().cloned().collect(),
            queued_follow_up: state.follow_up.iter().cloned().collect(),
        }
    }
}
