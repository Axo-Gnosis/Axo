//! The loop itself (pi `agent/src/agent-loop.ts`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axo_ai::transcript::{replay_tools, tool_state_changes};
use axo_ai::{
    Accumulator, AssistantEvent, AssistantMessage, Message, Model, SimpleStreamOptions, StopReason,
    SystemMessage, ThinkingLevel, ToolCall, ToolResultMessage, ToolSpec, Transcript, UserBlock,
    now_ms,
};
use futures::StreamExt;
use jsonschema::error::ValidationErrorKind;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::{Id, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, RunOutcome};
use crate::cancel::CancelReason;
use crate::error::Error;
use crate::event::{AgentEvent, MessageOrigin};
use crate::hooks::{
    AfterToolCallContext, AgentContext, BeforeToolCallContext, ToolCallDecision, TurnContext,
};
use crate::message::{AgentMessage, QueuedMessage, last_non_system, unanswered_calls};
use crate::tool::{
    ExecutionMode, SteerSignal, Tool, ToolContext, ToolError, ToolInvocationId, ToolOutput,
    ToolUpdate, ToolUpdates,
};

/// How a run starts.
#[allow(clippy::large_enum_variant)] // One owned startup value; boxing would add an allocation.
pub(crate) enum RunStart {
    /// `Agent::prompt`: append and emit these messages, then request a turn.
    Prompt(Vec<QueuedMessage>),
    /// `Agent::continue_run` on a non-empty transcript whose last message is a user or
    /// tool-result message, an assistant message with unanswered tool calls (those calls
    /// run first), or a settled reply with queued steering or follow-ups.
    Continue,
    RecoverResponse {
        response: AssistantMessage,
        model: Model,
        tool_results: Vec<ToolResultMessage>,
    },
}

/// Runs one agent run to completion. `cancel` is the run's token (already registered in
/// the agent's state with its reason slot); the caller clears the run state afterwards.
pub(crate) async fn run_loop(
    agent: &Agent,
    start: RunStart,
    cancel: CancellationToken,
) -> Result<RunOutcome, Error> {
    let context = {
        let state = agent.lock();
        AgentContext {
            messages: state.messages.clone(),
            tools: state.tools.clone(),
        }
    };
    let mut run = Run {
        agent,
        cancel,
        context,
        new_messages: Vec::new(),
    };
    run.run(start).await
}

struct Response {
    message: AssistantMessage,
    model: Model,
    results: Vec<ToolResultMessage>,
    recovered: bool,
}

struct Completed {
    results: Vec<ToolResultMessage>,
    continue_turn: bool,
    outcome: Option<RunOutcome>,
}

/// One finished tool call, before it becomes a tool-result message.
struct Outcome {
    call_id: String,
    name: String,
    output: ToolOutput,
    duration_ms: Option<u64>,
    /// Set once the tool is executed; kept whatever it returns. `None` for results the
    /// loop synthesizes, so the persisted result keeps its provenance.
    invocation: Option<ToolInvocationId>,
}

impl Outcome {
    fn synthetic(call: &ToolCall, output: ToolOutput) -> Self {
        Self {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output,
            duration_ms: None,
            invocation: None,
        }
    }
}

/// What preparing a call yields (pi `prepareToolCall`).
enum Prepared {
    /// Not executed: unknown tool, invalid arguments, blocked, or aborted.
    Immediate(ToolOutput),
    Ready(Arc<dyn Tool>),
}

/// How long a stopped call may take to return on its own before its task is aborted.
/// Covers bash's SIGTERM→SIGKILL grace (1 s) plus its drain.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// The spawned calls of one batch, their shared cancellation, and the channel their
/// updates arrive on.
// ponytail: a loop panic drops the batch unjoined; stragglers are bounded by the job registry's settle-time cleanup
struct Batch {
    tasks: JoinSet<(Result<ToolOutput, ToolError>, u64)>,
    calls: HashMap<Id, Spawned>,
    /// A child of the run's token; also fired when persisting the batch fails.
    cancel: CancellationToken,
    sender: mpsc::UnboundedSender<(Arc<str>, ToolUpdate)>,
    updates: mpsc::UnboundedReceiver<(Arc<str>, ToolUpdate)>,
}

impl Batch {
    fn new(run_cancel: &CancellationToken) -> Self {
        let (sender, updates) = mpsc::unbounded_channel();
        Self {
            tasks: JoinSet::new(),
            calls: HashMap::new(),
            cancel: run_cancel.child_token(),
            sender,
            updates,
        }
    }
}

/// One running call, keyed by its task id.
struct Spawned {
    index: usize,
    call: Arc<ToolCall>,
    invocation: ToolInvocationId,
    started: Instant,
}

/// The state of one run (pi `runLoop`'s locals).
struct Run<'a> {
    agent: &'a Agent,
    cancel: CancellationToken,
    /// The loop's working context; hooks may replace it. Its tools are refreshed from the
    /// agent before each turn's request, so `set_tools` applies from the next turn.
    context: AgentContext,
    /// Messages this run added, for `AgentEnd`.
    new_messages: Vec<AgentMessage>,
}

impl Run<'_> {
    async fn run(&mut self, start: RunStart) -> Result<RunOutcome, Error> {
        let agent = self.agent;
        self.emit(&AgentEvent::AgentStart).await?;
        self.emit(&AgentEvent::TurnStart).await?;

        let mut resume: Option<Response> = None;
        let mut pending = Vec::new();
        match start {
            RunStart::Prompt(messages) => {
                let messages = messages.into_iter().map(input).collect();
                for (message, origin) in declare_tool_changes(&self.context, messages) {
                    self.add_message(message, origin).await?;
                }
                pending = self.drain_queue(true).await;
            }
            RunStart::RecoverResponse {
                response,
                model,
                tool_results,
            } => {
                resume = Some(Response {
                    message: response,
                    model,
                    results: tool_results,
                    recovered: true,
                });
            }
            RunStart::Continue => {
                resume =
                    unanswered_calls(self.context.messages.iter()).map(|unanswered| Response {
                        message: unanswered.message.clone(),
                        model: self.model().0,
                        results: unanswered.results.into_iter().cloned().collect(),
                        recovered: false,
                    });
                if resume.is_none() {
                    pending = self.drain_queue(true).await;
                    let after_reply = matches!(
                        last_non_system(&self.context.messages),
                        Some(AgentMessage::Llm(Message::Assistant(_)))
                    );
                    if pending.is_empty() && after_reply {
                        pending = self.drain_queue(false).await;
                    }
                }
            }
        }

        let mut last_turn: Option<Response> = None;
        loop {
            let Response {
                message,
                model,
                results,
                recovered,
            } = match resume.take() {
                Some(resumed) => resumed,
                None => {
                    if self.cancel.is_cancelled() {
                        return self.finish_aborted().await;
                    }
                    let mut prepared = Vec::new();
                    if let Some(Response {
                        message,
                        model,
                        results: tool_results,
                        ..
                    }) = &last_turn
                    {
                        let messages = agent
                            .hooks
                            .prepare_next_turn(
                                TurnContext {
                                    message,
                                    model,
                                    tool_results,
                                    context: &self.context,
                                },
                                &self.cancel,
                            )
                            .await;
                        if self.cancel.is_cancelled() {
                            return self.finish_aborted().await;
                        }
                        prepared = messages
                            .into_iter()
                            .map(|message| (message, MessageOrigin::Run))
                            .collect();
                        if pending.is_empty() {
                            pending = self.drain_queue(true).await;
                        }
                        self.emit(&AgentEvent::TurnStart).await?;
                    }
                    self.context.tools = agent.lock().tools.clone();
                    prepared.extend(pending.drain(..).map(input));
                    for (message, origin) in declare_tool_changes(&self.context, prepared) {
                        self.add_message(message, origin).await?;
                    }
                    if self.cancel.is_cancelled() {
                        return self.finish_aborted().await;
                    }
                    let update = agent
                        .hooks
                        .prepare_request(&self.context, &self.cancel)
                        .await;
                    match update {
                        Ok(Some(messages)) => self.replace_messages(messages),
                        Ok(None) => {}
                        Err(_) if self.cancel.is_cancelled() => return self.finish_aborted().await,
                        Err(error) => return self.finish(RunOutcome::Failed(error.0)).await,
                    }
                    if self.cancel.is_cancelled() {
                        return self.finish_aborted().await;
                    }
                    let Some((message, model)) = self.stream_turn().await? else {
                        return self.finish_aborted().await;
                    };
                    Response {
                        message,
                        model,
                        results: Vec::new(),
                        recovered: false,
                    }
                }
            };

            let Completed {
                results,
                continue_turn,
                outcome,
            } = self
                .complete_response(&message, &model, results, recovered)
                .await?;
            if let Some(outcome) = outcome {
                return self.finish(outcome).await;
            }
            pending = self.drain_queue(true).await;
            if !continue_turn && pending.is_empty() {
                pending = self.drain_queue(false).await;
                if pending.is_empty() {
                    return self.finish(RunOutcome::Done).await;
                }
            }
            last_turn = Some(Response {
                message,
                model,
                results,
                recovered,
            });
        }
    }

    /// Fresh and recovered responses share classification, result completion and
    /// maintenance ordering. Recovery never executes a missing call.
    async fn complete_response(
        &mut self,
        message: &AssistantMessage,
        model: &Model,
        existing: Vec<ToolResultMessage>,
        recovered: bool,
    ) -> Result<Completed, Error> {
        let calls: Vec<ToolCall> = message.tool_calls().cloned().collect();
        let mut results = Vec::with_capacity(calls.len());
        let skipped = self.agent.hooks.should_skip_tools(message, model);
        let failed = message.stop_reason == StopReason::Error;
        let aborted = message.stop_reason == StopReason::Aborted || self.cancel.is_cancelled();
        let missing: Vec<ToolCall> = calls
            .iter()
            .filter(|call| !existing.iter().any(|result| result.tool_call_id == call.id))
            .cloned()
            .collect();
        let mut terminate = false;
        let produced = if aborted {
            Vec::new()
        } else if skipped || failed {
            self.fail_calls(&missing, false).await?
        } else if message.stop_reason == StopReason::Length {
            self.fail_calls(&missing, true).await?
        } else if recovered {
            self.fail_calls(&missing, false).await?
        } else {
            let (produced, ended) = self.execute_tool_calls(message, &missing).await?;
            terminate = ended;
            produced
        };
        let mut available = existing;
        available.extend(produced);
        for call in &calls {
            if let Some(index) = available
                .iter()
                .position(|result| result.tool_call_id == call.id)
            {
                results.push(available.remove(index));
            }
        }
        if aborted || self.cancel.is_cancelled() {
            self.complete_aborted_results(&calls, &mut results).await?;
        }
        let finish = if self.cancel_reason() != Some(CancelReason::Shutdown) {
            Some(
                self.agent
                    .hooks
                    .finish_turn(
                        TurnContext {
                            message,
                            model,
                            tool_results: &results,
                            context: &self.context,
                        },
                        &self.cancel,
                    )
                    .await,
            )
        } else {
            None
        };
        // Cancellation wins over errors and continuation, even when the hook returned
        // just as cancellation arrived. Do not invoke the hook a second time.
        let cancelled = aborted || self.cancel.is_cancelled();
        let mut continue_turn = false;
        let mut outcome = None;
        if let Some(finish) = finish {
            match finish {
                Ok(finish) => {
                    if let Some(messages) = finish.messages {
                        self.replace_messages(messages);
                    }
                    continue_turn = finish.continue_turn;
                }
                Err(error) => outcome = Some(RunOutcome::Failed(error.0)),
            }
        }
        if cancelled {
            self.complete_aborted_results(&calls, &mut results).await?;
            outcome = Some(RunOutcome::Aborted(
                self.cancel_reason().unwrap_or(CancelReason::User),
            ));
        }
        self.emit(&AgentEvent::TurnEnd {
            message: AgentMessage::Llm(Message::Assistant(message.clone())),
            tool_results: results.clone(),
        })
        .await?;
        if outcome.is_none() && !continue_turn && (failed || skipped) {
            outcome = Some(RunOutcome::Failed(
                message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Model response could not be used".into()),
            ));
        }
        continue_turn |= !calls.is_empty() && !terminate;
        Ok(Completed {
            results,
            continue_turn,
            outcome,
        })
    }

    async fn finish_aborted(&mut self) -> Result<RunOutcome, Error> {
        self.finish(RunOutcome::Aborted(
            self.cancel_reason().unwrap_or(CancelReason::User),
        ))
        .await
    }

    /// Streams one assistant response (pi `streamAssistantResponse`) and adds it to the
    /// transcript, unless the run is shutting down.
    async fn stream_turn(&mut self) -> Result<Option<(AssistantMessage, Model)>, Error> {
        let agent = self.agent;
        let messages = agent.hooks.convert_to_llm(self.context.messages.clone());
        if self.cancel.is_cancelled() {
            return Ok(None);
        }
        let transcript = Transcript::new(messages);
        let (model, thinking) = self.model();
        let options = SimpleStreamOptions {
            thinking,
            session_id: agent.session_id.clone(),
            ..SimpleStreamOptions::default()
        };
        let mut stream =
            agent
                .streamer
                .stream(&model, transcript, options, self.cancel.child_token());

        let mut accumulator = Accumulator::new();
        let mut started = false;
        let mut ended = false;
        while let Some(event) = stream.next().await {
            accumulator.apply(&event);
            if !started {
                let partial = match &event {
                    AssistantEvent::Start { partial } => Some(partial),
                    // A setup failure ends the stream without `Start`.
                    AssistantEvent::Done { message, .. }
                    | AssistantEvent::Error { message, .. } => Some(message),
                    _ => None,
                };
                if let Some(partial) = partial {
                    started = true;
                    let message = AgentMessage::Llm(Message::Assistant(partial.clone()));
                    self.emit(&AgentEvent::MessageStart { message }).await?;
                }
            }
            let terminal = event.is_terminal();
            self.emit(&AgentEvent::MessageUpdate { event }).await?;
            if terminal {
                ended = true;
                break;
            }
        }
        drop(stream);

        let mut message = match accumulator.into_message() {
            Some(message) if ended => message,
            partial => {
                let mut message = partial.unwrap_or_else(|| AssistantMessage::empty(&model));
                if self.cancel.is_cancelled() {
                    message.stop_reason = StopReason::Aborted;
                    message.error_message = Some("Request was aborted".into());
                } else {
                    message.stop_reason = StopReason::Error;
                    message.error_message =
                        Some("Provider stream ended without a final event".into());
                }
                message
            }
        };
        message.thinking_level = Some(thinking);
        if !started {
            let start = AgentMessage::Llm(Message::Assistant(message.clone()));
            self.emit(&AgentEvent::MessageStart { message: start })
                .await?;
        }
        self.end_message(
            AgentMessage::Llm(Message::Assistant(message.clone())),
            MessageOrigin::Run,
        )
        .await?;
        Ok(Some((message, model)))
    }

    /// Runs the calls of one assistant message (pi `executeToolCalls`). Returns their
    /// result messages in call order and whether the batch asked to terminate.
    async fn execute_tool_calls(
        &mut self,
        message: &AssistantMessage,
        calls: &[ToolCall],
    ) -> Result<(Vec<ToolResultMessage>, bool), Error> {
        let sequential = calls.iter().any(|call| {
            self.context.tools.iter().any(|tool| {
                tool.name() == call.name && tool.execution_mode() == ExecutionMode::Sequential
            })
        });
        // The batch terminates only when every produced result asks for it.
        let mut terminates: Vec<bool> = Vec::with_capacity(calls.len());
        let mut results = Vec::with_capacity(calls.len());

        for calls in calls.chunks(if sequential { 1 } else { calls.len().max(1) }) {
            let mut slots: Vec<Option<Outcome>> = calls.iter().map(|_| None).collect();
            let mut ready = Vec::with_capacity(calls.len());
            // Preparation, including `before_tool_call`, runs in source order and clears
            // the whole batch before any call starts.
            for (index, call) in calls.iter().enumerate() {
                self.emit_start(call).await?;
                match self.prepare(message, call).await {
                    Prepared::Immediate(output) => {
                        let outcome = Outcome::synthetic(call, output);
                        self.emit_end(&outcome).await?;
                        slots[index] = Some(outcome);
                    }
                    Prepared::Ready(tool) => ready.push((index, tool, call)),
                }
                if self.cancel.is_cancelled() {
                    break;
                }
            }
            if self.cancel.is_cancelled() {
                for (index, _, call) in ready {
                    let outcome = Outcome::synthetic(call, error_output("Operation aborted"));
                    self.emit_end(&outcome).await?;
                    slots[index] = Some(outcome);
                }
            } else {
                let mut batch = Batch::new(&self.cancel);
                for (index, tool, call) in ready {
                    self.spawn_call(&mut batch, index, tool, call);
                }
                for (index, outcome) in self.drive(message, batch).await? {
                    slots[index] = Some(outcome);
                }
            }
            // Result messages follow the order the model issued the calls.
            for outcome in slots.into_iter().flatten() {
                terminates.push(outcome.output.terminate);
                results.push(self.add_tool_result(&outcome).await?);
            }
            if self.cancel.is_cancelled() {
                break;
            }
        }

        let terminate = !terminates.is_empty() && terminates.iter().all(|&terminate| terminate);
        Ok((results, terminate))
    }

    /// Looks up, validates and clears one call with `before_tool_call`.
    async fn prepare(&self, message: &AssistantMessage, call: &ToolCall) -> Prepared {
        let Some(tool) = self
            .context
            .tools
            .iter()
            .find(|tool| tool.name() == call.name)
        else {
            return Prepared::Immediate(error_output(format!("Tool {} not found", call.name)));
        };
        if let Err(text) = validate_arguments(&**tool, call) {
            return Prepared::Immediate(error_output(text));
        }
        for hook in &self.agent.tool_hooks {
            let decision = hook
                .before_tool_call(
                    BeforeToolCallContext {
                        assistant_message: message,
                        tool_call: call,
                        context: &self.context,
                    },
                    &self.cancel,
                )
                .await;
            match decision {
                Err(error) => return Prepared::Immediate(error_output(error.0)),
                Ok(_) if self.cancel.is_cancelled() => {
                    return Prepared::Immediate(error_output("Operation aborted"));
                }
                Ok(ToolCallDecision::Block { reason, terminate }) => {
                    let mut output =
                        error_output(reason.unwrap_or_else(|| "Tool execution was blocked".into()));
                    output.terminate = terminate;
                    return Prepared::Immediate(output);
                }
                Ok(ToolCallDecision::Allow) => {}
            }
        }
        if self.cancel.is_cancelled() {
            Prepared::Immediate(error_output("Operation aborted"))
        } else {
            Prepared::Ready(tool.clone())
        }
    }

    /// Starts one prepared call on its own task, so the loop never polls tool futures:
    /// a tool waiting on something the loop's `Persist` holds cannot stall the loop. The
    /// call's token is a child of the batch's, so Stop and a failed persist both reach it.
    fn spawn_call(&self, batch: &mut Batch, index: usize, tool: Arc<dyn Tool>, call: &ToolCall) {
        let call = Arc::new(call.clone());
        let invocation =
            ToolInvocationId(self.agent.next_invocation.fetch_add(1, Ordering::Relaxed));
        let call_id: Arc<str> = Arc::from(call.id.as_str());
        let sender = batch.sender.clone();
        let cx = ToolContext {
            call: Arc::clone(&call),
            cancel: batch.cancel.child_token(),
            updates: ToolUpdates::new(move |update| {
                let _ = sender.send((Arc::clone(&call_id), update));
            }),
            steer: SteerSignal(self.agent.steer_pending.subscribe()),
            invocation,
        };
        let started = Instant::now();
        let task = batch.tasks.spawn(async move {
            let result = tool.execute(cx).await;
            (result, elapsed_ms(started))
        });
        batch.calls.insert(
            task.id(),
            Spawned {
                index,
                call,
                invocation,
                started,
            },
        );
    }

    /// Awaits the batch's tasks, forwarding their updates, and emits each
    /// `ToolExecutionEnd` (after `after_tool_call`, on this task) as its call completes.
    /// Cancellation is cooperative: each call's token fires and the loop keeps joining;
    /// calls still running `STOP_GRACE` later are aborted and end as aborted. A
    /// persistence failure stops the batch the same way and is returned only after every
    /// call was joined. A panicked call ends as an error. Returns the outcomes in
    /// completion order.
    async fn drive(
        &self,
        message: &AssistantMessage,
        mut batch: Batch,
    ) -> Result<Vec<(usize, Outcome)>, Error> {
        drop(batch.sender);
        let mut finished = Vec::with_capacity(batch.calls.len());
        let mut stopping: Option<Instant> = None;
        let mut aborted = false;
        let mut failure: Option<Error> = None;
        loop {
            let joined = tokio::select! {
                biased;
                Some((call_id, update)) = batch.updates.recv() => {
                    if failure.is_none()
                        && let Err(error) = self.emit_update(call_id, update).await
                    {
                        failure = Some(error);
                        batch.cancel.cancel();
                    }
                    continue;
                }
                () = batch.cancel.cancelled(), if stopping.is_none() => {
                    stopping = Some(Instant::now() + STOP_GRACE);
                    continue;
                }
                () = tokio::time::sleep_until(stopping.unwrap_or_else(Instant::now).into()),
                    if stopping.is_some() && !aborted =>
                {
                    batch.tasks.abort_all();
                    aborted = true;
                    continue;
                }
                joined = batch.tasks.join_next_with_id() => joined,
            };
            let Some(joined) = joined else { break };
            let (id, result) = match joined {
                Ok((id, result)) => (id, Ok(result)),
                Err(error) => (error.id(), Err(error)),
            };
            let Some(spawned) = batch.calls.remove(&id) else {
                continue;
            };
            if failure.is_some() {
                continue;
            }
            let (output, duration_ms, executed) = match result {
                Ok((result, duration_ms)) => (
                    result.unwrap_or_else(|error| error_output(error.0)),
                    duration_ms,
                    true,
                ),
                // Aborted after the grace above: the call never returned.
                Err(error) if error.is_cancelled() => (
                    error_output("Operation aborted"),
                    elapsed_ms(spawned.started),
                    false,
                ),
                Err(error) => (
                    error_output(format!("Tool {} crashed: {error}", spawned.call.name)),
                    elapsed_ms(spawned.started),
                    true,
                ),
            };
            let mut outcome = Outcome {
                call_id: spawned.call.id.clone(),
                name: spawned.call.name.clone(),
                output,
                duration_ms: Some(duration_ms),
                invocation: Some(spawned.invocation),
            };
            if executed {
                self.after_tool_call(message, &spawned.call, &mut outcome.output)
                    .await;
            }
            let emitted = async {
                // Updates sent before the call returned precede its end.
                while let Ok((call_id, update)) = batch.updates.try_recv() {
                    self.emit_update(call_id, update).await?;
                }
                self.emit_end(&outcome).await
            }
            .await;
            match emitted {
                Ok(()) => finished.push((spawned.index, outcome)),
                Err(error) => {
                    failure = Some(error);
                    batch.cancel.cancel();
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(finished),
        }
    }

    /// Applies `after_tool_call` overrides to an executed call's output.
    async fn after_tool_call(
        &self,
        message: &AssistantMessage,
        call: &ToolCall,
        output: &mut ToolOutput,
    ) {
        for hook in &self.agent.tool_hooks {
            let overrides = hook
                .after_tool_call(
                    AfterToolCallContext {
                        assistant_message: message,
                        tool_call: call,
                        result: &*output,
                        context: &self.context,
                    },
                    &self.cancel,
                )
                .await;
            match overrides {
                Ok(Some(overrides)) => {
                    if let Some(content) = overrides.content {
                        output.content = content;
                    }
                    if let Some(details) = overrides.details {
                        output.details = Some(details);
                    }
                    if let Some(is_error) = overrides.is_error {
                        output.is_error = is_error;
                    }
                    if let Some(terminate) = overrides.terminate {
                        output.terminate = terminate;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(tool = %call.name, %error, "after_tool_call failed; keeping the executed result");
                }
            }
        }
    }

    async fn emit_update(&self, call_id: Arc<str>, update: ToolUpdate) -> Result<(), Error> {
        self.emit(&AgentEvent::ToolExecutionUpdate {
            call_id: call_id.as_ref().to_owned(),
            update,
        })
        .await
    }

    /// Produces ordered nonexecuted results; ordinary Length keeps its diagnostic.
    async fn fail_calls(
        &mut self,
        calls: &[ToolCall],
        truncated: bool,
    ) -> Result<Vec<ToolResultMessage>, Error> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            self.emit_start(call).await?;
            let outcome = Outcome::synthetic(
                call,
                error_output(if truncated {
                    format!(
                        "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                        call.name
                    )
                } else {
                    "Tool call was not executed because the model response could not be used."
                        .into()
                }),
            );
            self.emit_end(&outcome).await?;
            results.push(self.add_tool_result(&outcome).await?);
        }
        Ok(results)
    }

    /// User cancellation answers missing calls; shutdown leaves durable history open.
    async fn complete_aborted_results(
        &mut self,
        calls: &[ToolCall],
        tool_results: &mut Vec<ToolResultMessage>,
    ) -> Result<(), Error> {
        let reason = self.cancel_reason().unwrap_or(CancelReason::User);
        if reason == CancelReason::User {
            for call in calls {
                if tool_results
                    .iter()
                    .any(|result| result.tool_call_id == call.id)
                {
                    continue;
                }
                let outcome = Outcome::synthetic(call, error_output("Operation aborted"));
                tool_results.push(self.add_tool_result(&outcome).await?);
            }
        }
        tool_results
            .sort_by_key(|result| calls.iter().position(|call| call.id == result.tool_call_id));
        Ok(())
    }

    async fn finish(&mut self, outcome: RunOutcome) -> Result<RunOutcome, Error> {
        self.emit(&AgentEvent::AgentEnd {
            messages: self.new_messages.clone(),
        })
        .await?;
        Ok(outcome)
    }

    /// Takes the next messages of the steering (`true`) or follow-up queue per its mode,
    /// then the host's steering messages.
    async fn drain_queue(&self, steering: bool) -> Vec<QueuedMessage> {
        let agent = self.agent;
        let mut messages = agent.take_queued(steering);
        if steering {
            messages.extend(agent.hooks.steering().await);
        }
        messages
    }

    fn model(&self) -> (Model, ThinkingLevel) {
        let state = self.agent.lock();
        (state.model.clone(), state.thinking)
    }

    /// Replaces the transcript in both the agent state and the run's working context.
    fn replace_messages(&mut self, messages: Vec<AgentMessage>) {
        self.agent.lock().messages = messages.clone();
        self.context.messages = messages;
    }

    fn cancel_reason(&self) -> Option<CancelReason> {
        self.agent.lock().run.as_ref().and_then(|run| run.reason)
    }

    async fn add_tool_result(&mut self, outcome: &Outcome) -> Result<ToolResultMessage, Error> {
        let result = ToolResultMessage {
            tool_call_id: outcome.call_id.clone(),
            tool_name: outcome.name.clone(),
            content: outcome.output.content.clone(),
            details: outcome.output.details.clone(),
            is_error: outcome.output.is_error,
            timestamp: now_ms(),
            duration_ms: outcome.duration_ms,
        };
        self.add_message(
            AgentMessage::Llm(Message::ToolResult(result.clone())),
            outcome
                .invocation
                .map_or(MessageOrigin::Run, MessageOrigin::Tool),
        )
        .await?;
        Ok(result)
    }

    /// Emits `MessageStart` and `MessageEnd` for a message the run adds.
    async fn add_message(
        &mut self,
        message: AgentMessage,
        origin: MessageOrigin,
    ) -> Result<(), Error> {
        self.emit(&AgentEvent::MessageStart {
            message: message.clone(),
        })
        .await?;
        self.end_message(message, origin).await
    }

    /// Emits `MessageEnd` and appends the supplied message. During a shutdown the
    /// message is neither persisted nor appended.
    async fn end_message(
        &mut self,
        message: AgentMessage,
        origin: MessageOrigin,
    ) -> Result<(), Error> {
        if self
            .persist(&AgentEvent::MessageEnd {
                message: message.clone(),
                origin,
            })
            .await?
        {
            self.agent.lock().messages.push(message.clone());
            self.context.messages.push(message.clone());
            self.new_messages.push(message);
        }
        Ok(())
    }

    async fn emit_start(&self, call: &ToolCall) -> Result<(), Error> {
        self.emit(&AgentEvent::ToolExecutionStart {
            call_id: call.id.clone(),
            name: call.name.clone(),
            args: call.arguments.clone(),
        })
        .await
    }

    async fn emit_end(&self, outcome: &Outcome) -> Result<(), Error> {
        self.emit(&AgentEvent::ToolExecutionEnd {
            call_id: outcome.call_id.clone(),
            name: outcome.name.clone(),
            result: outcome.output.clone(),
        })
        .await
    }

    /// Delivers one event to the persistence sink.
    async fn emit(&self, event: &AgentEvent) -> Result<(), Error> {
        self.persist(event).await?;
        Ok(())
    }

    /// Hands the event to `Persist`. Once a shutdown is requested nothing but `AgentEnd`
    /// is persisted, so the run resumes from the last state saved before it; returns
    /// whether the event was persisted.
    async fn persist(&self, event: &AgentEvent) -> Result<bool, Error> {
        if self.cancel_reason() == Some(CancelReason::Shutdown)
            && !matches!(event, AgentEvent::AgentEnd { .. })
        {
            return Ok(false);
        }
        if let Some(persist) = &self.agent.persist {
            persist.persist(event).await?;
        }
        Ok(true)
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// An error result with one text block (pi `createErrorToolResult`).
fn error_output(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: vec![UserBlock::Text { text: text.into() }],
        details: None,
        is_error: true,
        terminate: false,
    }
}

/// Validates a call's arguments against its tool's JSON Schema, with pi's error text
/// (`validateToolArguments`). pi's type coercion is not ported.
fn validate_arguments(tool: &dyn Tool, call: &ToolCall) -> Result<(), String> {
    if let Some(error) = &call.arguments_error {
        return Err(format!(
            "Invalid arguments for tool \"{}\": {error}",
            call.name
        ));
    }
    let validator = jsonschema::validator_for(tool.parameters()).map_err(|error| {
        format!(
            "Invalid parameter schema for tool \"{}\": {error}",
            call.name
        )
    })?;
    let arguments = Value::Object(call.arguments.clone());
    let errors: Vec<String> = validator
        .iter_errors(&arguments)
        .map(|error| {
            let path = error
                .instance_path()
                .as_str()
                .trim_start_matches('/')
                .replace('/', ".");
            let path = match error.kind() {
                ValidationErrorKind::Required { property } => {
                    let property = property
                        .as_str()
                        .map_or_else(|| property.to_string(), str::to_owned);
                    if path.is_empty() {
                        property
                    } else {
                        format!("{path}.{property}")
                    }
                }
                _ if path.is_empty() => "root".to_owned(),
                _ => path,
            };
            format!("  - {path}: {error}")
        })
        .collect();
    if errors.is_empty() {
        return Ok(());
    }
    let received = serde_json::to_string_pretty(&arguments).unwrap_or_default();
    Err(format!(
        "Validation failed for tool \"{}\":\n{}\n\nReceived arguments:\n{received}",
        call.name,
        errors.join("\n")
    ))
}

/// A queued input as a pending message with its origin.
fn input(queued: QueuedMessage) -> (AgentMessage, MessageOrigin) {
    (queued.message, MessageOrigin::Input(queued.id))
}

/// Declares tool-set changes to the model (pi `declareToolChanges`): the transcript's
/// system messages must replay to exactly `context.tools`. The last pending system
/// message gets the real delta; otherwise a new system message carrying it is inserted,
/// with origin `Run`, before the first non-system pending message.
fn declare_tool_changes(
    context: &AgentContext,
    mut pending: Vec<(AgentMessage, MessageOrigin)>,
) -> Vec<(AgentMessage, MessageOrigin)> {
    let system_index = pending
        .iter()
        .rposition(|(message, _)| matches!(message, AgentMessage::Llm(Message::System(_))));
    let offset = context.messages.len();
    let declared = replay_tools(
        context
            .messages
            .iter()
            .chain(pending.iter().map(|(message, _)| message))
            .enumerate()
            // The pending system message's own tool fields are intent, not state.
            .filter_map(|(index, message)| match message {
                AgentMessage::Llm(Message::System(system))
                    if system_index.map(|pending_index| pending_index + offset) != Some(index) =>
                {
                    Some(system)
                }
                _ => None,
            }),
    );
    let executable: Vec<ToolSpec> = context
        .tools
        .iter()
        .map(|tool| ToolSpec {
            name: tool.name().to_owned(),
            description: tool.description().to_owned(),
            parameters: tool.parameters().clone(),
        })
        .collect();
    let changes = tool_state_changes(&declared, &executable);

    match system_index {
        Some(index) => {
            if let (AgentMessage::Llm(Message::System(system)), _) = &mut pending[index] {
                system.tools_added = changes.tools_added;
                system.tools_removed = changes.tools_removed;
            }
        }
        None if changes.tools_added.is_empty() && changes.tools_removed.is_empty() => {}
        None => {
            let at = pending
                .iter()
                .position(|(message, _)| !matches!(message, AgentMessage::Llm(Message::System(_))))
                .unwrap_or(pending.len());
            let update = SystemMessage {
                tools_added: changes.tools_added,
                tools_removed: changes.tools_removed,
                timestamp: now_ms(),
                ..SystemMessage::default()
            };
            pending.insert(
                at,
                (
                    AgentMessage::Llm(Message::System(update)),
                    MessageOrigin::Run,
                ),
            );
        }
    }
    pending
}
