//! The loop itself (pi `agent/src/agent-loop.ts`).

use std::sync::Arc;
use std::time::Instant;

use axo_ai::transcript::{current_tools, tool_state_changes};
use axo_ai::{
    Accumulator, AssistantEvent, AssistantMessage, Context, Message, Model, SimpleStreamOptions,
    StopReason, SystemMessage, ThinkingLevel, ToolCall, ToolResultMessage, ToolSpec, Transcript,
    UserBlock, now_ms,
};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use jsonschema::error::ValidationErrorKind;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, QueueMode, RunOutcome};
use crate::cancel::CancelReason;
use crate::error::Error;
use crate::event::AgentEvent;
use crate::hooks::{
    AfterToolCallContext, AgentContext, BeforeToolCallContext, PrepareRequestContext,
    ToolCallDecision, TurnContext, TurnDecision,
};
use crate::message::AgentMessage;
use crate::tool::{ExecutionMode, Tool, ToolOutput, ToolUpdate};

/// How a run starts.
pub(crate) enum RunStart {
    /// `Agent::prompt`: append and emit these messages, then request a turn.
    Prompt(Vec<AgentMessage>),
    /// `Agent::continue_run` on a non-empty transcript whose last message is a user or
    /// tool-result message, or an assistant message with unanswered tool calls (those
    /// calls run first).
    Continue,
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
    match run.run(start).await {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            // Persistence failed, so the closing event goes to subscribers only.
            let _ = agent.events.send(AgentEvent::AgentEnd {
                messages: run.new_messages,
            });
            Err(error)
        }
    }
}

/// One finished tool call, before it becomes a tool-result message.
struct Outcome {
    call_id: String,
    name: String,
    output: ToolOutput,
    duration_ms: Option<u64>,
}

/// What preparing a call yields (pi `prepareToolCall`).
enum Prepared {
    /// Not executed: unknown tool, invalid arguments, blocked, or aborted.
    Immediate(ToolOutput),
    Ready(Arc<dyn Tool>),
}

/// The state of one run (pi `runLoop`'s locals).
struct Run<'a> {
    agent: &'a Agent,
    cancel: CancellationToken,
    /// The loop's working context; hooks may replace it. Its tools are refreshed from the
    /// agent before each turn's request, so `set_tools` applies from the next turn.
    context: AgentContext,
    /// Messages this run added, for `AgentEnd` and the turn hooks.
    new_messages: Vec<AgentMessage>,
}

impl Run<'_> {
    async fn run(&mut self, start: RunStart) -> Result<RunOutcome, Error> {
        let agent = self.agent;
        self.emit(AgentEvent::AgentStart).await?;
        self.emit(AgentEvent::TurnStart).await?;

        let mut resume = None;
        let mut pending = Vec::new();
        match start {
            RunStart::Prompt(messages) => {
                for message in declare_tool_changes(&self.context, messages) {
                    self.add_message(message).await?;
                }
                pending = self.drain_queue(true).await;
            }
            RunStart::Continue => {
                resume = unanswered_calls(&self.context.messages);
                // Steering polled now would be overwritten by the poll after the resumed
                // calls finish, so only poll when there is nothing to resume.
                if resume.is_none() {
                    pending = self.drain_queue(true).await;
                }
            }
        }

        let mut last_turn: Option<(AssistantMessage, Vec<ToolResultMessage>)> = None;
        let mut explicit_continuation = false;
        loop {
            let mut more_tool_calls = true;
            while more_tool_calls || !pending.is_empty() {
                let (message, calls) = match resume.take() {
                    Some(resumed) => resumed,
                    None => {
                        let mut prepared = Vec::new();
                        if let Some((message, tool_results)) = &last_turn {
                            let update = agent
                                .hooks
                                .prepare_next_turn(
                                    TurnContext {
                                        message,
                                        tool_results,
                                        context: &self.context,
                                        new_messages: &self.new_messages,
                                    },
                                    &self.cancel,
                                )
                                .await;
                            if let Some(update) = update {
                                prepared = update.messages;
                                self.apply_update(update.context, update.model, update.thinking);
                            }
                            // Preparation can be long (e.g. compaction): pick up steering
                            // queued meanwhile, unless one-at-a-time already took one.
                            if pending.is_empty() {
                                pending = self.drain_queue(true).await;
                            }
                            self.emit(AgentEvent::TurnStart).await?;
                        }

                        self.context.tools = agent.lock().tools.clone();
                        prepared.append(&mut pending);
                        for message in declare_tool_changes(&self.context, prepared) {
                            self.add_message(message).await?;
                        }

                        let (model, thinking) = self.model();
                        let update = agent
                            .hooks
                            .prepare_request(
                                PrepareRequestContext {
                                    context: &self.context,
                                    model: &model,
                                    thinking,
                                },
                                &self.cancel,
                            )
                            .await;
                        if let Some(update) = update {
                            self.apply_update(update.context, update.model, update.thinking);
                        }

                        let message = self.stream_turn().await?;
                        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                            return self.end_failed_turn(message).await;
                        }
                        let calls = message.tool_calls().cloned().collect();
                        (message, calls)
                    }
                };

                let mut tool_results = Vec::new();
                more_tool_calls = false;
                if !calls.is_empty() {
                    // A `Length` stop may have cut any call's arguments short: run none.
                    let (results, terminate) = if message.stop_reason == StopReason::Length {
                        (self.fail_truncated_calls(&calls).await?, false)
                    } else {
                        self.execute_tool_calls(&message, &calls).await?
                    };
                    tool_results = results;
                    more_tool_calls = !terminate;
                }

                if self.cancel.is_cancelled() {
                    return self.end_aborted_turn(message, &calls, tool_results).await;
                }

                let decision = agent
                    .hooks
                    .finish_turn(
                        TurnContext {
                            message: &message,
                            tool_results: &tool_results,
                            context: &self.context,
                            new_messages: &self.new_messages,
                        },
                        &self.cancel,
                    )
                    .await;
                self.emit(AgentEvent::TurnEnd {
                    message: AgentMessage::Llm(Message::Assistant(message.clone())),
                    tool_results: tool_results.clone(),
                })
                .await?;
                if decision == Some(TurnDecision::End) {
                    return self.finish(RunOutcome::Done).await;
                }

                explicit_continuation = decision == Some(TurnDecision::Continue);
                pending = self.drain_queue(true).await;
                if more_tool_calls || !pending.is_empty() {
                    explicit_continuation = false;
                }
                last_turn = Some((message, tool_results));
            }

            // The run would stop here: follow-ups keep it going.
            let follow_ups = self.drain_queue(false).await;
            if !follow_ups.is_empty() {
                explicit_continuation = false;
                pending = follow_ups;
                continue;
            }
            // `finish_turn` asked for one more request and nothing else provided one.
            if explicit_continuation {
                explicit_continuation = false;
                continue;
            }
            return self.finish(RunOutcome::Done).await;
        }
    }

    /// Streams one assistant response (pi `streamAssistantResponse`) and adds it to the
    /// transcript, unless the run is shutting down.
    async fn stream_turn(&mut self) -> Result<AssistantMessage, Error> {
        let agent = self.agent;
        let messages = agent
            .hooks
            .transform_context(self.context.messages.clone(), &self.cancel)
            .await;
        let mut messages = agent.hooks.convert_to_llm(&messages);
        // Failed and aborted turns stay in the transcript but are never sent (providers'
        // `transform_messages` applies the same rule; doing it here keeps every request,
        // whatever the `Streamer`, free of them).
        messages.retain(|message| {
            !matches!(
                message,
                Message::Assistant(assistant)
                    if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted)
            )
        });
        let transcript = Transcript::normalize(Context {
            system_prompt: None,
            messages,
            tools: Vec::new(),
        });
        let (model, thinking) = self.model();
        let options = SimpleStreamOptions {
            thinking,
            session_id: agent.session_id.clone(),
            ..SimpleStreamOptions::default()
        };
        let mut stream =
            agent
                .streamer
                .stream(&model, &transcript, options, self.cancel.child_token());

        agent.lock().streaming = Some(Accumulator::new());
        let mut started = false;
        let mut ended = false;
        while let Some(event) = stream.next().await {
            if let Some(accumulator) = agent.lock().streaming.as_mut() {
                accumulator.apply(&event);
            }
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
                    self.emit(AgentEvent::MessageStart { message }).await?;
                }
            }
            let terminal = event.is_terminal();
            self.emit(AgentEvent::MessageUpdate { event }).await?;
            if terminal {
                ended = true;
                break;
            }
        }
        drop(stream);

        let accumulated = agent
            .lock()
            .streaming
            .take()
            .and_then(Accumulator::into_message);
        let mut message = match accumulated {
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
            self.emit(AgentEvent::MessageStart { message: start })
                .await?;
        }
        let adjusted = self
            .end_message(AgentMessage::Llm(Message::Assistant(message.clone())))
            .await?;
        Ok(match adjusted {
            Some(AgentMessage::Llm(Message::Assistant(adjusted))) => adjusted,
            _ => message,
        })
    }

    /// Runs the calls of one assistant message (pi `executeToolCalls`). Returns their
    /// result messages in call order and whether the batch asked to terminate.
    async fn execute_tool_calls(
        &mut self,
        message: &AssistantMessage,
        calls: &[ToolCall],
    ) -> Result<(Vec<ToolResultMessage>, bool), Error> {
        let sequential = self.agent.tool_execution == ExecutionMode::Sequential
            || calls.iter().any(|call| {
                self.context.tools.iter().any(|tool| {
                    tool.name() == call.name && tool.execution_mode() == ExecutionMode::Sequential
                })
            });
        // The batch terminates only when every produced result asks for it.
        let mut terminates: Vec<bool> = Vec::with_capacity(calls.len());
        let mut results = Vec::with_capacity(calls.len());

        if sequential {
            for call in calls {
                self.emit_start(call).await?;
                let outcome = match self.prepare(message, call).await {
                    Prepared::Immediate(output) => {
                        let outcome = Outcome {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            output,
                            duration_ms: None,
                        };
                        self.emit_end(&outcome).await?;
                        outcome
                    }
                    Prepared::Ready(tool) => {
                        let (updates, receiver) = mpsc::unbounded_channel();
                        let running = FuturesUnordered::from_iter([
                            self.execute_call(0, tool, message, call, updates)
                        ]);
                        let Some((_, outcome)) = self.drive(running, receiver).await?.pop() else {
                            continue;
                        };
                        outcome
                    }
                };
                terminates.push(outcome.output.terminate);
                results.extend(self.add_tool_result(&outcome).await?);
                if self.cancel.is_cancelled() {
                    break;
                }
            }
        } else {
            let mut slots: Vec<Option<Outcome>> = calls.iter().map(|_| None).collect();
            let (updates, receiver) = mpsc::unbounded_channel();
            let running = FuturesUnordered::new();
            // Preparation, including `before_tool_call`, runs in source order.
            for (index, call) in calls.iter().enumerate() {
                self.emit_start(call).await?;
                match self.prepare(message, call).await {
                    Prepared::Immediate(output) => {
                        let outcome = Outcome {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            output,
                            duration_ms: None,
                        };
                        self.emit_end(&outcome).await?;
                        slots[index] = Some(outcome);
                    }
                    Prepared::Ready(tool) => {
                        running.push(self.execute_call(
                            index,
                            tool,
                            message,
                            call,
                            updates.clone(),
                        ));
                    }
                }
                if self.cancel.is_cancelled() {
                    break;
                }
            }
            drop(updates);
            for (index, outcome) in self.drive(running, receiver).await? {
                slots[index] = Some(outcome);
            }
            // Result messages follow the order the model issued the calls.
            for outcome in slots.into_iter().flatten() {
                terminates.push(outcome.output.terminate);
                results.extend(self.add_tool_result(&outcome).await?);
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
        let decision = self
            .agent
            .hooks
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
            Err(error) => Prepared::Immediate(error_output(error.0)),
            Ok(_) if self.cancel.is_cancelled() => {
                Prepared::Immediate(error_output("Operation aborted"))
            }
            Ok(ToolCallDecision::Block { reason, terminate }) => {
                let mut output =
                    error_output(reason.unwrap_or_else(|| "Tool execution was blocked".into()));
                output.terminate = terminate;
                Prepared::Immediate(output)
            }
            Ok(ToolCallDecision::Allow) => Prepared::Ready(tool.clone()),
        }
    }

    /// Executes one prepared call and applies `after_tool_call`.
    async fn execute_call(
        &self,
        index: usize,
        tool: Arc<dyn Tool>,
        message: &AssistantMessage,
        call: &ToolCall,
        updates: mpsc::UnboundedSender<(String, ToolUpdate)>,
    ) -> (usize, Outcome) {
        let mut outcome = Outcome {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: error_output("Operation aborted"),
            duration_ms: None,
        };
        if self.cancel.is_cancelled() {
            return (index, outcome);
        }
        let call_id = call.id.clone();
        let on_update = move |update: ToolUpdate| {
            let _ = updates.send((call_id.clone(), update));
        };
        let started = Instant::now();
        let result = tool
            .execute(call, self.cancel.child_token(), &on_update)
            .await;
        outcome.duration_ms =
            Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        outcome.output = result.unwrap_or_else(|error| error_output(error.0));

        let overrides = self
            .agent
            .hooks
            .after_tool_call(
                AfterToolCallContext {
                    assistant_message: message,
                    tool_call: call,
                    result: &outcome.output,
                    context: &self.context,
                },
                &self.cancel,
            )
            .await;
        match overrides {
            Ok(Some(overrides)) => {
                let output = &mut outcome.output;
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
        (index, outcome)
    }

    /// Polls running calls, forwarding their updates, and emits each `ToolExecutionEnd`
    /// as its call completes. Returns the outcomes in completion order.
    async fn drive<F>(
        &self,
        mut running: FuturesUnordered<F>,
        mut updates: mpsc::UnboundedReceiver<(String, ToolUpdate)>,
    ) -> Result<Vec<(usize, Outcome)>, Error>
    where
        F: Future<Output = (usize, Outcome)>,
    {
        let mut finished = Vec::with_capacity(running.len());
        loop {
            tokio::select! {
                biased;
                Some((call_id, update)) = updates.recv() => {
                    self.emit(AgentEvent::ToolExecutionUpdate { call_id, update }).await?;
                }
                next = running.next() => {
                    let Some((index, outcome)) = next else { break };
                    // Updates sent before the call returned precede its end.
                    while let Ok((call_id, update)) = updates.try_recv() {
                        self.emit(AgentEvent::ToolExecutionUpdate { call_id, update }).await?;
                    }
                    self.emit_end(&outcome).await?;
                    finished.push((index, outcome));
                }
            }
        }
        Ok(finished)
    }

    /// Fails every call of a response cut off by the output-token limit.
    async fn fail_truncated_calls(
        &mut self,
        calls: &[ToolCall],
    ) -> Result<Vec<ToolResultMessage>, Error> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            self.emit_start(call).await?;
            let outcome = Outcome {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output: error_output(format!(
                    "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                    call.name
                )),
                duration_ms: None,
            };
            self.emit_end(&outcome).await?;
            results.extend(self.add_tool_result(&outcome).await?);
        }
        Ok(results)
    }

    /// Ends the run after a response that failed or was aborted (no tool execution).
    async fn end_failed_turn(&mut self, message: AssistantMessage) -> Result<RunOutcome, Error> {
        if message.stop_reason == StopReason::Aborted {
            // The aborted message is never sent, so its calls need no results: one would
            // answer a call the model never sees.
            return self.end_aborted_turn(message, &[], Vec::new()).await;
        }
        // The decision is ignored: a failed turn always ends the run.
        self.agent
            .hooks
            .finish_turn(
                TurnContext {
                    message: &message,
                    tool_results: &[],
                    context: &self.context,
                    new_messages: &self.new_messages,
                },
                &self.cancel,
            )
            .await;
        let error = message.error_message.clone().unwrap_or_default();
        self.emit(AgentEvent::TurnEnd {
            message: AgentMessage::Llm(Message::Assistant(message)),
            tool_results: Vec::new(),
        })
        .await?;
        self.finish(RunOutcome::Failed(error)).await
    }

    /// Ends a cancelled run. A user cancel answers every call of `calls` left without a
    /// result with `"Operation aborted"`; a shutdown records nothing (see `persist`).
    async fn end_aborted_turn(
        &mut self,
        message: AssistantMessage,
        calls: &[ToolCall],
        mut tool_results: Vec<ToolResultMessage>,
    ) -> Result<RunOutcome, Error> {
        let reason = self.cancel_reason().unwrap_or(CancelReason::User);
        if reason == CancelReason::User {
            for call in calls {
                if tool_results
                    .iter()
                    .any(|result| result.tool_call_id == call.id)
                {
                    continue;
                }
                let outcome = Outcome {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    output: error_output("Operation aborted"),
                    duration_ms: None,
                };
                tool_results.extend(self.add_tool_result(&outcome).await?);
            }
            self.agent
                .hooks
                .finish_turn(
                    TurnContext {
                        message: &message,
                        tool_results: &tool_results,
                        context: &self.context,
                        new_messages: &self.new_messages,
                    },
                    &self.cancel,
                )
                .await;
        }
        self.emit(AgentEvent::TurnEnd {
            message: AgentMessage::Llm(Message::Assistant(message)),
            tool_results,
        })
        .await?;
        self.finish(RunOutcome::Aborted(reason)).await
    }

    async fn finish(&mut self, outcome: RunOutcome) -> Result<RunOutcome, Error> {
        self.emit(AgentEvent::AgentEnd {
            messages: self.new_messages.clone(),
        })
        .await?;
        Ok(outcome)
    }

    /// Takes the next messages of the steering (`true`) or follow-up queue per its mode,
    /// then the host's messages from the matching hook.
    async fn drain_queue(&self, steering: bool) -> Vec<AgentMessage> {
        let agent = self.agent;
        let mut messages: Vec<AgentMessage> = {
            let mut state = agent.lock();
            let (queue, mode) = if steering {
                (&mut state.steering, agent.steering_mode)
            } else {
                (&mut state.follow_up, agent.follow_up_mode)
            };
            match mode {
                QueueMode::All => queue.drain(..).collect(),
                QueueMode::OneAtATime => queue.pop_front().into_iter().collect(),
            }
        };
        messages.extend(if steering {
            agent.hooks.steering().await
        } else {
            agent.hooks.follow_up().await
        });
        messages
    }

    fn model(&self) -> (Model, ThinkingLevel) {
        let state = self.agent.lock();
        (state.model.clone(), state.thinking)
    }

    /// Applies a hook's replacements. Model, thinking and tools live in the agent state,
    /// so they hold for later requests (and `set_*` calls keep working).
    fn apply_update(
        &mut self,
        context: Option<AgentContext>,
        model: Option<Model>,
        thinking: Option<ThinkingLevel>,
    ) {
        let agent = self.agent;
        let mut state = agent.lock();
        if let Some(context) = context {
            state.tools = context.tools.clone();
            self.context = context;
        }
        if let Some(model) = model {
            state.model = model;
        }
        if let Some(thinking) = thinking {
            state.thinking = thinking;
        }
    }

    fn cancel_reason(&self) -> Option<CancelReason> {
        self.agent.lock().run.as_ref().and_then(|run| run.reason)
    }

    async fn add_tool_result(
        &mut self,
        outcome: &Outcome,
    ) -> Result<Option<ToolResultMessage>, Error> {
        let result = ToolResultMessage {
            tool_call_id: outcome.call_id.clone(),
            tool_name: outcome.name.clone(),
            content: outcome.output.content.clone(),
            details: outcome.output.details.clone(),
            is_error: outcome.output.is_error,
            timestamp: now_ms(),
            duration_ms: outcome.duration_ms,
        };
        Ok(
            match self
                .add_message(AgentMessage::Llm(Message::ToolResult(result)))
                .await?
            {
                Some(AgentMessage::Llm(Message::ToolResult(result))) => Some(result),
                _ => None,
            },
        )
    }

    /// Emits `MessageStart` and `MessageEnd` for a message the run adds.
    async fn add_message(&mut self, message: AgentMessage) -> Result<Option<AgentMessage>, Error> {
        self.emit(AgentEvent::MessageStart {
            message: message.clone(),
        })
        .await?;
        self.end_message(message).await
    }

    /// Emits `MessageEnd` and appends the message as persistence left it. During a
    /// shutdown the message is neither persisted nor appended; it is still returned.
    async fn end_message(&mut self, message: AgentMessage) -> Result<Option<AgentMessage>, Error> {
        let mut event = AgentEvent::MessageEnd { message };
        let persisted = self.persist(&mut event).await?;
        let message = match &event {
            AgentEvent::MessageEnd { message } => Some(message.clone()),
            _ => None,
        };
        {
            let mut state = self.agent.lock();
            state.streaming = None;
            if let (true, Some(message)) = (persisted, &message) {
                state.messages.push(message.clone());
            }
        }
        if let (true, Some(message)) = (persisted, &message) {
            self.context.messages.push(message.clone());
            self.new_messages.push(message.clone());
        }
        let _ = self.agent.events.send(event);
        Ok(message)
    }

    async fn emit_start(&self, call: &ToolCall) -> Result<(), Error> {
        self.emit(AgentEvent::ToolExecutionStart {
            call_id: call.id.clone(),
            name: call.name.clone(),
            args: call.arguments.clone(),
        })
        .await
    }

    async fn emit_end(&self, outcome: &Outcome) -> Result<(), Error> {
        self.emit(AgentEvent::ToolExecutionEnd {
            call_id: outcome.call_id.clone(),
            name: outcome.name.clone(),
            result: outcome.output.clone(),
            is_error: outcome.output.is_error,
        })
        .await
    }

    /// Delivers one event: persistence first (awaited, may mutate it), then the state
    /// reduction, then the broadcast.
    async fn emit(&self, mut event: AgentEvent) -> Result<(), Error> {
        self.persist(&mut event).await?;
        match &event {
            AgentEvent::ToolExecutionStart {
                call_id,
                name,
                args,
            } => self.agent.lock().pending_tool_calls.push(ToolCall {
                id: call_id.clone(),
                name: name.clone(),
                arguments: args.clone(),
                arguments_error: None,
            }),
            AgentEvent::ToolExecutionEnd { call_id, .. } => self
                .agent
                .lock()
                .pending_tool_calls
                .retain(|call| &call.id != call_id),
            _ => {}
        }
        let _ = self.agent.events.send(event);
        Ok(())
    }

    /// Hands the event to `Persist`. Once a shutdown is requested nothing but `AgentEnd`
    /// is persisted, so the run resumes from the last state saved before it; returns
    /// whether the event was persisted.
    async fn persist(&self, event: &mut AgentEvent) -> Result<bool, Error> {
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

/// Declares tool-set changes to the model (pi `declareToolChanges`): the transcript's
/// system messages must replay to exactly `context.tools`. The last pending system
/// message gets the real delta; otherwise a new system message carrying it is inserted
/// before the first non-system pending message.
fn declare_tool_changes(
    context: &AgentContext,
    mut pending: Vec<AgentMessage>,
) -> Vec<AgentMessage> {
    let system_index = pending
        .iter()
        .rposition(|message| matches!(message, AgentMessage::Llm(Message::System(_))));
    let offset = context.messages.len();
    let declared: Vec<Message> = context
        .messages
        .iter()
        .chain(&pending)
        .enumerate()
        .filter_map(|(index, message)| match message {
            AgentMessage::Llm(Message::System(system)) => {
                let mut system = system.clone();
                // The pending system message's own tool fields are intent, not state.
                if system_index.map(|pending_index| pending_index + offset) == Some(index) {
                    system.tools_added.clear();
                    system.tools_removed.clear();
                }
                Some(Message::System(system))
            }
            _ => None,
        })
        .collect();
    let executable: Vec<ToolSpec> = context
        .tools
        .iter()
        .map(|tool| ToolSpec {
            name: tool.name().to_owned(),
            description: tool.description().to_owned(),
            parameters: tool.parameters().clone(),
        })
        .collect();
    let changes = tool_state_changes(&current_tools(&declared), &executable);

    match system_index {
        Some(index) => {
            if let AgentMessage::Llm(Message::System(system)) = &mut pending[index] {
                system.tools_added = changes.tools_added;
                system.tools_removed = changes.tools_removed;
            }
        }
        None if changes.tools_added.is_empty() && changes.tools_removed.is_empty() => {}
        None => {
            let at = pending
                .iter()
                .position(|message| !matches!(message, AgentMessage::Llm(Message::System(_))))
                .unwrap_or(pending.len());
            let update = SystemMessage {
                content: Vec::new(),
                sections: None,
                tools_added: changes.tools_added,
                tools_removed: changes.tools_removed,
                timestamp: now_ms(),
            };
            pending.insert(at, AgentMessage::Llm(Message::System(update)));
        }
    }
    pending
}

/// The last assistant message's calls that no later tool result answers, when only tool
/// results (or system messages) follow it. Failed and aborted responses are never
/// resumed: continuing after them requests a new turn.
fn unanswered_calls(messages: &[AgentMessage]) -> Option<(AssistantMessage, Vec<ToolCall>)> {
    let mut answered = Vec::new();
    for message in messages.iter().rev() {
        match message {
            AgentMessage::Llm(Message::ToolResult(result)) => answered.push(&result.tool_call_id),
            AgentMessage::Llm(Message::System(_)) => {}
            AgentMessage::Llm(Message::Assistant(assistant))
                if !matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) =>
            {
                let calls: Vec<ToolCall> = assistant
                    .tool_calls()
                    .filter(|call| !answered.contains(&&call.id))
                    .cloned()
                    .collect();
                return (!calls.is_empty()).then(|| (assistant.clone(), calls));
            }
            _ => return None,
        }
    }
    None
}
