//! Ordering and lifecycle invariants of the agent loop (`docs/core.md` "Agent loop"),
//! exercised through the public `Agent` API with a scripted provider.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use axo_agent::{
    Agent, AgentConfig, AgentEvent, AgentMessage, BeforeToolCallContext, CancelReason,
    ExecutionMode, LoopHooks, Persist, PersistError, RunOutcome, Tool, ToolCallDecision, ToolError,
    ToolOutput, ToolUpdate, TurnContext, TurnDecision,
};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{
    ApiKind, AssistantBlock, AssistantEvent, AssistantMessage, Compat, Cost, InputKind, Message,
    Model, ScriptedApi, ScriptedTurn, StopReason, ToolCall, ToolResultMessage, UserBlock,
    UserMessage, now_ms,
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
    /// Call ids whose `before_tool_call` fails.
    block: Vec<String>,
    /// Per-call-id delay inside `before_tool_call`.
    prep_delay_ms: Vec<(String, u64)>,
    /// Delay inside `persist` for `MessageEnd`, to expose a loop that doesn't await it.
    slow_persist: bool,
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
}

#[async_trait]
impl LoopHooks for Recorder {
    async fn finish_turn(
        &self,
        _turn: TurnContext<'_>,
        _cancel: &CancellationToken,
    ) -> Option<TurnDecision> {
        self.push("finish_turn".into());
        None
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
            AgentEvent::MessageEnd { message } => {
                if self.slow_persist {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                match message {
                    AgentMessage::Llm(message) => {
                        match message {
                            Message::ToolResult(result) => {
                                self.results.lock().unwrap().push(result.clone());
                            }
                            Message::Assistant(assistant) => {
                                self.assistants.lock().unwrap().push(assistant.clone());
                            }
                            Message::System(_) | Message::User(_) => {}
                        }
                        match describe(message) {
                            Some(label) => format!("end:{label}"),
                            None => "end:system".to_owned(),
                        }
                    }
                    AgentMessage::Custom(_) => "end:custom".to_owned(),
                }
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

    async fn execute(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        _updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
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
            _ = cancel.cancelled() => {}
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

    async fn execute(
        &self,
        _call: &ToolCall,
        _cancel: CancellationToken,
        _updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
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

    async fn execute(
        &self,
        _call: &ToolCall,
        cancel: CancellationToken,
        _updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        cancel.cancelled().await;
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

    async fn execute(
        &self,
        _call: &ToolCall,
        _cancel: CancellationToken,
        _updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
        let agent = self
            .agent
            .get()
            .and_then(Weak::upgrade)
            .expect("agent attached");
        if self.steer {
            agent.steer(user("steered"));
        }
        if self.follow_up {
            agent.follow_up(user("followed"));
        }
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: "queued".into(),
            }],
            ..ToolOutput::default()
        })
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
        block: Vec::new(),
        prep_delay_ms: Vec::new(),
        slow_persist: false,
    };
    configure(&mut recorder);
    let rec = Arc::new(recorder);
    let mut config = AgentConfig::new(api.clone(), model());
    config.tools = tools;
    config.hooks = rec.clone();
    config.persist = Some(rec.clone());
    Harness {
        agent: Arc::new(Agent::new(config)),
        api,
        rec,
    }
}

impl Harness {
    async fn prompt(&self, text: &str) -> RunOutcome {
        tokio::time::timeout(TIMEOUT, self.agent.prompt(vec![user(text)]))
            .await
            .expect("run settles")
            .expect("run succeeds")
    }

    fn request(&self, index: usize) -> Vec<String> {
        convo(&self.api.requests()[index])
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
    h.agent.follow_up(user("later"));
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
        assert_results_adjacent(request);
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
    let run = tokio::spawn(async move { agent.prompt(vec![user("go")]).await });
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
    let run = tokio::spawn(async move { agent.prompt(vec![user("go")]).await });
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
    let run = tokio::spawn(async move { agent.prompt(vec![user("go")]).await });
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
    let run = tokio::spawn(async move { agent.prompt(vec![user("go")]).await });
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
    h.agent.steer(user("more"));

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
