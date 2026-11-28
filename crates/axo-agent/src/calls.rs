//! Tool calls owned by a model-issued call, not by the loop's current batch.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use axo_ai::{AssistantMessage, JsonMap, ToolCall};
use futures::future::Either;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::hooks::{AgentContext, BeforeToolCallContext};
use crate::run::{
    Outcome, Routed, STOP_GRACE, after_tool_call, before_tool_call, elapsed_ms, error_output, route,
};
use crate::tool::{
    ExecutionMode, SteerSignal, Tool, ToolContext, ToolOutput, ToolUpdates, execute_call,
};
use crate::{Agent, AgentEvent, Error};

/// A model-issued call's handle for running nested tools. It can outlive that call.
#[derive(Clone)]
pub struct Calls(Option<Arc<Family>>);

struct Family {
    agent: Weak<Agent>,
    root_call_id: Arc<str>,
    assistant: Arc<AssistantMessage>,
    /// Hook context sampled at the first call; routing still uses the current tools.
    context: OnceLock<Arc<AgentContext>>,
    cancel: CancellationToken,
    /// Sequential tools run exclusively; parallel ones share the gate.
    gate: RwLock<()>,
    next: AtomicU32,
}

impl Calls {
    pub(crate) fn new(
        agent: &Agent,
        root_call_id: Arc<str>,
        assistant: Arc<AssistantMessage>,
    ) -> Self {
        Self(Some(Arc::new(Family {
            agent: agent.this.clone(),
            root_call_id,
            assistant,
            context: OnceLock::new(),
            cancel: agent.nested_stop.child_token(),
            gate: RwLock::new(()),
            next: AtomicU32::new(1),
        })))
    }

    /// An unavailable handle, for tool invocations outside an agent.
    pub fn none() -> Self {
        Self(None)
    }

    /// Whether this call was given a nested-call runner.
    pub fn available(&self) -> bool {
        self.0.is_some()
    }

    /// Direct tools, then deferred tools not already listed; only nestable tools.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let Some(agent) = self.0.as_ref().and_then(|family| family.agent.upgrade()) else {
            return Vec::new();
        };
        let tools = {
            let state = agent.lock();
            state
                .tools
                .iter()
                .chain(&state.deferred)
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut seen = HashSet::new();
        tools
            .into_iter()
            .filter(|tool| seen.insert(tool.name().to_owned()) && tool.nestable())
            .collect()
    }

    /// Starts a nested call on its own task. Dropping the returned future does not stop
    /// the call; use [`Calls::cancel`].
    pub fn call(
        &self,
        name: &str,
        arguments: JsonMap,
    ) -> impl Future<Output = ToolOutput> + Send + 'static {
        let task = self.0.as_ref().map(|family| {
            let family = family.clone();
            let name = name.to_owned();
            tokio::spawn(async move { family.run(name, arguments).await })
        });
        async move {
            match task {
                Some(task) => match task.await {
                    Ok(output) => output,
                    Err(error) if !error.is_cancelled() => {
                        error_output(format!("Nested call runner crashed: {error}"))
                    }
                    Err(_) => error_output("Operation aborted"),
                },
                None => error_output("Operation aborted"),
            }
        }
    }

    /// Cancels the whole family, including calls waiting on the execution gate.
    /// Running tools get a grace period before their execution futures are dropped.
    pub fn cancel(&self) {
        if let Some(family) = &self.0 {
            family.cancel.cancel();
        }
    }
}

impl Family {
    async fn run(&self, name: String, arguments: JsonMap) -> ToolOutput {
        let Some(agent) = self.agent.upgrade() else {
            return error_output("Operation aborted");
        };
        let (context, tools, deferred) = {
            let state = agent.lock();
            if state.shutting_down() || self.cancel.is_cancelled() {
                return error_output("Operation aborted");
            }
            let context = self.context.get_or_init(|| Arc::new(state.context()));
            (context, state.tools.clone(), state.deferred.clone())
        };
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let call = ToolCall {
            id: format!("{}/{n}", self.root_call_id),
            name,
            arguments,
            arguments_error: None,
            thought_signature: None,
        };
        let routed = route(&tools, &deferred, &call);
        let effective = routed.as_ref().map_or(&call, |routed| routed.call.as_ref());
        let start = agent
            .emit(&AgentEvent::ToolExecutionStart {
                call_id: effective.id.clone(),
                name: effective.name.clone(),
                args: effective.arguments.clone(),
                parent_call_id: Some(self.root_call_id.to_string()),
            })
            .await;
        match start {
            Ok(true) => {}
            Ok(false) => return error_output("Operation aborted"),
            Err(error) => return self.recording_failed(error),
        }
        let hook_context = BeforeToolCallContext {
            assistant_message: &self.assistant,
            tool_call: effective,
            context,
            parent: Some(&self.root_call_id),
        };
        let output = match routed.as_ref() {
            Ok(routed) => match self.prepare(&agent, &call, routed, hook_context).await {
                Some(output) => output,
                None => {
                    return self
                        .execute(&agent, routed.tool.clone(), hook_context)
                        .await;
                }
            },
            Err(text) => error_output(text.as_str()),
        };
        self.finish(&agent, Outcome::synthetic(effective, output))
            .await
    }

    async fn prepare(
        &self,
        agent: &Agent,
        call: &ToolCall,
        routed: &Routed<'_>,
        context: BeforeToolCallContext<'_>,
    ) -> Option<ToolOutput> {
        if self.cancel.is_cancelled() {
            return Some(error_output("Operation aborted"));
        }
        if !routed.tool.nestable() {
            return Some(error_output(format!(
                "{} cannot be called by another tool",
                routed.call.name
            )));
        }
        if let Err(text) = routed.validate(call) {
            return Some(error_output(text));
        }
        before_tool_call(&agent.tool_hooks(), context, &self.cancel).await
    }

    async fn execute(
        &self,
        agent: &Agent,
        tool: Arc<dyn Tool>,
        hook_context: BeforeToolCallContext<'_>,
    ) -> ToolOutput {
        let call = hook_context.tool_call;
        let _gate = tokio::select! {
            biased;
            () = self.cancel.cancelled() => {
                return self.finish(agent, Outcome::synthetic(call, error_output("Operation aborted"))).await;
            }
            guard = async {
                match tool.execution_mode() {
                    ExecutionMode::Sequential => Either::Left(self.gate.write().await),
                    ExecutionMode::Parallel => Either::Right(self.gate.read().await),
                }
            } => guard,
        };
        let invocation = agent.nested_invocation();
        let (sender, mut updates) = mpsc::unbounded_channel();
        let cx = ToolContext {
            call: Arc::new(call.clone()),
            cancel: self.cancel.child_token(),
            updates: ToolUpdates::new(move |update| {
                let _ = sender.send(update);
            }),
            steer: SteerSignal::never(),
            invocation: invocation.id,
            parent: Some(self.root_call_id.clone()),
            calls: Calls::none(),
        };
        let started = Instant::now();
        let cancel = self.cancel.clone();
        let mut task = tokio::spawn(async move {
            let hard_stop = CancellationToken::new();
            let execute = execute_call(&*tool, cx, &hard_stop);
            tokio::pin!(execute);
            let (result, executed) = tokio::select! {
                result = &mut execute => result,
                () = async {
                    cancel.cancelled().await;
                    tokio::time::sleep(STOP_GRACE).await;
                } => {
                    hard_stop.cancel();
                    execute.await
                }
            };
            (result, elapsed_ms(started), executed)
        });
        let mut joined = None;
        let mut failure = None;
        loop {
            let update = tokio::select! {
                result = &mut task, if joined.is_none() => {
                    joined = Some(result);
                    updates.close();
                    continue;
                }
                update = updates.recv() => update,
            };
            let Some(update) = update else { break };
            if failure.is_none()
                && let Err(error) = agent
                    .emit(&AgentEvent::ToolExecutionUpdate {
                        call_id: call.id.clone(),
                        update,
                        parent_call_id: Some(self.root_call_id.to_string()),
                    })
                    .await
            {
                failure = Some(self.recording_failed(error));
            }
        }
        let joined = match joined {
            Some(result) => result,
            None => task.await,
        };
        let (mut outcome, executed) = Outcome::joined(call, invocation.id, started, joined);
        if executed {
            after_tool_call(
                &agent.tool_hooks(),
                hook_context.assistant_message,
                call,
                hook_context.context,
                hook_context.parent,
                &mut outcome.output,
                &self.cancel,
            )
            .await;
        }
        let output = self.finish(agent, outcome).await;
        failure.unwrap_or(output)
    }

    async fn finish(&self, agent: &Agent, outcome: Outcome) -> ToolOutput {
        match agent
            .emit(&outcome.end_event(Some(self.root_call_id.to_string())))
            .await
        {
            Ok(_) => outcome.output,
            Err(error) => self.recording_failed(error),
        }
    }

    fn recording_failed(&self, error: Error) -> ToolOutput {
        tracing::warn!(parent_call_id = %self.root_call_id, %error, "could not persist nested tool call");
        self.cancel.cancel();
        error_output(format!("The call could not be recorded: {error}"))
    }
}
