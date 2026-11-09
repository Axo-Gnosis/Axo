//! The stateful agent: owns the transcript, queues and run cancellation around the
//! loop in [`crate::run`]. Events are delivered through [`Persist`].

use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard};

use axo_ai::{AssistantMessage, Message, Model, Streamer, ThinkingLevel, ToolResultMessage};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::cancel::CancelReason;
use crate::error::Error;
use crate::event::Persist;
use crate::hooks::{LoopHooks, NoHooks, ToolHooks};
use crate::message::{AgentMessage, QueuedMessage, last_non_system};
use crate::run::{self, RunStart};
use crate::tool::Tool;

pub struct AgentConfig {
    pub streamer: Arc<dyn Streamer>,
    pub model: Model,
    pub thinking: ThinkingLevel,
    pub tools: Vec<Arc<dyn Tool>>,
    pub hooks: Arc<dyn LoopHooks>,
    pub tool_hooks: Vec<Arc<dyn ToolHooks>>,
    /// The ordered, awaited subscriber; see [`Persist`].
    pub persist: Option<Arc<dyn Persist>>,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    /// Forwarded to providers for cache-aware backends.
    pub session_id: Option<String>,
}

impl AgentConfig {
    /// A config with no hooks or persistence, one-at-a-time queues and thinking off.
    pub fn new(streamer: Arc<dyn Streamer>, model: Model) -> Self {
        Self {
            streamer,
            model,
            thinking: ThinkingLevel::Off,
            tools: Vec::new(),
            hooks: Arc::new(NoHooks),
            tool_hooks: Vec::new(),
            persist: None,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            session_id: None,
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

pub struct Agent {
    pub(crate) streamer: Arc<dyn Streamer>,
    pub(crate) hooks: Arc<dyn LoopHooks>,
    pub(crate) tool_hooks: Vec<Arc<dyn ToolHooks>>,
    pub(crate) persist: Option<Arc<dyn Persist>>,
    pub(crate) steering_mode: QueueMode,
    pub(crate) follow_up_mode: QueueMode,
    pub(crate) session_id: Option<String>,
    pub(crate) state: Mutex<State>,
    /// True while `state.steering` is non-empty; written under the state lock.
    pub(crate) steer_pending: watch::Sender<bool>,
    /// The next [`ToolInvocationId`](crate::ToolInvocationId).
    pub(crate) next_invocation: AtomicU64,
}

/// Mutable agent state. The lock is never held across an `.await`.
pub(crate) struct State {
    pub(crate) messages: Vec<AgentMessage>,
    pub(crate) model: Model,
    pub(crate) thinking: ThinkingLevel,
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) steering: VecDeque<QueuedMessage>,
    pub(crate) follow_up: VecDeque<QueuedMessage>,
    /// The active run, if any.
    pub(crate) run: Option<ActiveRun>,
    /// Set by `Agent::shutdown`: every later run begins cancelled.
    pub(crate) shut_down: bool,
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
    }
}

impl Agent {
    pub fn new(cfg: AgentConfig) -> Self {
        Self {
            streamer: cfg.streamer,
            hooks: cfg.hooks,
            tool_hooks: cfg.tool_hooks,
            persist: cfg.persist,
            steering_mode: cfg.steering_mode,
            follow_up_mode: cfg.follow_up_mode,
            session_id: cfg.session_id,
            state: Mutex::new(State {
                messages: Vec::new(),
                model: cfg.model,
                thinking: cfg.thinking,
                tools: cfg.tools,
                steering: VecDeque::new(),
                follow_up: VecDeque::new(),
                run: None,
                shut_down: false,
            }),
            steer_pending: watch::channel(false).0,
            next_invocation: AtomicU64::new(0),
        }
    }

    /// Locks the state, recovering from a poisoned lock (no invariant spans a panic).
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers a new run; `Error::Busy` if one is active. After `shutdown` the run
    /// begins cancelled with `CancelReason::Shutdown`.
    fn begin_run(state: &mut State) -> Result<CancellationToken, Error> {
        if state.run.is_some() {
            return Err(Error::Busy);
        }
        let cancel = CancellationToken::new();
        let mut reason = None;
        if state.shut_down {
            cancel.cancel();
            reason = Some(CancelReason::Shutdown);
        }
        state.run = Some(ActiveRun {
            cancel: cancel.clone(),
            reason,
        });
        Ok(cancel)
    }

    /// Starts a run with these messages. Returns when the run settles.
    pub async fn prompt(&self, messages: Vec<QueuedMessage>) -> Result<RunOutcome, Error> {
        let cancel = Self::begin_run(&mut self.lock())?;
        let _guard = RunGuard(self);
        run::run_loop(self, RunStart::Prompt(messages), cancel).await
    }

    /// Continues from the current transcript: executes the unanswered tool calls of a
    /// trailing assistant message, or requests a new turn after a user/tool-result
    /// message. A trailing assistant message without tool calls is a settled reply: with
    /// queued steering or follow-ups the run starts from them, otherwise this returns
    /// `Done` without emitting events.
    pub async fn continue_run(&self) -> Result<RunOutcome, Error> {
        let cancel = {
            let mut state = self.lock();
            if state.run.is_some() {
                return Err(Error::Busy);
            }
            let last = last_non_system(&state.messages);
            match last {
                None => return Err(Error::NothingToContinue),
                Some(AgentMessage::Llm(Message::Assistant(message)))
                    if message.tool_calls().next().is_none()
                        && state.steering.is_empty()
                        && state.follow_up.is_empty() =>
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

    /// Classifies an already persisted response without streaming or replaying its
    /// completed calls. Recovery and any subsequent requests share one run lifetime.
    pub async fn recover_response(
        &self,
        response: AssistantMessage,
        model: Model,
        tool_results: Vec<ToolResultMessage>,
    ) -> Result<RunOutcome, Error> {
        let cancel = Self::begin_run(&mut self.lock())?;
        let _guard = RunGuard(self);
        run::run_loop(
            self,
            RunStart::RecoverResponse {
                response,
                model,
                tool_results,
            },
            cancel,
        )
        .await
    }

    /// Queues a message injected after the current turn's tool calls finish, and raises
    /// the steer signal tools see until the queue is drained.
    pub fn steer(&self, m: QueuedMessage) {
        let mut state = self.lock();
        state.steering.push_back(m);
        self.steer_pending.send_replace(true);
    }

    /// Queues a message processed only when the run would otherwise end.
    pub fn follow_up(&self, m: QueuedMessage) {
        self.lock().follow_up.push_back(m);
    }

    /// Takes the next messages of the steering (`true`) or follow-up queue per its mode.
    /// Draining the last steering message clears the steer signal under the same lock.
    pub(crate) fn take_queued(&self, steering: bool) -> Vec<QueuedMessage> {
        let mut state = self.lock();
        let (queue, mode) = if steering {
            (&mut state.steering, self.steering_mode)
        } else {
            (&mut state.follow_up, self.follow_up_mode)
        };
        let taken = match mode {
            QueueMode::All => queue.drain(..).collect(),
            QueueMode::OneAtATime => queue.pop_front().into_iter().collect(),
        };
        if steering && queue.is_empty() {
            self.steer_pending.send_replace(false);
        }
        taken
    }

    /// What a run started from idle begins with: the next steering messages per the
    /// steering mode, else the next follow-ups per the follow-up mode. The rest stay
    /// queued for the run's own drain points.
    pub fn take_next_input(&self) -> Vec<QueuedMessage> {
        let steering = self.take_queued(true);
        if steering.is_empty() {
            self.take_queued(false)
        } else {
            steering
        }
    }

    /// Cancels the active run, if any. The first reason given for a run wins.
    pub fn cancel(&self, reason: CancelReason) {
        let mut state = self.lock();
        if let Some(run) = &mut state.run {
            run.reason.get_or_insert(reason);
            run.cancel.cancel();
        }
    }

    /// Stops the agent for good: the active run is cancelled with `CancelReason::Shutdown`,
    /// and every run started afterwards begins already cancelled the same way.
    pub fn shutdown(&self) {
        let mut state = self.lock();
        state.shut_down = true;
        if let Some(run) = &mut state.run {
            run.reason.get_or_insert(CancelReason::Shutdown);
            run.cancel.cancel();
        }
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

    /// The queued steering and follow-up messages, in delivery order.
    pub fn queued(&self) -> (Vec<AgentMessage>, Vec<AgentMessage>) {
        let state = self.lock();
        (
            state.steering.iter().map(|m| m.message.clone()).collect(),
            state.follow_up.iter().map(|m| m.message.clone()).collect(),
        )
    }
}
