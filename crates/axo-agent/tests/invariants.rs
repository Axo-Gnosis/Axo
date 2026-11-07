//! Ordering and lifecycle invariants of the agent loop (`plan/design.md` "Agent loop"),
//! exercised through the public `Agent` API with a scripted provider.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{
    Agent, AgentConfig, AgentContext, AgentEvent, AgentMessage, BeforeToolCallContext,
    CancelReason, ExecutionMode, HookError, LoopHooks, MessageOrigin, Persist, PersistError,
    PrepareRequestContext, QueuedMessage, RequestUpdate, RunOutcome, Tool, ToolCallDecision,
    ToolContext, ToolError, ToolInvocationId, ToolOutput, TurnContext, TurnDecision, TurnFinish,
};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{
    ApiKind, AssistantBlock, AssistantEvent, AssistantMessage, Compat, Cost, InputKind, Message,
    Model, ScriptedApi, ScriptedTurn, StopReason, ToolResultMessage, UserBlock, UserMessage,
    now_ms,
};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(10);

fn model() -> Model {
    Model {
        id: "scripted-model".into(),
        name: "Scripted".into(),
        api: ApiKind::AnthropicMessages,
        provider: "scripted".into(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: Cost::default(),
        context_window: 100_000,
        max_tokens: 4096,
        headers: BTreeMap::new(),
        compat: Compat::default(),
    }
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::Llm(Message::User(UserMessage {
        content: vec![UserBlock::Text { text: text.into() }],
        timestamp: now_ms(),
    }))
}

fn first_text(content: &[UserBlock]) -> String {
    content
        .iter()
        .find_map(|block| match block {
            UserBlock::Text { text } => Some(text.clone()),
            UserBlock::Image { .. } => None,
        })
        .unwrap_or_default()
}

/// A short label for a message; `None` for system messages (tool declarations), which the
/// loop inserts on its own and these tests ignore.
fn describe(message: &Message) -> Option<String> {
    match message {
        Message::System(_) => None,
        Message::User(user) => Some(format!("user:{}", first_text(&user.content))),
        Message::Assistant(assistant) => Some(format!("assistant:{:?}", assistant.stop_reason)),
        Message::ToolResult(result) => Some(format!("result:{}", result.tool_call_id)),
    }
}

/// The non-system messages of one request, as labels.
fn convo(messages: &[Message]) -> Vec<String> {
    messages.iter().filter_map(describe).collect()
}

fn labels(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Every tool call of an assistant message is immediately followed by its result, in call
/// order, with nothing in between.
fn assert_results_adjacent(messages: &[Message]) {
    for (index, message) in messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message {
            for (offset, call) in assistant.tool_calls().enumerate() {
                match messages.get(index + 1 + offset) {
                    Some(Message::ToolResult(result)) if result.tool_call_id == call.id => {}
                    other => panic!(
                        "call {} is not followed by its result; found {:?}",
                        call.id,
                        other.and_then(describe)
                    ),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Recording hooks + persistence
// ---------------------------------------------------------------------------------------

/// Records hook calls and persisted events into one ordered log. Each entry carries the
/// number of provider requests made when it was recorded.
struct Recorder {
    api: Arc<ScriptedApi>,
    log: Mutex<Vec<(String, usize)>>,
    results: Mutex<Vec<ToolResultMessage>>,
    assistants: Mutex<Vec<AssistantMessage>>,
    exec_ends: Mutex<Vec<(String, bool)>>,
    /// The `MessageEnd` label and origin of every persisted message, in order.
    origins: Mutex<Vec<(String, MessageOrigin)>>,
    /// Call ids whose `before_tool_call` fails.
    block: Vec<String>,
    /// Per-call-id delay inside `before_tool_call`.
    prep_delay_ms: Vec<(String, u64)>,
    /// Delay inside `persist` for `MessageEnd`, to expose a loop that doesn't await it.
    slow_persist: bool,
    recover: bool,
    skip_tools: bool,
    wait_prepare: bool,
    wait_finish: bool,
    hook_failure: bool,
    prepare_failure: bool,
    replace_final: bool,
    switch_model: bool,
    agent: OnceLock<Weak<Agent>>,
    entered: Notify,
    finished_results: Mutex<Vec<Vec<String>>>,
    finished_models: Mutex<Vec<String>>,
}

impl Recorder {
    fn push(&self, name: String) {
        let requests = self.api.requests().len();
        self.log.lock().unwrap().push((name, requests));
    }

    fn names(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn position(&self, name: &str) -> usize {
        self.names()
            .iter()
            .position(|entry| entry == name)
            .unwrap_or_else(|| panic!("{name} not in log {:?}", self.names()))
    }

    fn result(&self, id: &str) -> ToolResultMessage {
        let results = self.results.lock().unwrap();
        let matching: Vec<_> = results.iter().filter(|r| r.tool_call_id == id).collect();
        assert_eq!(matching.len(), 1, "exactly one persisted result for {id}");
        matching[0].clone()
    }

    fn origin(&self, label: &str) -> MessageOrigin {
        let origins = self.origins.lock().unwrap();
        let matching: Vec<_> = origins.iter().filter(|(name, _)| name == label).collect();
        assert_eq!(matching.len(), 1, "exactly one {label} in {origins:?}");
        matching[0].1.clone()
    }
}

#[async_trait]
impl LoopHooks for Recorder {
    async fn finish_turn(
        &self,
        turn: TurnContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<TurnFinish, HookError> {
        self.finished_results.lock().unwrap().push(
            turn.tool_results
                .iter()
                .map(|result| result.tool_call_id.clone())
                .collect(),
        );
        self.finished_models
            .lock()
            .unwrap()
            .push(turn.model.id.clone());
        self.push("finish_turn".into());
        if self.wait_finish {
            self.entered.notify_one();
            cancel.cancelled().await;
        }
        if self.hook_failure {
            return Err(HookError("maintenance failed".into()));
        }
        if self.recover
            && (turn.message.stop_reason == StopReason::Error
                || self.should_skip_tools(turn.message, turn.model))
        {
            return Ok(TurnFinish {
                decision: Some(TurnDecision::Continue),
                context: Some(AgentContext {
                    messages: vec![user("projected")],
                    tools: turn.context.tools.clone(),
                }),
            });
        }
        if self.replace_final {
            return Ok(TurnFinish {
                decision: None,
                context: Some(AgentContext {
                    messages: vec![user("final projection")],
                    tools: turn.context.tools.clone(),
                }),
            });
        }
        Ok(TurnFinish::default())
    }

    fn should_skip_tools(&self, response: &AssistantMessage, _model: &Model) -> bool {
        self.skip_tools && response.tool_calls().next().is_some()
    }

    async fn prepare_request(
        &self,
        _request: PrepareRequestContext<'_>,
        cancel: &CancellationToken,
    ) -> Result<Option<RequestUpdate>, HookError> {
        if self.prepare_failure {
            return Err(HookError("preparation failed".into()));
        }
        if self.wait_prepare {
            self.entered.notify_one();
            cancel.cancelled().await;
            return Err(HookError("cancelled preparation".into()));
        }
        Ok(None)
    }

    async fn before_tool_call(
        &self,
        call: BeforeToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        let id = call.tool_call.id.clone();
        if let Some((_, ms)) = self
            .prep_delay_ms
            .iter()
            .find(|(delayed, _)| *delayed == id)
        {
            tokio::time::sleep(Duration::from_millis(*ms)).await;
        }
        self.push(format!("before:{id}"));
        if self.block.contains(&id) {
            return Err(ToolError("hook exploded".into()));
        }
        Ok(ToolCallDecision::Allow)
    }
}

#[async_trait]
impl Persist for Recorder {
    async fn persist(&self, event: &mut AgentEvent) -> Result<(), PersistError> {
        let name = match event {
            AgentEvent::AgentStart => "AgentStart".to_owned(),
            AgentEvent::AgentEnd { .. } => "AgentEnd".to_owned(),
            AgentEvent::TurnStart => "TurnStart".to_owned(),
            AgentEvent::TurnEnd { .. } => "TurnEnd".to_owned(),
            AgentEvent::MessageStart { .. } | AgentEvent::MessageUpdate { .. } => return Ok(()),
            AgentEvent::ToolExecutionUpdate { .. } => return Ok(()),
            AgentEvent::MessageEnd { message, origin } => {
                if self.slow_persist {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                let name = match message {
                    AgentMessage::Llm(message) => {
                        match message {
                            Message::ToolResult(result) => {
                                self.results.lock().unwrap().push(result.clone());
                            }
                            Message::Assistant(assistant) => {
                                self.assistants.lock().unwrap().push(assistant.clone());
                                if self.switch_model {
                                    let mut next = model();
                                    next.id = "next-model".into();
                                    self.agent.get().unwrap().upgrade().unwrap().set_model(next);
                                }
                            }
                            Message::System(_) | Message::User(_) => {}
                        }
                        match describe(message) {
                            Some(label) => format!("end:{label}"),
                            None => "end:system".to_owned(),
                        }
                    }
                    AgentMessage::Custom(_) => "end:custom".to_owned(),
                };
                self.origins
                    .lock()
                    .unwrap()
                    .push((name.clone(), origin.clone()));
                name
            }
            AgentEvent::ToolExecutionStart { call_id, .. } => format!("exec-start:{call_id}"),
            AgentEvent::ToolExecutionEnd {
                call_id, is_error, ..
            } => {
                self.exec_ends
                    .lock()
                    .unwrap()
                    .push((call_id.clone(), *is_error));
                format!("exec-end:{call_id}")
            }
        };
        self.push(name);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Test tools
// ---------------------------------------------------------------------------------------

/// Shared observation of tool bodies: which calls started/finished and the peak number
/// running at once.
#[derive(Default)]
struct Probe {
    running: AtomicUsize,
    max_running: AtomicUsize,
    started: Mutex<Vec<String>>,
    finished: Mutex<Vec<String>>,
}

impl Probe {
    fn started(&self) -> Vec<String> {
        self.started.lock().unwrap().clone()
    }

    fn finished(&self) -> Vec<String> {
        self.finished.lock().unwrap().clone()
    }
}

/// Sleeps for the optional `ms` argument, recording into its probe.
struct ProbeTool {
    name: &'static str,
    mode: ExecutionMode,
    probe: Arc<Probe>,
    schema: Value,
}

fn probe_tool(name: &'static str, mode: ExecutionMode, probe: &Arc<Probe>) -> Arc<dyn Tool> {
    Arc::new(ProbeTool {
        name,
        mode,
        probe: probe.clone(),
        schema: json!({
            "type": "object",
            "properties": { "ms": { "type": "integer", "minimum": 0 } }
        }),
    })
}

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Sleeps for `ms` milliseconds."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.mode
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let call = cx.call;
        self.probe.started.lock().unwrap().push(call.id.clone());
        let running = self.probe.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.probe.max_running.fetch_max(running, Ordering::SeqCst);
        let ms = call
            .arguments
            .get("ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
            _ = cx.cancel.cancelled() => {}
        }
        self.probe.running.fetch_sub(1, Ordering::SeqCst);
        self.probe.finished.lock().unwrap().push(call.id.clone());
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: format!("{} done", call.id),
            }],
            ..ToolOutput::default()
        })
    }
}

/// Always fails.
struct FailTool {
    schema: Value,
}

#[async_trait]
impl Tool for FailTool {
    fn name(&self) -> &str {
        "fail"
    }

    fn description(&self) -> &str {
        "Always fails."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    async fn execute(&self, _cx: ToolContext) -> Result<ToolOutput, ToolError> {
        Err(ToolError("tool failed".into()))
    }
}

/// Sequential tool that signals when it starts and then waits for cancellation.
struct GateTool {
    started: Notify,
    runs: AtomicUsize,
    schema: Value,
}

impl GateTool {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Notify::new(),
            runs: AtomicUsize::new(0),
            schema: json!({ "type": "object" }),
        })
    }
}

#[async_trait]
impl Tool for GateTool {
    fn name(&self) -> &str {
        "gate"
    }

    fn description(&self) -> &str {
        "Waits until cancelled."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        cx.cancel.cancelled().await;
        Err(ToolError("gate cancelled".into()))
    }
}

/// Queues a steering and/or follow-up message on its agent while it runs.
struct QueueTool {
    agent: OnceLock<Weak<Agent>>,
    steer: bool,
    follow_up: bool,
    schema: Value,
}

impl QueueTool {
    fn new(steer: bool, follow_up: bool) -> Arc<Self> {
        Arc::new(Self {
            agent: OnceLock::new(),
            steer,
            follow_up,
            schema: json!({ "type": "object" }),
        })
    }
}

#[async_trait]
impl Tool for QueueTool {
    fn name(&self) -> &str {
        "queue"
    }

    fn description(&self) -> &str {
        "Queues messages on the agent."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    async fn execute(&self, _cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let agent = self
            .agent
            .get()
            .and_then(Weak::upgrade)
            .expect("agent attached");
        if self.steer {
            agent.steer(user("steered").into());
        }
        if self.follow_up {
            agent.follow_up(user("followed").into());
        }
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: "queued".into(),
            }],
            ..ToolOutput::default()
        })
    }
}

/// Records what the steer signal reads: once on entry and, with `{"wait": true}`, again
/// after waiting for steering to become pending.
struct SteerTool {
    started: Notify,
    seen: Mutex<Vec<bool>>,
    schema: Value,
}

#[async_trait]
impl Tool for SteerTool {
    fn name(&self) -> &str {
        "steer"
    }

    fn description(&self) -> &str {
        "Observes the steer signal."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let mut steer = cx.steer.clone();
        self.seen.lock().unwrap().push(steer.is_pending());
        if cx.call.arguments.get("wait").and_then(Value::as_bool) == Some(true) {
            self.started.notify_one();
            steer.pending().await;
            self.seen.lock().unwrap().push(steer.is_pending());
        }
        Ok(ToolOutput::default())
    }
}

/// Sequential tool that records each call's invocation id. `{"steer": true}` queues a
/// steering message under id `"s1"`; `{"hang": true}` signals and waits for cancellation.
struct OriginTool {
    agent: OnceLock<Weak<Agent>>,
    invocations: Mutex<Vec<(String, ToolInvocationId)>>,
    started: Notify,
    schema: Value,
}

impl OriginTool {
    fn invocation(&self, id: &str) -> ToolInvocationId {
        self.invocations
            .lock()
            .unwrap()
            .iter()
            .find(|(call, _)| call == id)
            .map(|(_, invocation)| *invocation)
            .unwrap_or_else(|| panic!("{id} never executed"))
    }
}

#[async_trait]
impl Tool for OriginTool {
    fn name(&self) -> &str {
        "origin"
    }

    fn description(&self) -> &str {
        "Records its invocation."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.invocations
            .lock()
            .unwrap()
            .push((cx.call.id.clone(), cx.invocation));
        let flag = |name: &str| cx.call.arguments.get(name).and_then(Value::as_bool) == Some(true);
        if flag("steer") {
            let agent = self
                .agent
                .get()
                .and_then(Weak::upgrade)
                .expect("agent attached");
            agent.steer(QueuedMessage {
                message: user("steered"),
                id: Some("s1".into()),
            });
        }
        if flag("hang") {
            self.started.notify_one();
            cx.cancel.cancelled().await;
            return Err(ToolError("origin cancelled".into()));
        }
        Ok(ToolOutput::default())
    }
}

// ---------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------

struct Harness {
    agent: Arc<Agent>,
    api: Arc<ScriptedApi>,
    rec: Arc<Recorder>,
}

fn setup(
    turns: Vec<ScriptedTurn>,
    tools: Vec<Arc<dyn Tool>>,
    configure: impl FnOnce(&mut Recorder),
) -> Harness {
    let api = Arc::new(ScriptedApi::new(turns));
    let mut recorder = Recorder {
        api: api.clone(),
        log: Mutex::default(),
        results: Mutex::default(),
        assistants: Mutex::default(),
        exec_ends: Mutex::default(),
        origins: Mutex::default(),
        block: Vec::new(),
        prep_delay_ms: Vec::new(),
        slow_persist: false,
        recover: false,
        skip_tools: false,
        wait_prepare: false,
        wait_finish: false,
        hook_failure: false,
        prepare_failure: false,
        replace_final: false,
        switch_model: false,
        agent: OnceLock::new(),
        entered: Notify::new(),
        finished_results: Mutex::default(),
        finished_models: Mutex::default(),
    };
    configure(&mut recorder);
    let rec = Arc::new(recorder);
    let mut config = AgentConfig::new(api.clone(), model());
    config.tools = tools;
    config.hooks = rec.clone();
    config.persist = Some(rec.clone());
    let agent = Arc::new(Agent::new(config));
    rec.agent.set(Arc::downgrade(&agent)).unwrap();
    Harness { agent, api, rec }
}

impl Harness {
    async fn prompt(&self, text: &str) -> RunOutcome {
        tokio::time::timeout(TIMEOUT, self.agent.prompt(vec![user(text).into()]))
            .await
            .expect("run settles")
            .expect("run succeeds")
    }

    fn request(&self, index: usize) -> Vec<String> {
        convo(&self.api.requests()[index].transcript)
    }
}

fn turn(calls: &[(&str, &str, Value)]) -> ScriptedTurn {
    ScriptedTurn::Reply(reply_tool_calls(calls))
}

fn text(reply: &str) -> ScriptedTurn {
    ScriptedTurn::Reply(reply_text(reply))
}

// ---------------------------------------------------------------------------------------
// Invariants
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finish_turn_runs_before_turn_end() {
    let probe = Arc::new(Probe::default());
    let h = setup(
        vec![turn(&[("c1", "sleep", json!({}))]), text("done")],
        vec![probe_tool("sleep", ExecutionMode::Parallel, &probe)],
        |_| {},
    );
    assert_eq!(h.prompt("go").await, RunOutcome::Done);

    let names = h.rec.names();
    let turn_ends: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, name)| *name == "TurnEnd")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(turn_ends.len(), 2, "{names:?}");
    assert_eq!(
        names.iter().filter(|name| *name == "finish_turn").count(),
        2,
        "{names:?}"
    );
    for index in turn_ends {
        assert_eq!(names[index - 1], "finish_turn", "{names:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_preparation_runs_in_source_order() {
    let probe = Arc::new(Probe::default());
    let h = setup(
        vec![
            turn(&[
                ("c1", "sleep", json!({ "ms": 0 })),
                ("c2", "sleep", json!({ "ms": 0 })),
                ("c3", "sleep", json!({ "ms": 0 })),
            ]),
            text("done"),
        ],
        vec![probe_tool("sleep", ExecutionMode::Parallel, &probe)],
        // Earlier calls prepare slower: concurrent preparation would finish out of order.
        |rec| rec.prep_delay_ms = vec![("c1".into(), 80), ("c2".into(), 30)],
    );
    assert_eq!(h.prompt("go").await, RunOutcome::Done);

    let names = h.rec.names();
    let before: Vec<&String> = names.iter().filter(|n| n.starts_with("before:")).collect();
    assert_eq!(before, ["before:c1", "before:c2", "before:c3"]);
    let last_before = h.rec.position("before:c3");
    let first_end = names
        .iter()
        .position(|n| n.starts_with("exec-end:"))
        .expect("tool ends");
    assert!(
        last_before < first_end,
        "preparation finishes before any tool ends: {names:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_ends_in_completion_order_results_in_call_order() {
    let probe = Arc::new(Probe::default());
    let h = setup(
        vec![
            turn(&[
                ("c1", "sleep", json!({ "ms": 300 })),
                ("c2", "sleep", json!({ "ms": 0 })),
                ("c3", "sleep", json!({ "ms": 150 })),
            ]),
            text("done"),
        ],
        vec![probe_tool("sleep", ExecutionMode::Parallel, &probe)],
        |_| {},
    );
    assert_eq!(h.prompt("go").await, RunOutcome::Done);

    let names = h.rec.names();
    let ends: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("exec-end:"))
        .collect();
    assert_eq!(ends, ["exec-end:c2", "exec-end:c3", "exec-end:c1"]);
    let results: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("end:result:"))
        .collect();
    assert_eq!(results, ["end:result:c1", "end:result:c2", "end:result:c3"]);
    assert_eq!(
        h.request(1),
        labels(&[
            "user:go",
            "assistant:ToolUse",
            "result:c1",
            "result:c2",
            "result:c3"
        ])
    );
    assert!(
        probe.max_running.load(Ordering::SeqCst) > 1,
        "calls overlapped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_with_a_sequential_tool_runs_sequentially() {
    let probe = Arc::new(Probe::default());
    let h = setup(
        vec![
            turn(&[
                ("c1", "sleep", json!({ "ms": 60 })),
                ("c2", "seq", json!({ "ms": 60 })),
                ("c3", "sleep", json!({ "ms": 0 })),
            ]),
            text("done"),
        ],
        vec![
            probe_tool("sleep", ExecutionMode::Parallel, &probe),
            probe_tool("seq", ExecutionMode::Sequential, &probe),
        ],
        |_| {},
    );
    assert_eq!(h.prompt("go").await, RunOutcome::Done);

    assert_eq!(probe.max_running.load(Ordering::SeqCst), 1, "no overlap");
    assert_eq!(probe.finished(), labels(&["c1", "c2", "c3"]));
    let names = h.rec.names();
    let ends: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("exec-end:"))
        .collect();
    assert_eq!(ends, ["exec-end:c1", "exec-end:c2", "exec-end:c3"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn length_stop_fails_every_tool_call_without_running_it() {
    let probe = Arc::new(Probe::default());
    let mut truncated = reply_tool_calls(&[
        ("c1", "sleep", json!({ "ms": 0 })),
        ("c2", "sleep", json!({ "ms": 0 })),
    ]);
    truncated.stop_reason = StopReason::Length;
    let h = setup(
        vec![ScriptedTurn::Reply(truncated), text("done")],
        vec![probe_tool("sleep", ExecutionMode::Parallel, &probe)],
        |_| {},
    );
    h.prompt("go").await;

    assert!(probe.started().is_empty(), "no tool body ran");
    for id in ["c1", "c2"] {
        assert!(h.rec.result(id).is_error, "{id} result is an error");
    }
    let ends = h.rec.exec_ends.lock().unwrap().clone();
    assert_eq!(ends.len(), 2, "{ends:?}");
    assert!(ends.iter().all(|(_, is_error)| *is_error), "{ends:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steering_is_injected_after_all_tool_calls_finish() {
    let probe = Arc::new(Probe::default());
    let queue = QueueTool::new(true, false);
    let h = setup(
        vec![
            turn(&[
                ("c1", "queue", json!({})),
                ("c2", "sleep", json!({ "ms": 150 })),
            ]),
            text("done"),
        ],
        vec![
            queue.clone() as Arc<dyn Tool>,
            probe_tool("sleep", ExecutionMode::Parallel, &probe),
        ],
        |_| {},
    );
    assert!(queue.agent.set(Arc::downgrade(&h.agent)).is_ok());
    assert_eq!(h.prompt("go").await, RunOutcome::Done);

    assert_eq!(h.api.requests().len(), 2);
    assert_eq!(
        h.request(1),
        labels(&[
            "user:go",
            "assistant:ToolUse",
            "result:c1",
            "result:c2",
            "user:steered"
        ])
    );
    assert!(h.rec.position("exec-end:c2") < h.rec.position("end:user:steered"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follow_ups_are_used_only_when_the_run_would_end() {
    let probe = Arc::new(Probe::default());
    let h = setup(
        vec![
            turn(&[("c1", "sleep", json!({}))]),
            text("first"),
            text("second"),
        ],
        vec![probe_tool("sleep", ExecutionMode::Parallel, &probe)],
        |_| {},
    );
    h.agent.follow_up(user("later").into());
    assert_eq!(h.prompt("go").await, RunOutcome::Done);

    assert_eq!(h.api.requests().len(), 3);
    assert_eq!(h.request(0), labels(&["user:go"]));
    assert_eq!(
        h.request(1),
        labels(&["user:go", "assistant:ToolUse", "result:c1"])
    );
    assert_eq!(
        h.request(2),
        labels(&[
            "user:go",
            "assistant:ToolUse",
            "result:c1",
            "assistant:Stop",
            "user:later"
        ])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_sits_between_a_tool_call_and_its_result() {
    let probe = Arc::new(Probe::default());
    let queue = QueueTool::new(true, true);
    let h = setup(
        vec![
            turn(&[
                ("c1", "queue", json!({})),
                ("c2", "sleep", json!({ "ms": 100 })),
                ("c3", "sleep", json!({ "ms": 0 })),
            ]),
            text("after steer"),
            text("after follow-up"),
        ],
        vec![
            queue.clone() as Arc<dyn Tool>,
            probe_tool("sleep", ExecutionMode::Parallel, &probe),
        ],
        |_| {},
    );
    assert!(queue.agent.set(Arc::downgrade(&h.agent)).is_ok());
    assert_eq!(h.prompt("go").await, RunOutcome::Done);

    let requests = h.api.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_results_adjacent(&request.transcript);
    }
    assert_results_adjacent(
        &h.agent
            .messages()
            .into_iter()
            .filter_map(|message| match message {
                AgentMessage::Llm(message) => Some(message),
                AgentMessage::Custom(_) => None,
            })
            .collect::<Vec<_>>(),
    );
    let last = h.request(2);
    assert!(last.contains(&"user:steered".to_owned()), "{last:?}");
    assert!(last.contains(&"user:followed".to_owned()), "{last:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_before_tool_call_blocks_the_call() {
    let probe = Arc::new(Probe::default());
    let h = setup(
        vec![
            turn(&[
                ("c1", "sleep", json!({})),
                ("c2", "fail", json!({})),
                ("c3", "sleep", json!({})),
            ]),
            text("done"),
        ],
        vec![
            probe_tool("sleep", ExecutionMode::Parallel, &probe),
            Arc::new(FailTool {
                schema: json!({ "type": "object" }),
            }),
        ],
        |rec| rec.block = vec!["c1".into()],
    );
    assert_eq!(h.prompt("go").await, RunOutcome::Done);

    assert_eq!(probe.started(), labels(&["c3"]), "blocked call never ran");
    let blocked = h.rec.result("c1");
    assert!(blocked.is_error);
    assert!(
        first_text(&blocked.content).contains("hook exploded"),
        "{blocked:?}"
    );
    let failed = h.rec.result("c2");
    assert!(failed.is_error);
    assert!(
        first_text(&failed.content).contains("tool failed"),
        "{failed:?}"
    );
    assert!(!h.rec.result("c3").is_error);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn errored_turns_are_kept_but_not_sent() {
    let h = setup(
        vec![ScriptedTurn::Error("boom".into()), text("recovered")],
        Vec::new(),
        |_| {},
    );
    let outcome = h.prompt("first").await;
    assert!(
        matches!(&outcome, RunOutcome::Failed(error) if error.contains("boom")),
        "{outcome:?}"
    );
    let kept = h.agent.messages().iter().any(|message| {
        matches!(
            message,
            AgentMessage::Llm(Message::Assistant(assistant))
                if assistant.stop_reason == StopReason::Error
        )
    });
    assert!(kept, "errored assistant message stays in messages()");

    assert_eq!(h.prompt("second").await, RunOutcome::Done);
    assert_eq!(h.api.requests().len(), 2);
    assert_eq!(h.request(1), labels(&["user:first", "user:second"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_cancel_mid_tool_aborts_unanswered_calls() {
    let gate = GateTool::new();
    let h = setup(
        vec![
            turn(&[("c1", "gate", json!({})), ("c2", "gate", json!({}))]),
            ScriptedTurn::HangUntilCancel,
        ],
        vec![gate.clone() as Arc<dyn Tool>],
        |_| {},
    );
    let agent = h.agent.clone();
    let run = tokio::spawn(async move { agent.prompt(vec![user("go").into()]).await });
    tokio::time::timeout(TIMEOUT, gate.started.notified())
        .await
        .expect("gate started");
    h.agent.cancel(CancelReason::User);
    let outcome = tokio::time::timeout(TIMEOUT, run)
        .await
        .expect("run settles")
        .expect("task joins")
        .expect("run succeeds");

    assert_eq!(outcome, RunOutcome::Aborted(CancelReason::User));
    assert_eq!(gate.runs.load(Ordering::SeqCst), 1, "c2 never ran");
    for id in ["c1", "c2"] {
        assert!(h.rec.result(id).is_error, "{id}");
    }
    let aborted = h.rec.result("c2");
    assert_eq!(first_text(&aborted.content), "Operation aborted");
    assert!(h.rec.position("end:result:c2") < h.rec.position("AgentEnd"));
    let in_messages = h
        .agent
        .messages()
        .iter()
        .filter(|message| {
            matches!(
                message,
                AgentMessage::Llm(Message::ToolResult(result)) if result.tool_call_id == "c2"
            )
        })
        .count();
    assert_eq!(in_messages, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_cancel_persists_nothing_aborted() {
    // Mid-stream: the aborted assistant message is not persisted.
    let h = setup(vec![ScriptedTurn::HangUntilCancel], Vec::new(), |_| {});
    let mut events = h.agent.subscribe();
    let agent = h.agent.clone();
    let run = tokio::spawn(async move { agent.prompt(vec![user("go").into()]).await });
    tokio::time::timeout(TIMEOUT, async {
        while let Ok(event) = events.recv().await {
            if matches!(event, AgentEvent::MessageUpdate { .. }) {
                break;
            }
        }
    })
    .await
    .expect("stream started");
    h.agent.cancel(CancelReason::Shutdown);
    let outcome = tokio::time::timeout(TIMEOUT, run)
        .await
        .expect("run settles")
        .expect("task joins")
        .expect("run succeeds");
    assert_eq!(outcome, RunOutcome::Aborted(CancelReason::Shutdown));
    assert!(
        h.rec
            .assistants
            .lock()
            .unwrap()
            .iter()
            .all(|assistant| assistant.stop_reason != StopReason::Aborted),
        "no aborted assistant message persisted"
    );

    // Mid-tool: no aborted results are synthesized for unanswered calls.
    let gate = GateTool::new();
    let h = setup(
        vec![
            turn(&[("c1", "gate", json!({})), ("c2", "gate", json!({}))]),
            ScriptedTurn::HangUntilCancel,
        ],
        vec![gate.clone() as Arc<dyn Tool>],
        |_| {},
    );
    let agent = h.agent.clone();
    let run = tokio::spawn(async move { agent.prompt(vec![user("go").into()]).await });
    tokio::time::timeout(TIMEOUT, gate.started.notified())
        .await
        .expect("gate started");
    h.agent.cancel(CancelReason::Shutdown);
    let outcome = tokio::time::timeout(TIMEOUT, run)
        .await
        .expect("run settles")
        .expect("task joins")
        .expect("run succeeds");
    assert_eq!(outcome, RunOutcome::Aborted(CancelReason::Shutdown));
    assert_eq!(gate.runs.load(Ordering::SeqCst), 1);
    let results = h.rec.results.lock().unwrap().clone();
    assert!(
        results.iter().all(|result| result.tool_call_id != "c2"),
        "no result synthesized for the unanswered call: {results:?}"
    );
    assert!(
        results
            .iter()
            .all(|result| first_text(&result.content) != "Operation aborted"),
        "{results:?}"
    );
    assert!(
        h.rec
            .assistants
            .lock()
            .unwrap()
            .iter()
            .all(|assistant| assistant.stop_reason != StopReason::Aborted),
        "no aborted assistant message persisted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continue_run_executes_only_unanswered_calls() {
    let probe = Arc::new(Probe::default());
    let h = setup(
        vec![text("done")],
        vec![probe_tool("sleep", ExecutionMode::Parallel, &probe)],
        |_| {},
    );
    let answered = ToolResultMessage {
        tool_call_id: "c1".into(),
        tool_name: "sleep".into(),
        content: vec![UserBlock::Text {
            text: "c1 done".into(),
        }],
        details: None,
        is_error: false,
        timestamp: now_ms(),
        duration_ms: None,
    };
    h.agent.set_messages(vec![
        user("go"),
        AgentMessage::Llm(Message::Assistant(reply_tool_calls(&[
            ("c1", "sleep", json!({})),
            ("c2", "sleep", json!({})),
        ]))),
        AgentMessage::Llm(Message::ToolResult(answered)),
    ]);

    let outcome = tokio::time::timeout(TIMEOUT, h.agent.continue_run())
        .await
        .expect("run settles")
        .expect("run succeeds");
    assert_eq!(outcome, RunOutcome::Done);
    assert_eq!(probe.started(), labels(&["c2"]));
    let names = h.rec.names();
    let starts: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("exec-start:"))
        .collect();
    assert_eq!(starts, ["exec-start:c2"]);
    assert_eq!(h.api.requests().len(), 1);
    assert_eq!(
        h.request(0),
        labels(&["user:go", "assistant:ToolUse", "result:c1", "result:c2"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persist_sees_message_end_before_the_next_request() {
    let probe = Arc::new(Probe::default());
    let h = setup(
        vec![turn(&[("c1", "sleep", json!({}))]), text("done")],
        vec![probe_tool("sleep", ExecutionMode::Parallel, &probe)],
        |rec| rec.slow_persist = true,
    );
    assert_eq!(h.prompt("go").await, RunOutcome::Done);

    let log = h.rec.log.lock().unwrap().clone();
    let requests_at = |name: &str| {
        log.iter()
            .find(|(entry, _)| entry == name)
            .map(|(_, requests)| *requests)
            .unwrap_or_else(|| panic!("{name} not in {log:?}"))
    };
    assert_eq!(requests_at("end:user:go"), 0);
    assert_eq!(requests_at("end:assistant:ToolUse"), 1);
    assert_eq!(requests_at("end:result:c1"), 1);
    assert_eq!(requests_at("end:assistant:Stop"), 2);
    assert_eq!(h.api.requests().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_cancel_mid_stream_answers_streamed_calls() {
    let h = setup(
        vec![
            ScriptedTurn::HangAfter(reply_tool_calls(&[("c1", "sleep", json!({}))])),
            text("next"),
        ],
        Vec::new(),
        |_| {},
    );
    let mut events = h.agent.subscribe();
    let agent = h.agent.clone();
    let run = tokio::spawn(async move { agent.prompt(vec![user("go").into()]).await });
    tokio::time::timeout(TIMEOUT, async {
        while let Ok(event) = events.recv().await {
            if matches!(
                &event,
                AgentEvent::MessageUpdate {
                    event: AssistantEvent::ToolCallEnd {
                        block: AssistantBlock::ToolCall(call),
                        ..
                    },
                } if call.id == "c1"
            ) {
                break;
            }
        }
    })
    .await
    .expect("c1 streamed");
    h.agent.cancel(CancelReason::User);
    let outcome = tokio::time::timeout(TIMEOUT, run)
        .await
        .expect("run settles")
        .expect("task joins")
        .expect("run succeeds");

    assert_eq!(outcome, RunOutcome::Aborted(CancelReason::User));
    let answers: Vec<ToolResultMessage> = h
        .agent
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            AgentMessage::Llm(Message::ToolResult(result)) if result.tool_call_id == "c1" => {
                Some(result)
            }
            _ => None,
        })
        .collect();
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert!(answers[0].is_error);
    assert_eq!(first_text(&answers[0].content), "Operation aborted");

    // The aborted turn and its answer stay in the transcript but are never sent.
    assert_eq!(h.prompt("again").await, RunOutcome::Done);
    assert_eq!(h.request(1), labels(&["user:go", "user:again"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continue_run_from_a_reply_runs_queued_input() {
    let h = setup(vec![text("reply")], Vec::new(), |_| {});
    h.agent.set_messages(vec![
        user("go"),
        AgentMessage::Llm(Message::Assistant(reply_text("hi"))),
    ]);
    h.agent.steer(user("more").into());

    let outcome = tokio::time::timeout(TIMEOUT, h.agent.continue_run())
        .await
        .expect("run settles")
        .expect("run succeeds");
    assert_eq!(outcome, RunOutcome::Done);
    assert_eq!(h.api.requests().len(), 1);
    assert_eq!(
        h.request(0),
        labels(&["user:go", "assistant:Stop", "user:more"])
    );

    // Nothing queued: the reply is settled and no request is made.
    let outcome = tokio::time::timeout(TIMEOUT, h.agent.continue_run())
        .await
        .expect("run settles")
        .expect("run succeeds");
    assert_eq!(outcome, RunOutcome::Done);
    assert_eq!(h.api.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_started_after_shutdown_begin_cancelled() {
    let h = setup(vec![text("never")], Vec::new(), |_| {});
    h.agent.shutdown();
    assert_eq!(
        h.prompt("go").await,
        RunOutcome::Aborted(CancelReason::Shutdown)
    );
    let names = h.rec.names();
    assert!(
        names.iter().all(|name| !name.starts_with("end:")),
        "no MessageEnd persisted: {names:?}"
    );
}

#[tokio::test]
async fn failed_response_recovers_inside_one_run_after_synthetic_results() {
    let probe = Arc::new(Probe::default());
    let mut failed = reply_tool_calls(&[("c1", "sleep", json!({})), ("c2", "sleep", json!({}))]);
    failed.stop_reason = StopReason::Error;
    let h = setup(
        vec![ScriptedTurn::Reply(failed), text("done")],
        vec![probe_tool("sleep", ExecutionMode::Parallel, &probe)],
        |rec| rec.recover = true,
    );
    assert_eq!(h.prompt("go").await, RunOutcome::Done);
    assert!(probe.started().is_empty());
    assert_eq!(h.api.requests().len(), 2);
    assert_eq!(h.request(1), labels(&["user:projected"]));
    let messages: Vec<Message> = h
        .agent
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            AgentMessage::Llm(message) => Some(message),
            _ => None,
        })
        .collect();
    // Replacing the transcript drops the failed assistant and both synthetic results.
    // The loop may restore tool declarations, but only the successful reply is appended
    // to the replacement conversation.
    assert_eq!(
        convo(&messages),
        labels(&["user:projected", "assistant:Stop"])
    );
    let Some(Message::Assistant(response)) = messages.last() else {
        panic!("the recovered response must finish the public transcript");
    };
    assert!(matches!(
        response.content.as_slice(),
        [AssistantBlock::Text { text, .. }] if text == "done"
    ));
    let names = h.rec.names();
    assert_eq!(names.iter().filter(|name| *name == "AgentStart").count(), 1);
    assert_eq!(names.iter().filter(|name| *name == "AgentEnd").count(), 1);
    assert!(h.rec.position("end:result:c2") < h.rec.position("finish_turn"));
    assert!(h.rec.position("finish_turn") < h.rec.position("TurnEnd"));
    assert!(!names.contains(&"end:user:projected".to_owned()));
    for id in ["c1", "c2"] {
        assert_eq!(
            first_text(&h.rec.result(id).content),
            "Tool call was not executed because the model response could not be used."
        );
    }
}

#[tokio::test]
async fn skipped_stop_never_executes_calls_or_silently_succeeds() {
    for recover in [false, true] {
        let probe = Arc::new(Probe::default());
        let mut response = reply_tool_calls(&[("c1", "sleep", json!({}))]);
        response.stop_reason = StopReason::Stop;
        response.usage.input = model().context_window + 1;
        let h = setup(
            vec![ScriptedTurn::Reply(response), text("done")],
            vec![probe_tool("sleep", ExecutionMode::Parallel, &probe)],
            |rec| {
                rec.skip_tools = true;
                rec.recover = recover;
            },
        );
        let outcome = h.prompt("go").await;
        if recover {
            assert_eq!(outcome, RunOutcome::Done);
            assert_eq!(h.api.requests().len(), 2);
        } else {
            assert!(matches!(outcome, RunOutcome::Failed(_)));
            assert_eq!(h.api.requests().len(), 1);
        }
        assert!(probe.started().is_empty());
        assert!(h.rec.result("c1").is_error);
    }
}

#[tokio::test]
async fn recovered_response_reuses_results_without_reemitting_assistant() {
    let probe = Arc::new(Probe::default());
    let h = setup(
        vec![text("done")],
        vec![probe_tool("sleep", ExecutionMode::Parallel, &probe)],
        |rec| rec.recover = true,
    );
    let mut response = reply_tool_calls(&[
        ("c1", "sleep", json!({})),
        ("c2", "sleep", json!({})),
        ("c3", "sleep", json!({})),
    ]);
    response.stop_reason = StopReason::Error;
    let result = |id: &str| ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "sleep".into(),
        content: vec![UserBlock::Text {
            text: "already completed".into(),
        }],
        details: None,
        is_error: false,
        timestamp: now_ms(),
        duration_ms: None,
    };
    let mut original_model = model();
    original_model.id = "original-request-model".into();
    h.agent.set_messages(vec![user("go")]);
    assert_eq!(
        h.agent
            .recover_response(response, original_model, vec![result("c3"), result("c1")])
            .await
            .unwrap(),
        RunOutcome::Done,
    );
    assert_eq!(h.api.requests().len(), 1);
    assert!(probe.started().is_empty());
    assert_eq!(h.rec.results.lock().unwrap().len(), 1);
    assert!(h.rec.result("c2").is_error);
    assert_eq!(
        h.rec.finished_results.lock().unwrap()[0],
        labels(&["c1", "c2", "c3"])
    );
    assert_eq!(
        h.rec.finished_models.lock().unwrap()[0],
        "original-request-model"
    );
    assert_eq!(h.rec.assistants.lock().unwrap().len(), 1);
    let names = h.rec.names();
    assert_eq!(names.iter().filter(|name| *name == "AgentStart").count(), 1);
    assert_eq!(names.iter().filter(|name| *name == "AgentEnd").count(), 1);
    assert!(h.rec.position("end:result:c2") < h.rec.position("finish_turn"));
}

#[tokio::test]
async fn cancellation_during_hooks_wins_over_errors_and_continuation() {
    for prepare in [false, true] {
        for reason in [CancelReason::User, CancelReason::Shutdown] {
            let h = setup(
                vec![ScriptedTurn::Error("transient".into()), text("never")],
                Vec::new(),
                |rec| {
                    rec.wait_prepare = prepare;
                    rec.wait_finish = !prepare;
                    rec.hook_failure = prepare;
                    rec.recover = true;
                },
            );
            let agent = h.agent.clone();
            let run = tokio::spawn(async move { agent.prompt(vec![user("go").into()]).await });
            tokio::time::timeout(TIMEOUT, h.rec.entered.notified())
                .await
                .unwrap();
            h.agent.cancel(reason);
            let outcome = tokio::time::timeout(TIMEOUT, run)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(outcome, RunOutcome::Aborted(reason));
            assert_eq!(h.api.requests().len(), usize::from(!prepare));
            let names = h.rec.names();
            assert_eq!(names.iter().filter(|name| *name == "AgentEnd").count(), 1);
            assert_eq!(
                names.iter().filter(|name| *name == "TurnEnd").count(),
                usize::from(!prepare && reason == CancelReason::User)
            );
        }
    }
}

#[tokio::test]
async fn hook_error_uses_one_persisted_terminal_event() {
    let h = setup(vec![text("done")], Vec::new(), |rec| {
        rec.hook_failure = true
    });
    assert_eq!(
        h.prompt("go").await,
        RunOutcome::Failed("maintenance failed".into())
    );
    let names = h.rec.names();
    assert_eq!(names.iter().filter(|name| *name == "AgentEnd").count(), 1);
    assert_eq!(names.iter().filter(|name| *name == "TurnEnd").count(), 1);
    assert!(h.rec.position("finish_turn") < h.rec.position("TurnEnd"));
}

#[tokio::test]
async fn preparation_failure_does_not_manufacture_a_turn() {
    let h = setup(vec![text("never")], Vec::new(), |rec| {
        rec.prepare_failure = true
    });
    assert_eq!(
        h.prompt("go").await,
        RunOutcome::Failed("preparation failed".into())
    );
    assert!(h.api.requests().is_empty());
    assert!(h.rec.assistants.lock().unwrap().is_empty());
    let names = h.rec.names();
    assert!(!names.contains(&"TurnEnd".to_owned()));
    assert_eq!(names.iter().filter(|name| *name == "AgentEnd").count(), 1);
}

#[tokio::test]
async fn final_context_replacement_is_public_without_duplicate_messages() {
    let h = setup(vec![text("done")], Vec::new(), |rec| {
        rec.replace_final = true
    });
    assert_eq!(h.prompt("go").await, RunOutcome::Done);
    assert_eq!(h.api.requests().len(), 1);
    let messages: Vec<Message> = h
        .agent
        .messages()
        .into_iter()
        .filter_map(|message| {
            if let AgentMessage::Llm(message) = message {
                Some(message)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(convo(&messages), labels(&["user:final projection"]));
    assert!(
        !h.rec
            .names()
            .contains(&"end:user:final projection".to_owned())
    );
}

#[tokio::test]
async fn response_classification_uses_request_model_after_live_switch() {
    let h = setup(vec![text("done")], Vec::new(), |rec| {
        rec.switch_model = true
    });
    assert_eq!(h.prompt("go").await, RunOutcome::Done);
    assert_eq!(
        *h.rec.finished_models.lock().unwrap(),
        labels(&["scripted-model"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steer_signal_is_pending_until_drained() {
    let tool = Arc::new(SteerTool {
        started: Notify::new(),
        seen: Mutex::default(),
        schema: json!({ "type": "object" }),
    });
    let h = setup(
        vec![
            turn(&[("c1", "steer", json!({ "wait": true }))]),
            turn(&[("c2", "steer", json!({}))]),
            text("done"),
        ],
        vec![tool.clone() as Arc<dyn Tool>],
        |_| {},
    );
    let agent = h.agent.clone();
    let run = tokio::spawn(async move { agent.prompt(vec![user("go").into()]).await });
    tokio::time::timeout(TIMEOUT, tool.started.notified())
        .await
        .expect("c1 started");
    h.agent.steer(user("steered").into());
    let outcome = tokio::time::timeout(TIMEOUT, run)
        .await
        .expect("steering releases the waiting tool")
        .expect("task joins")
        .expect("run succeeds");

    assert_eq!(outcome, RunOutcome::Done);
    // c1: idle on entry, pending after the steer; c2 runs after the steer was drained.
    assert_eq!(*tool.seen.lock().unwrap(), [false, true, false]);
    assert_eq!(
        h.request(1),
        labels(&["user:go", "assistant:ToolUse", "result:c1", "user:steered"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_end_origins_follow_the_persisted_result() {
    let tool = Arc::new(OriginTool {
        agent: OnceLock::new(),
        invocations: Mutex::default(),
        started: Notify::new(),
        schema: json!({ "type": "object" }),
    });
    let h = setup(
        vec![
            turn(&[("c1", "origin", json!({ "steer": true }))]),
            turn(&[
                ("c2", "origin", json!({ "hang": true })),
                ("c3", "origin", json!({ "hang": true })),
            ]),
            ScriptedTurn::HangUntilCancel,
        ],
        vec![tool.clone() as Arc<dyn Tool>],
        |_| {},
    );
    assert!(tool.agent.set(Arc::downgrade(&h.agent)).is_ok());
    let agent = h.agent.clone();
    let run = tokio::spawn(async move { agent.prompt(vec![user("go").into()]).await });
    tokio::time::timeout(TIMEOUT, tool.started.notified())
        .await
        .expect("c2 started");
    h.agent.cancel(CancelReason::User);
    let outcome = tokio::time::timeout(TIMEOUT, run)
        .await
        .expect("run settles")
        .expect("task joins")
        .expect("run succeeds");
    assert_eq!(outcome, RunOutcome::Aborted(CancelReason::User));

    assert_eq!(h.rec.origin("end:user:go"), MessageOrigin::Input(None));
    assert_eq!(
        h.rec.origin("end:user:steered"),
        MessageOrigin::Input(Some("s1".into()))
    );
    // Executed results carry their call's invocation, even when cancellation ended the tool.
    let (c1, c2) = (tool.invocation("c1"), tool.invocation("c2"));
    assert_ne!(c1, c2);
    assert_eq!(h.rec.origin("end:result:c1"), MessageOrigin::Tool(c1));
    assert_eq!(h.rec.origin("end:result:c2"), MessageOrigin::Tool(c2));
    // The unanswered call never ran; its synthetic aborted result belongs to the run.
    assert!(
        tool.invocations
            .lock()
            .unwrap()
            .iter()
            .all(|(id, _)| id != "c3")
    );
    assert_eq!(first_text(&h.rec.result("c3").content), "Operation aborted");
    assert_eq!(h.rec.origin("end:result:c3"), MessageOrigin::Run);
    let origins = h.rec.origins.lock().unwrap();
    assert!(
        origins
            .iter()
            .filter(|(name, _)| name.starts_with("end:assistant:"))
            .all(|(_, origin)| *origin == MessageOrigin::Run),
        "{origins:?}"
    );
}

// ---------------------------------------------------------------------------------------
// Tool calls run on their own tasks
// ---------------------------------------------------------------------------------------

/// A hang fails the test instead of stalling the suite; covers the 3 s stop grace.
const SETTLE: Duration = Duration::from_secs(10);

fn bare_agent(
    turns: Vec<ScriptedTurn>,
    tools: Vec<Arc<dyn Tool>>,
    persist: Arc<dyn Persist>,
) -> Arc<Agent> {
    let mut config = AgentConfig::new(Arc::new(ScriptedApi::new(turns)), model());
    config.tools = tools;
    config.persist = Some(persist);
    Arc::new(Agent::new(config))
}

/// Shared state of the lock-inversion scenario.
#[derive(Default)]
struct Inversion {
    lock: Arc<tokio::sync::Mutex<()>>,
    /// `b` is about to queue on `lock`.
    b_queued: Notify,
    /// `persist` is about to queue on `lock`.
    persist_waiting: Notify,
}

/// Persists `ToolExecutionEnd` under `lock`, like a session journal behind one mutex.
struct LockedPersist(Arc<Inversion>);

#[async_trait]
impl Persist for LockedPersist {
    async fn persist(&self, event: &mut AgentEvent) -> Result<(), PersistError> {
        if matches!(event, AgentEvent::ToolExecutionEnd { .. }) {
            self.0.persist_waiting.notify_one();
            let _guard = self.0.lock.lock().await;
        }
        Ok(())
    }
}

/// `{"role": "b"}` queues on the persist lock; `{"role": "a"}` returns once `b` has.
struct InversionTool {
    state: Arc<Inversion>,
    schema: Value,
}

#[async_trait]
impl Tool for InversionTool {
    fn name(&self) -> &str {
        "inversion"
    }

    fn description(&self) -> &str {
        "Waits on the persist lock."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        if cx.call.arguments.get("role").and_then(Value::as_str) == Some("b") {
            self.state.b_queued.notify_one();
            let _guard = self.state.lock.lock().await;
        } else {
            self.state.b_queued.notified().await;
        }
        Ok(ToolOutput::default())
    }
}

/// A tool queued on the lock `Persist` needs, granted it while a sibling's end
/// persists, must not wedge the run: the loop never polls tool futures itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_waiting_on_the_persist_lock_does_not_wedge_the_run() {
    let state = Arc::new(Inversion::default());
    let agent = bare_agent(
        vec![
            turn(&[
                ("a", "inversion", json!({ "role": "a" })),
                ("b", "inversion", json!({ "role": "b" })),
            ]),
            text("done"),
        ],
        vec![Arc::new(InversionTool {
            state: state.clone(),
            schema: json!({ "type": "object" }),
        })],
        Arc::new(LockedPersist(state.clone())),
    );
    // Hold the lock so `b` queues on it; release it once `a`'s end is persisting, so
    // the fair mutex hands the permit to `b`, ahead of `persist`.
    let held = state.lock.clone().lock_owned().await;
    let release = {
        let state = state.clone();
        tokio::spawn(async move {
            state.persist_waiting.notified().await;
            drop(held);
        })
    };
    let outcome = tokio::time::timeout(SETTLE, agent.prompt(vec![user("go").into()]))
        .await
        .expect("run settles")
        .expect("run succeeds");
    assert_eq!(outcome, RunOutcome::Done);
    release.await.unwrap();
}

/// Never returns and ignores its cancellation token.
struct ParkedTool {
    started: Notify,
    schema: Value,
}

#[async_trait]
impl Tool for ParkedTool {
    fn name(&self) -> &str {
        "parked"
    }

    fn description(&self) -> &str {
        "Parks forever."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    async fn execute(&self, _cx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.started.notify_one();
        std::future::pending().await
    }
}

/// Records persisted tool events as `update:<id>` / `end:<id>`.
#[derive(Default)]
struct ToolEvents(Mutex<Vec<String>>);

impl ToolEvents {
    fn names(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait]
impl Persist for ToolEvents {
    async fn persist(&self, event: &mut AgentEvent) -> Result<(), PersistError> {
        let name = match event {
            AgentEvent::ToolExecutionUpdate { call_id, .. } => format!("update:{call_id}"),
            AgentEvent::ToolExecutionEnd { call_id, .. } => format!("end:{call_id}"),
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } => format!(
                "result:{}:{}",
                result.tool_call_id,
                first_text(&result.content)
            ),
            _ => return Ok(()),
        };
        self.0.lock().unwrap().push(name);
        Ok(())
    }
}

/// Stopping a batch whose tools ignore cancellation still settles: unfinished calls
/// are aborted and answered as such.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_aborts_parked_tools() {
    let tool = Arc::new(ParkedTool {
        started: Notify::new(),
        schema: json!({ "type": "object" }),
    });
    let events = Arc::new(ToolEvents::default());
    let agent = bare_agent(
        vec![turn(&[
            ("p1", "parked", json!({})),
            ("p2", "parked", json!({})),
        ])],
        vec![tool.clone() as Arc<dyn Tool>],
        events.clone(),
    );
    let run = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt(vec![user("go").into()]).await }
    });
    for _ in 0..2 {
        tokio::time::timeout(SETTLE, tool.started.notified())
            .await
            .expect("tool started");
    }
    agent.cancel(CancelReason::User);
    let outcome = tokio::time::timeout(SETTLE, run)
        .await
        .expect("run settles")
        .expect("task joins")
        .expect("run succeeds");
    assert_eq!(outcome, RunOutcome::Aborted(CancelReason::User));
    let names = events.names();
    for id in ["p1", "p2"] {
        assert!(names.contains(&format!("end:{id}")), "{names:?}");
        assert!(
            names.contains(&format!("result:{id}:Operation aborted")),
            "{names:?}"
        );
    }
}

/// Sends three updates, then returns.
struct ChattyTool {
    schema: Value,
}

#[async_trait]
impl Tool for ChattyTool {
    fn name(&self) -> &str {
        "chatty"
    }

    fn description(&self) -> &str {
        "Streams updates."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        for step in 0..3 {
            cx.updates.send(axo_agent::ToolUpdate {
                content: vec![UserBlock::Text {
                    text: format!("step {step}"),
                }],
                details: None,
            });
            tokio::task::yield_now().await;
        }
        Ok(ToolOutput::default())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn updates_sent_before_return_precede_the_end() {
    let events = Arc::new(ToolEvents::default());
    let agent = bare_agent(
        vec![
            turn(&[("u1", "chatty", json!({})), ("u2", "chatty", json!({}))]),
            text("done"),
        ],
        vec![Arc::new(ChattyTool {
            schema: json!({ "type": "object" }),
        })],
        events.clone(),
    );
    let outcome = tokio::time::timeout(SETTLE, agent.prompt(vec![user("go").into()]))
        .await
        .expect("run settles")
        .expect("run succeeds");
    assert_eq!(outcome, RunOutcome::Done);
    let names = events.names();
    for id in ["u1", "u2"] {
        let end = names
            .iter()
            .position(|name| *name == format!("end:{id}"))
            .unwrap_or_else(|| panic!("no end for {id}: {names:?}"));
        let updates: Vec<usize> = names
            .iter()
            .enumerate()
            .filter(|(_, name)| **name == format!("update:{id}"))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(updates.len(), 3, "{names:?}");
        assert!(updates.iter().all(|&index| index < end), "{names:?}");
    }
}

/// Returns its own result once its call is cancelled.
struct GracefulTool {
    started: Notify,
    schema: Value,
}

#[async_trait]
impl Tool for GracefulTool {
    fn name(&self) -> &str {
        "graceful"
    }

    fn description(&self) -> &str {
        "Stops cleanly when cancelled."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.started.notify_one();
        cx.cancel.cancelled().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: "stopped cleanly".into(),
            }],
            ..ToolOutput::default()
        })
    }
}

/// Stop is cooperative: a call that honors its token keeps the result it returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_keeps_a_cooperative_tools_own_result() {
    let tool = Arc::new(GracefulTool {
        started: Notify::new(),
        schema: json!({ "type": "object" }),
    });
    let events = Arc::new(ToolEvents::default());
    let agent = bare_agent(
        vec![turn(&[("g1", "graceful", json!({}))])],
        vec![tool.clone() as Arc<dyn Tool>],
        events.clone(),
    );
    let run = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt(vec![user("go").into()]).await }
    });
    tokio::time::timeout(SETTLE, tool.started.notified())
        .await
        .expect("tool started");
    agent.cancel(CancelReason::User);
    tokio::time::timeout(SETTLE, run)
        .await
        .expect("run settles")
        .expect("task joins")
        .expect("run succeeds");
    let names = events.names();
    assert!(
        names.contains(&"result:g1:stopped cleanly".to_owned()),
        "{names:?}"
    );
    assert!(
        !names.contains(&"result:g1:Operation aborted".to_owned()),
        "{names:?}"
    );
}

/// Marks that it ran.
struct MarkTool {
    ran: Arc<AtomicBool>,
    schema: Value,
}

#[async_trait]
impl Tool for MarkTool {
    fn name(&self) -> &str {
        "mark"
    }

    fn description(&self) -> &str {
        "Marks that it ran."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    async fn execute(&self, _cx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.ran.store(true, Ordering::SeqCst);
        Ok(ToolOutput::default())
    }
}

/// Delays clearing call `b`, then records whether any call already ran.
struct SlowClearance {
    ran: Arc<AtomicBool>,
    seen: Mutex<Option<bool>>,
}

#[async_trait]
impl LoopHooks for SlowClearance {
    async fn before_tool_call(
        &self,
        call: BeforeToolCallContext<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        if call.tool_call.id == "b" {
            tokio::time::sleep(Duration::from_millis(100)).await;
            *self.seen.lock().unwrap() = Some(self.ran.load(Ordering::SeqCst));
        }
        Ok(ToolCallDecision::Allow)
    }
}

/// A parallel batch starts executing only once every call passed `before_tool_call`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_call_starts_before_the_batch_is_cleared() {
    let ran = Arc::new(AtomicBool::new(false));
    let hooks = Arc::new(SlowClearance {
        ran: ran.clone(),
        seen: Mutex::new(None),
    });
    let mut config = AgentConfig::new(
        Arc::new(ScriptedApi::new(vec![
            turn(&[("a", "mark", json!({})), ("b", "mark", json!({}))]),
            text("done"),
        ])),
        model(),
    );
    config.tools = vec![Arc::new(MarkTool {
        ran: ran.clone(),
        schema: json!({ "type": "object" }),
    })];
    config.hooks = hooks.clone();
    let agent = Agent::new(config);
    let outcome = tokio::time::timeout(SETTLE, agent.prompt(vec![user("go").into()]))
        .await
        .expect("run settles")
        .expect("run succeeds");
    assert_eq!(outcome, RunOutcome::Done);
    assert_eq!(*hooks.seen.lock().unwrap(), Some(false));
    assert!(ran.load(Ordering::SeqCst));
}

/// Fails persisting the end of call `fast`.
struct FailFastEnd;

#[async_trait]
impl Persist for FailFastEnd {
    async fn persist(&self, event: &mut AgentEvent) -> Result<(), PersistError> {
        match event {
            AgentEvent::ToolExecutionEnd { call_id, .. } if call_id == "fast" => {
                Err(PersistError("disk full".into()))
            }
            _ => Ok(()),
        }
    }
}

/// `{"slow": true}` waits for its cancellation, then finishes after a delay; otherwise
/// returns at once.
struct SettleTool {
    done: AtomicBool,
    schema: Value,
}

#[async_trait]
impl Tool for SettleTool {
    fn name(&self) -> &str {
        "settle"
    }

    fn description(&self) -> &str {
        "Finishes after its cancellation when slow."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        if cx.call.arguments.get("slow").and_then(Value::as_bool) == Some(true) {
            cx.cancel.cancelled().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.done.store(true, Ordering::SeqCst);
        }
        Ok(ToolOutput::default())
    }
}

/// A persistence failure mid-batch stops the other calls and returns only after
/// every call was joined, so no tool task outlives the loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_persist_joins_every_call_first() {
    let tool = Arc::new(SettleTool {
        done: AtomicBool::new(false),
        schema: json!({ "type": "object" }),
    });
    let agent = bare_agent(
        vec![
            turn(&[
                ("slow", "settle", json!({ "slow": true })),
                ("fast", "settle", json!({})),
            ]),
            text("done"),
        ],
        vec![tool.clone() as Arc<dyn Tool>],
        Arc::new(FailFastEnd),
    );
    let _ = tokio::time::timeout(SETTLE, agent.prompt(vec![user("go").into()]))
        .await
        .expect("run settles");
    assert!(tool.done.load(Ordering::SeqCst));
}
