use crate::common;

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use axo::{
    Entry, HostTool, MessageDelivery, ModeSpec, RecoveryPolicy, SessionEvent, SessionOptions,
    SessionTools, ToolLimits, ToolPolicy, ToolScope,
};
use axo_agent::{
    AfterToolCallContext, AfterToolCallResult, AgentEvent, AgentMessage, BeforeToolCallContext,
    RunOutcome, Tool, ToolCallDecision, ToolContext, ToolError, ToolOutput,
};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{AssistantBlock, Message, ScriptedTurn, UserBlock};
use common::{WriteGate, bash_options, journal, only, within};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

static NO_ARGS: LazyLock<Value> = LazyLock::new(|| json!({"type": "object"}));

struct Outer {
    calls: Vec<(&'static str, Value)>,
    detach: bool,
}

#[async_trait]
impl Tool for Outer {
    fn name(&self) -> &str {
        "outer"
    }
    fn description(&self) -> &str {
        "Call the tools in the fixture."
    }
    fn parameters(&self) -> &Value {
        &NO_ARGS
    }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        for (name, args) in &self.calls {
            let call = cx.calls.call(name, args.as_object().unwrap().clone());
            if !self.detach {
                call.await;
            }
        }
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: "outer complete".into(),
            }],
            ..Default::default()
        })
    }
}

fn options(dir: &Path, calls: Vec<(&'static str, Value)>, detach: bool) -> SessionOptions {
    let mut options = bash_options(dir);
    let outer = Arc::new(Outer { calls, detach });
    options
        .host
        .tools
        .push(HostTool::direct(Arc::new(move |_: &Arc<SessionTools>| {
            outer.clone() as Arc<dyn Tool>
        })));
    options
}

fn turns() -> Vec<ScriptedTurn> {
    vec![
        ScriptedTurn::Reply(reply_tool_calls(&[("root", "outer", json!({}))])),
        ScriptedTurn::Reply(reply_text("done")),
    ]
}

fn is_nested(entry: &Value, name: &str) -> bool {
    entry["type"] == "nested_tool_call" && entry["name"] == name
}

#[tokio::test]
async fn journal_keeps_nested_calls_out_of_context_and_recovers_an_interrupted_root() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(dir.path(), turns()).await;
    let options = options(
        dir.path(),
        vec![
            ("read", json!({"path": "example.txt"})),
            ("bash", json!({"command": "printf nested-bash-marker"})),
        ],
        false,
    );
    std::fs::write(
        options.cwd.join("example.txt"),
        "nested-read-markeré".repeat(150),
    )
    .unwrap();
    let session = host.axo.create_session(options).await.unwrap();
    assert!(matches!(
        within(session.prompt("try the tools")).await.unwrap(),
        RunOutcome::Done
    ));

    let lines = journal(&session);
    let read = only(&lines, "nested read", |v| is_nested(v, "read"));
    let bash = only(&lines, "nested bash", |v| is_nested(v, "bash"));
    let outer = only(&lines, "outer result", |v| {
        v["message"]["toolCallId"] == "root"
    });
    assert!(read < bash && bash < outer);
    for (i, at) in [read, bash].into_iter().enumerate() {
        assert_eq!(lines[at]["rootCallId"], "root");
        assert_eq!(lines[at]["callId"], format!("root/{}", i + 1));
        assert_eq!(lines[at]["isError"], false);
        assert!(lines[at]["durationMs"].is_u64());
    }
    let read_text = lines[read]["result"].as_str().unwrap();
    assert!(read_text.contains("nested-read-marker"));
    assert!((2047..=2048).contains(&read_text.len()));
    assert_eq!(lines[read]["arguments"], json!({"path": "example.txt"}));
    assert!(
        lines[bash]["result"]
            .as_str()
            .unwrap()
            .contains("nested-bash-marker")
    );
    let projected = serde_json::to_string(&session.attach(None).await.live.messages).unwrap();
    assert!(!projected.contains("nested-read-marker") && !projected.contains("nested-bash-marker"));

    let path = session.path().to_path_buf();
    session.shutdown().await;
    drop(session);
    // Cut the journal at the first nested result and simulate an added field from a newer build.
    let raw = std::fs::read_to_string(&path).unwrap();
    let mut latest = lines[read].clone();
    latest["futureField"] = json!({"extra": true});
    let prefix = raw.lines().take(read + 1).collect::<Vec<_>>().join("\n");
    std::fs::write(&path, format!("{prefix}\n{latest}\n")).unwrap();
    let mut options = bash_options(dir.path());
    options.recovery = RecoveryPolicy::MarkInterrupted;
    let reopened = host.axo.open_session(&path, options).await.unwrap();
    let attached = reopened.attach(None).await;
    assert!(attached.missed.iter().any(|seq| matches!(&seq.entry,
        Entry::NestedToolCall { name, .. } if name == "read")));
    let effects = "1 nested calls had finished (read ×1); their effects stay.";
    assert!(attached.live.messages.iter().any(|message| matches!(message,
        AgentMessage::Llm(Message::ToolResult(result)) if result.tool_call_id == "root"
            && result.is_error && result.content.iter().filter_map(UserBlock::as_text).any(|text| text.contains(effects)))));
    reopened.shutdown().await;
}

#[tokio::test]
async fn journal_bounds_large_nested_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(dir.path(), turns()).await;
    let content = "€".repeat(350_000);
    let numbers = (0..10_000).collect::<Vec<_>>();
    let number_bytes = serde_json::to_vec(&numbers).unwrap().len();
    let options = options(
        dir.path(),
        vec![
            ("write", json!({"path": "big.txt", "content": content})),
            ("find", json!({"pattern": "big.txt", "values": numbers})),
        ],
        false,
    );
    let path = options.cwd.join("big.txt");
    let session = host.axo.create_session(options).await.unwrap();
    within(session.prompt("write a file")).await.unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), content);
    let lines = journal(&session);
    let write = &lines[only(&lines, "nested write", |v| is_nested(v, "write"))];
    assert!(write.to_string().len() < 16 * 1024);
    let expected = format!("{}… [{} bytes]", &content[..1023], content.len());
    assert_eq!(write["arguments"]["content"], expected);
    assert!(write["details"]["files"].is_array());
    let find = &lines[only(&lines, "nested find", |v| is_nested(v, "find"))];
    assert_eq!(find["isError"], false);
    assert!(find.to_string().len() < 16 * 1024);
    assert_eq!(
        find["arguments"],
        json!({"pattern": "… [9 bytes]", "values": format!("… [{number_bytes} bytes]")})
    );
    session.shutdown().await;
}

#[tokio::test]
async fn nested_job_receipts_survive_reopen_without_redelivery() {
    let dir = tempfile::tempdir().unwrap();
    let mut turns = turns();
    turns.push(ScriptedTurn::Reply(reply_text("reopened")));
    let host = common::host(dir.path(), turns).await;
    let options = options(
        dir.path(),
        vec![
            (
                "bash",
                json!({
                    "command": "while [ ! -f proceed ]; do sleep 0.02; done; printf job-marker",
                    "async": true,
                }),
            ),
            ("wait", json!({})),
        ],
        false,
    );
    let proceed = options.cwd.join("proceed");
    let session = host.axo.create_session(options).await.unwrap();
    let release = async {
        common::entry(&session, |entry| {
            matches!(entry,
            Entry::NestedToolCall { name, admitted_jobs, .. }
                if name == "bash" && admitted_jobs == &["bg_1"])
        })
        .await;
        std::fs::write(proceed, "").unwrap();
    };
    let (result, ()) =
        within(async { tokio::join!(session.prompt("run then wait"), release) }).await;
    assert!(matches!(result.unwrap(), RunOutcome::Done));
    let lines = journal(&session);
    let start = only(&lines, "job start", |v| v["type"] == "job_start");
    let admitted = only(&lines, "admission", |v| v.get("admittedJobs").is_some());
    let end = only(&lines, "job end", |v| v["type"] == "job_end");
    let delivered = only(&lines, "delivery", |v| v.get("deliveredJobs").is_some());
    assert!(start < admitted && admitted < end && end < delivered);
    assert!(is_nested(&lines[admitted], "bash"));
    assert_eq!(lines[admitted]["admittedJobs"], json!(["bg_1"]));
    assert!(is_nested(&lines[delivered], "wait"));
    assert_eq!(lines[delivered]["deliveredJobs"], json!(["bg_1"]));
    assert!(
        lines[delivered]["result"]
            .as_str()
            .unwrap()
            .contains("job-marker")
    );

    let reopened = common::reopen(&host.axo, session, || bash_options(dir.path())).await;
    assert_eq!(
        reopened.jobs()[0].messages[0].delivery,
        axo::MessageDelivery::Delivered
    );
    assert!(matches!(
        within(reopened.prompt("continue after reopening"))
            .await
            .unwrap(),
        RunOutcome::Done
    ));
    let lines = journal(&reopened);
    only(&lines, "delivery", |v| v.get("deliveredJobs").is_some());
    assert!(
        !lines
            .iter()
            .any(|v| v["message"]["customType"] == "job_result")
    );
    reopened.shutdown().await;
}

#[derive(Default)]
struct HoldWaitReceipt {
    reserved: Notify,
    persist: Notify,
}

#[async_trait]
impl ToolPolicy for HoldWaitReceipt {
    async fn after_tool_call(
        &self,
        _: &ToolScope,
        call: AfterToolCallContext<'_>,
        _: &CancellationToken,
    ) -> Result<Option<AfterToolCallResult>, ToolError> {
        match call.tool_call.name.as_str() {
            "outer" => self.reserved.notified().await,
            "wait" => {
                self.reserved.notify_one();
                self.persist.notified().await;
            }
            _ => {}
        }
        Ok(None)
    }
}

#[tokio::test]
async fn settling_run_keeps_nested_wait_reserved_while_its_append_is_gated() {
    let dir = tempfile::tempdir().unwrap();
    let gate = WriteGate::new(|entry| match entry {
        Entry::Message {
            message: AgentMessage::Llm(Message::Assistant(response)),
            ..
        } => response
            .content
            .iter()
            .any(|block| matches!(block, AssistantBlock::Text { text, .. } if text == "done")),
        Entry::NestedToolCall { name, .. } => name == "wait",
        _ => false,
    });
    let mut replies = vec![
        ScriptedTurn::Reply(reply_tool_calls(&[(
            "job",
            "bash",
            json!({
                "command": "while [ ! -f proceed ]; do sleep 0.02; done; echo job-marker",
                "async": true,
            }),
        )])),
        ScriptedTurn::Reply(reply_text("started")),
    ];
    replies.extend(turns());
    let host = common::gated_host(dir.path(), json!([{"id":"m"}]), &gate, replies).await;
    let mut options = options(dir.path(), vec![("wait", json!({}))], true);
    let proceed = options.cwd.join("proceed");
    let policy = Arc::new(HoldWaitReceipt::default());
    options.host.tool_policies.push(policy.clone());
    let session = host.axo.create_session(options).await.unwrap();
    assert_eq!(
        within(session.prompt("start the job")).await.unwrap(),
        RunOutcome::Done
    );

    let mut events = session.subscribe();
    let overlap = async {
        loop {
            if matches!(&*events.recv().await.unwrap(), SessionEvent::Agent(
                AgentEvent::ToolExecutionStart { name, parent_call_id: Some(_), .. }
            ) if name == "wait")
            {
                break;
            }
        }
        std::fs::write(proceed, "").unwrap();
        // Its reservation lets the outer call return, but the policy still holds wait's End.
        gate.entered.notified().await;
        gate.armed.store(true, Ordering::Release);
        policy.persist.notify_one();
        while !session
            .attach(None)
            .await
            .live
            .pending_nested_calls
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
        // Wait's append is now queued behind the final reply. Hold it there until the
        // run has executed its synchronous cleanup and is waiting to append RunEnd.
        gate.release.notify_one();
        gate.entered.notified().await;
        let live = loop {
            let live = session.attach(None).await.live;
            if live.run_id.is_none() {
                break live;
            }
            tokio::task::yield_now().await;
        };
        gate.release.notify_one();
        assert!(live.running);
        live.jobs[0].messages[0].delivery
    };
    let (outcome, delivery) =
        within(async { tokio::join!(session.prompt("wait within outer"), overlap) }).await;
    assert_eq!(outcome.unwrap(), RunOutcome::Done);
    assert_eq!(delivery, MessageDelivery::Reserved);
    within(async {
        loop {
            if matches!(&*events.recv().await.unwrap(), SessionEvent::Agent(
                AgentEvent::ToolExecutionEnd { name, parent_call_id: Some(_), .. }
            ) if name == "wait")
            {
                break;
            }
        }
    })
    .await;
    let lines = journal(&session);
    let receipt = &lines[only(&lines, "delivery receipt", |entry| {
        entry.get("deliveredJobs").is_some()
    })];
    assert!(is_nested(receipt, "wait"));
    assert_eq!(receipt["deliveredJobs"], json!(["bg_1"]));
    assert_eq!(
        session.jobs()[0].messages[0].delivery,
        MessageDelivery::Delivered
    );
    session.shutdown().await;
}

struct BlockBash;

#[async_trait]
impl ToolPolicy for BlockBash {
    async fn before_tool_call(
        &self,
        _: &ToolScope,
        call: BeforeToolCallContext<'_>,
        _: &CancellationToken,
    ) -> Result<ToolCallDecision, ToolError> {
        Ok(if call.tool_call.name == "bash" {
            assert_eq!(call.parent, Some("root"));
            ToolCallDecision::Block {
                reason: Some("bash blocked by host".into()),
                terminate: false,
                origin: None,
            }
        } else {
            ToolCallDecision::Allow
        })
    }
}

#[tokio::test]
async fn nested_calls_obey_host_policy_and_mode() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(dir.path(), turns()).await;
    let mut options = options(
        dir.path(),
        vec![
            ("bash", json!({"command": "touch bash-ran"})),
            ("write", json!({"path": "write-ran", "content": "hello"})),
        ],
        false,
    );
    let cwd = options.cwd.clone();
    options.host.tool_policies.push(Arc::new(BlockBash));
    options.host.definitions.modes.push(ModeSpec {
        name: "plan".into(),
        description: "Inspect files only".into(),
        body: "Do not write files.".into(),
        tools: ToolLimits {
            deny: vec!["write".into()],
            ..Default::default()
        },
        ..Default::default()
    });
    let session = host.axo.create_session(options).await.unwrap();
    session.set_mode(Some("plan")).await.unwrap();
    within(session.prompt("try the tools")).await.unwrap();
    let lines = journal(&session);
    for (tool, text) in [
        ("bash", "bash blocked by host"),
        ("write", "write is unavailable in mode \"plan\""),
    ] {
        let result = &lines[only(&lines, tool, |v| is_nested(v, tool))];
        assert_eq!(result["isError"], true);
        assert!(result["result"].as_str().unwrap().contains(text));
        assert!(!cwd.join(format!("{tool}-ran")).exists());
    }
    session.shutdown().await;
}

#[derive(Default)]
struct Hold {
    started: Notify,
    finish: Notify,
}

#[async_trait]
impl Tool for Hold {
    fn name(&self) -> &str {
        "hold"
    }
    fn description(&self) -> &str {
        "Wait for the test to finish inspecting live state."
    }
    fn parameters(&self) -> &Value {
        &NO_ARGS
    }
    async fn execute(&self, _: ToolContext) -> Result<ToolOutput, ToolError> {
        self.started.notify_one();
        self.finish.notified().await;
        Ok(ToolOutput::default())
    }
}

#[tokio::test]
async fn attach_lists_nested_calls_separately_even_after_the_run_ends() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(dir.path(), turns()).await;
    let mut options = options(dir.path(), vec![("hold", json!({}))], true);
    let hold = Arc::new(Hold::default());
    let fixture = hold.clone();
    options
        .host
        .tools
        .push(HostTool::direct(Arc::new(move |_: &Arc<SessionTools>| {
            fixture.clone() as Arc<dyn Tool>
        })));
    let session = host.axo.create_session(options).await.unwrap();
    assert!(matches!(
        within(session.prompt("start")).await.unwrap(),
        RunOutcome::Done
    ));
    within(hold.started.notified()).await;
    let live = session.attach(None).await.live;
    assert!(!live.running);
    assert!(live.pending_tool_calls.is_empty());
    let [pending] = live.pending_nested_calls.as_slice() else {
        panic!("wrong pending state: {live:?}")
    };
    assert_eq!(pending.parent_call_id, "root");
    assert_eq!(pending.call.id, "root/1");
    assert_eq!(pending.call.name, "hold");
    hold.finish.notify_one();
    common::entry(
        &session,
        |entry| matches!(entry, Entry::NestedToolCall { name, .. } if name == "hold"),
    )
    .await;
    assert!(
        session
            .attach(None)
            .await
            .live
            .pending_nested_calls
            .is_empty()
    );
    session.shutdown().await;
}

#[tokio::test]
async fn a_nested_end_whose_record_fails_still_reaches_subscribers() {
    let dir = tempfile::tempdir().unwrap();
    let gate = WriteGate::new(
        |entry| matches!(entry, Entry::NestedToolCall { name, .. } if name == "read"),
    );
    let host = common::gated_host(dir.path(), json!([{"id":"m"}]), &gate, turns()).await;
    let options = options(
        dir.path(),
        vec![("read", json!({"path": "example.txt"}))],
        false,
    );
    std::fs::write(options.cwd.join("example.txt"), "text").unwrap();
    let session = host.axo.create_session(options).await.unwrap();
    let mut events = session.subscribe();
    let fail = async {
        gate.entered.notified().await;
        gate.fail.store(true, Ordering::Release);
        gate.release.notify_one();
    };
    let (outcome, ()) = within(async { tokio::join!(session.prompt("read nested"), fail) }).await;
    assert_eq!(outcome.unwrap(), RunOutcome::Done);
    let ended = within(async {
        loop {
            if let SessionEvent::Agent(AgentEvent::ToolExecutionEnd {
                name,
                parent_call_id: Some(_),
                ..
            }) = &*events.recv().await.unwrap()
                && name == "read"
            {
                break true;
            }
        }
    })
    .await;
    assert!(ended);
    assert!(
        session
            .attach(None)
            .await
            .live
            .pending_nested_calls
            .is_empty()
    );
    session.shutdown().await;
}
