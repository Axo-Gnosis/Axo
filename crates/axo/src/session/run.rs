use std::sync::atomic::Ordering;
use std::sync::{Arc, MutexGuard, PoisonError, Weak};

use async_trait::async_trait;
use axo_agent::{
    Agent, AgentContext, AgentEvent, AgentMessage, CancelReason, HookError, LoopHooks,
    MessageOrigin, Persist, PersistError, QueuedMessage, RequestCache, RunOutcome, TurnContext,
    TurnFinish,
};
use axo_ai::{
    Accumulator, AssistantMessage, JsonMap, Message, Model, StopReason, ToolCall,
    ToolResultMessage, Usage, UserBlock,
};
use futures::FutureExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use super::{PendingNestedCall, SessionEvent, SessionHooks, journal};
use crate::compaction::Limits;
use crate::error::Error;
use crate::jobs::ChildProgress;
use crate::recovery;
use crate::store::{self, CompactionReason, Entry, EntryId};
use crate::tools::output::{MAX_YIELD_FAILURES, YIELD, YIELD_NUDGE};
use crate::tools::truncate::floor_char_boundary;

/// The open run's live state, updated and relayed under the same lock.
#[derive(Default)]
pub(super) struct LiveRun {
    /// The open run's id, set by its `run_start` (or by recovery).
    pub(super) run_id: Option<String>,
    pub(super) streaming: Option<Accumulator>,
    pub(super) tool_calls: Vec<ToolCall>,
    pub(super) nested: Vec<PendingNestedCall>,
    /// Assistant usage of the open run so far; seeded from the journal when a run resumes.
    pub(super) usage: Usage,
}

/// How `run_to_settle` starts its run.
#[allow(clippy::large_enum_variant)] // One owned startup value; boxing would add an allocation.
pub(super) enum RunRequest {
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
pub(super) async fn run_to_settle(
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
        Ok(result) => hooks.settle(&agent, result).await,
        Err(panic) => {
            let outcome = if hooks.shutdown.load(Ordering::Acquire) {
                RunOutcome::Aborted(CancelReason::Shutdown)
            } else {
                RunOutcome::Failed("Agent task panicked".into())
            };
            let _ = hooks.settle(&agent, Ok(outcome)).await;
            std::panic::resume_unwind(panic)
        }
    }
}

impl SessionHooks {
    pub(super) fn live(&self) -> MutexGuard<'_, LiveRun> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
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

    /// Show a child's executing tools on its parent's job row, so hosts see them without
    /// subscribing to the child.
    fn publish_child_tools(&self, tools: Vec<String>) {
        if let Some(parent) = self.parent_registry() {
            parent.child_tools(&self.session_id, tools);
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
        agent: &Agent,
        result: Result<RunOutcome, axo_agent::Error>,
    ) -> Result<RunOutcome, Error> {
        self.tools
            .runner
            .jobs
            .release_uncommitted(|id| agent.is_nested_invocation_active(id));
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

    fn request_cache(&self, model: &Model) -> RequestCache {
        self.cache_policy(model)
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

/// Keep small argument maps intact. For large ones, trim long strings without changing the JSON shape.
fn bounded_arguments(mut arguments: JsonMap) -> JsonMap {
    fn trim(value: &mut Value) {
        match value {
            Value::String(text) if text.len() > 1024 => {
                let bytes = text.len();
                text.truncate(floor_char_boundary(text, 1024));
                text.push_str(&format!("… [{bytes} bytes]"));
            }
            Value::Array(items) => items.iter_mut().for_each(trim),
            Value::Object(fields) => fields.values_mut().for_each(trim),
            _ => {}
        }
    }

    if serde_json::to_vec(&arguments)
        .expect("JSON arguments serialize")
        .len()
        > 8 * 1024
    {
        arguments.values_mut().for_each(trim);
    }
    arguments
}

fn nested_result_text(content: &[UserBlock]) -> String {
    let mut result = String::new();
    for (i, text) in content.iter().filter_map(UserBlock::as_text).enumerate() {
        if result.len() == 2048 {
            break;
        }
        if i != 0 {
            result.push('\n');
        }
        let end = floor_char_boundary(text, 2048 - result.len());
        result.push_str(&text[..end]);
        if end != text.len() {
            break;
        }
    }
    result
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
                parent_call_id,
            } => {
                let call = ToolCall {
                    id: call_id.clone(),
                    name: name.clone(),
                    arguments: args.clone(),
                    arguments_error: None,
                    thought_signature: None,
                };
                if let Some(root) = parent_call_id {
                    self.relay_live(event, |live| {
                        live.nested.push(PendingNestedCall {
                            parent_call_id: root.clone(),
                            call,
                        });
                    });
                } else {
                    let tools = self.relay_live(event, |live| {
                        live.tool_calls.push(call);
                        tool_names(live)
                    });
                    self.publish_child_tools(tools);
                }
                return Ok(());
            }
            AgentEvent::ToolExecutionEnd {
                call_id,
                name,
                result,
                parent_call_id: Some(root),
                invocation,
                duration_ms,
            } => {
                // As with message streaming, an attach in the append window receives the entry.
                let pending = self
                    .live()
                    .nested
                    .extract_if(.., |pending| {
                        &pending.parent_call_id == root && &pending.call.id == call_id
                    })
                    .next();
                let arguments = pending.map_or_else(JsonMap::new, |pending| {
                    bounded_arguments(pending.call.arguments)
                });
                let jobs = &self.tools.runner.jobs;
                let mut receipts = invocation.map_or_else(Default::default, |invocation| {
                    jobs.receipts(&MessageOrigin::Tool(invocation))
                });
                let mut batch = Vec::with_capacity(2);
                batch.extend(receipts.start.take());
                batch.push(Entry::NestedToolCall {
                    base: journal::base(),
                    root_call_id: root.clone(),
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments,
                    result: nested_result_text(&result.content),
                    is_error: result.is_error,
                    duration_ms: *duration_ms,
                    details: result.details.clone(),
                    admitted_jobs: std::mem::take(&mut receipts.admitted_jobs),
                    delivered_jobs: std::mem::take(&mut receipts.delivered_jobs),
                });
                let appended = self.append(batch).await;
                jobs.settle(receipts, appended.is_ok()).await;
                appended.map_err(|err| PersistError(err.to_string()))?;
            }
            AgentEvent::ToolExecutionEnd {
                call_id,
                parent_call_id: None,
                ..
            } => {
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
