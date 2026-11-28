//! The stateful agent: owns the transcript, queues and run cancellation around the
//! loop in [`crate::run`]. Events are delivered through [`Persist`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use axo_ai::{
    AssistantMessage, Message, Model, ServiceTier, SimpleStreamOptions, Streamer, ThinkingLevel,
    ToolResultMessage, Transcript,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::cancel::CancelReason;
use crate::error::Error;
use crate::event::{AgentEvent, Persist};
use crate::hooks::{AgentContext, LoopHooks, NoHooks, ToolHooks};
use crate::message::{AgentMessage, QueuedMessage, last_non_system};
use crate::run::{self, RunStart};
use crate::tool::{Tool, ToolInvocationId};

pub struct AgentConfig {
    pub streamer: Arc<dyn Streamer>,
    pub model: Model,
    pub thinking: ThinkingLevel,
    pub service_tier: ServiceTier,
    pub tools: Vec<Arc<dyn Tool>>,
    pub deferred: Vec<Arc<dyn Tool>>,
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
            service_tier: ServiceTier::Standard,
            tools: Vec::new(),
            deferred: Vec::new(),
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
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type,
)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
#[serde(
    rename = "AgentRunOutcome",
    tag = "type",
    content = "value",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RunOutcome {
    Done,
    Aborted(CancelReason),
    /// The last assistant turn ended with `StopReason::Error`; carries its error message.
    Failed(String),
}

/// A completed request, kept for re-sending its prefix (cache warming).
#[derive(Debug, Clone)]
pub struct SentRequest {
    pub model: Model,
    pub transcript: Transcript,
    pub options: SimpleStreamOptions,
    /// When the request was sent; the provider cache's lifetime starts here.
    pub started: std::time::Instant,
    /// Input plus cache-read and cache-write tokens of the response.
    pub prompt_tokens: u64,
}

pub struct Agent {
    pub(crate) streamer: Arc<dyn Streamer>,
    pub(crate) hooks: Arc<dyn LoopHooks>,
    pub(crate) tool_hooks: Mutex<Arc<[Arc<dyn ToolHooks>]>>,
    pub(crate) persist: Option<Arc<dyn Persist>>,
    pub(crate) steering_mode: QueueMode,
    pub(crate) follow_up_mode: QueueMode,
    pub(crate) session_id: Option<String>,
    pub(crate) state: Mutex<State>,
    /// True while `state.steering` is non-empty; written under the state lock.
    pub(crate) steer_pending: watch::Sender<bool>,
    /// The next [`ToolInvocationId`](crate::ToolInvocationId).
    next_invocation: AtomicU64,
    pub(crate) this: Weak<Agent>,
    pub(crate) nested_stop: CancellationToken,
    /// See [`Agent::last_request`].
    pub(crate) last_request: watch::Sender<Option<Arc<SentRequest>>>,
}

/// A withdrawn input and its position in the original queue.
pub struct RemovedQueued {
    message: QueuedMessage,
    steering: bool,
    behind: usize,
}

impl RemovedQueued {
    pub fn message(&self) -> &QueuedMessage {
        &self.message
    }
}

/// Mutable agent state. The lock is never held across an `.await`.
pub(crate) struct State {
    pub(crate) messages: Vec<AgentMessage>,
    pub(crate) model: Model,
    pub(crate) thinking: ThinkingLevel,
    pub(crate) service_tier: ServiceTier,
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) deferred: Vec<Arc<dyn Tool>>,
    pub(crate) steering: VecDeque<QueuedMessage>,
    pub(crate) follow_up: VecDeque<QueuedMessage>,
    /// The active run, if any.
    pub(crate) run: Option<ActiveRun>,
    /// Set by `Agent::shutdown`: every later run begins cancelled.
    pub(crate) shut_down: bool,
}

impl State {
    pub(crate) fn context(&self) -> AgentContext {
        AgentContext {
            messages: self.messages.clone(),
            tools: self.tools.clone(),
            deferred: self.deferred.clone(),
        }
    }

    pub(crate) fn shutting_down(&self) -> bool {
        self.shut_down
            || self.run.as_ref().and_then(|run| run.reason) == Some(CancelReason::Shutdown)
    }
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
    pub fn new(cfg: AgentConfig) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            streamer: cfg.streamer,
            hooks: cfg.hooks,
            tool_hooks: Mutex::new(cfg.tool_hooks.into()),
            persist: cfg.persist,
            steering_mode: cfg.steering_mode,
            follow_up_mode: cfg.follow_up_mode,
            session_id: cfg.session_id,
            state: Mutex::new(State {
                messages: Vec::new(),
                model: cfg.model,
                thinking: cfg.thinking,
                service_tier: cfg.service_tier,
                tools: cfg.tools,
                deferred: cfg.deferred,
                steering: VecDeque::new(),
                follow_up: VecDeque::new(),
                run: None,
                shut_down: false,
            }),
            steer_pending: watch::channel(false).0,
            next_invocation: AtomicU64::new(0),
            this: this.clone(),
            nested_stop: CancellationToken::new(),
            last_request: watch::channel(None).0,
        })
    }

    pub(crate) fn invocation(&self) -> ToolInvocationId {
        ToolInvocationId(self.next_invocation.fetch_add(1, Ordering::Relaxed))
    }

    /// During shutdown only AgentEnd is delivered, so the session can clear live state.
    pub(crate) async fn emit(&self, event: &AgentEvent) -> Result<bool, Error> {
        if !matches!(event, AgentEvent::AgentEnd { .. }) && self.lock().shutting_down() {
            return Ok(false);
        }
        if let Some(persist) = &self.persist {
            persist.persist(event).await?;
        }
        Ok(true)
    }

    /// Locks the state, recovering from a poisoned lock (no invariant spans a panic).
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The last completed request, kept only when `RequestCache::keep_warm`; `None` while a
    /// request streams or after the transcript or selection changed.
    pub fn last_request(&self) -> watch::Receiver<Option<Arc<SentRequest>>> {
        self.last_request.subscribe()
    }

    /// Replaces the tool hooks for hook passes that start after this returns.
    pub fn set_tool_hooks(&self, hooks: Vec<Arc<dyn ToolHooks>>) {
        *self
            .tool_hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = hooks.into();
    }

    /// The current tool hooks; a pass iterates this snapshot without holding the lock.
    pub(crate) fn tool_hooks(&self) -> Arc<[Arc<dyn ToolHooks>]> {
        self.tool_hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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

    fn sync_steer(&self, state: &State) {
        let pending = !state.steering.is_empty();
        self.steer_pending.send_if_modified(|value| {
            let changed = *value != pending;
            *value = pending;
            changed
        });
    }

    /// Queues a message injected after the current turn's tool calls finish, and raises
    /// the steer signal tools see until the queue is drained.
    pub fn steer(&self, m: QueuedMessage) {
        let mut state = self.lock();
        state.steering.push_back(m);
        self.sync_steer(&state);
    }

    /// Queues a message processed only when the run would otherwise end.
    pub fn follow_up(&self, m: QueuedMessage) {
        self.lock().follow_up.push_back(m);
    }

    /// Ids of the queued steering, then follow-up messages that carry one.
    pub fn queued_ids(&self) -> Vec<String> {
        let state = self.lock();
        state
            .steering
            .iter()
            .chain(&state.follow_up)
            .filter_map(|m| m.id.clone())
            .collect()
    }

    /// Removes an input, returning a token that can restore it to this agent. For
    /// rollback, serialize producers and other removals on that queue; consumers
    /// may still drain the front.
    pub fn remove_queued(&self, id: &str) -> Option<RemovedQueued> {
        let mut state = self.lock();
        let is = |m: &QueuedMessage| m.id.as_deref() == Some(id);
        if let Some(at) = state.steering.iter().position(is) {
            let behind = state.steering.len() - at - 1;
            let message = state.steering.remove(at)?;
            self.sync_steer(&state);
            return Some(RemovedQueued {
                message,
                steering: true,
                behind,
            });
        }
        let at = state.follow_up.iter().position(is)?;
        let behind = state.follow_up.len() - at - 1;
        let message = state.follow_up.remove(at)?;
        Some(RemovedQueued {
            message,
            steering: false,
            behind,
        })
    }

    /// Restores a removed message relative to the surviving suffix of its original
    /// queue. See `remove_queued` for the serialization required during rollback.
    pub fn restore_queued(&self, removed: RemovedQueued) {
        let RemovedQueued {
            message,
            steering,
            behind,
        } = removed;
        let mut state = self.lock();
        let queue = if steering {
            &mut state.steering
        } else {
            &mut state.follow_up
        };
        queue.insert(queue.len().saturating_sub(behind), message);
        if steering {
            self.sync_steer(&state);
        }
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
        if steering {
            self.sync_steer(&state);
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
        self.nested_stop.cancel();
        if let Some(run) = &mut state.run {
            run.reason.get_or_insert(CancelReason::Shutdown);
            run.cancel.cancel();
        }
    }

    pub fn messages(&self) -> Vec<AgentMessage> {
        self.lock().messages.clone()
    }

    /// Replaces the transcript; a different one drops [`Agent::last_request`].
    pub fn set_messages(&self, m: Vec<AgentMessage>) {
        let mut state = self.lock();
        if state.messages != m {
            state.messages = m;
            drop(state);
            self.last_request.send_replace(None);
        }
    }

    /// Installs the next request's selection under the same lock used to sample it; a
    /// different one drops [`Agent::last_request`].
    pub fn set_selection(&self, model: Option<Model>, thinking: ThinkingLevel, tier: ServiceTier) {
        let mut state = self.lock();
        let changed = model.as_ref().is_some_and(|model| *model != state.model)
            || state.thinking != thinking
            || state.service_tier != tier;
        if let Some(model) = model {
            state.model = model;
        }
        state.thinking = thinking;
        state.service_tier = tier;
        drop(state);
        if changed {
            self.last_request.send_replace(None);
        }
    }

    /// Takes effect from the next turn; the change is declared to the model with a
    /// system message.
    pub fn set_tools(&self, t: Vec<Arc<dyn Tool>>) {
        self.lock().tools = t;
    }

    /// Takes effect from the next turn.
    pub fn set_deferred(&self, tools: Vec<Arc<dyn Tool>>) {
        self.lock().deferred = tools;
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
