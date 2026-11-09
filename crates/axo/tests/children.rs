mod common;

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use axo::{
    AgentSpec, Entry, Error, InlineSpec, JobInfo, JobKind, RecoveryPolicy, Session, SessionEvent,
    SessionOptions, SessionTools, SpawnContext, SpawnDecision, SpawnHook, SpawnRequest,
    ToolFactory,
};
use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::testing::{RecordedRequest, reply_text, reply_tool_calls};
use axo_ai::{AssistantBlock, Message, ScriptedTurn, UserBlock};
use common::{host, journal, only, options, within};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

async fn until(mut ready: impl FnMut() -> bool) {
    within(async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

async fn entry(session: &Session, matches: impl Fn(&Entry) -> bool) {
    let mut attached = session.attach(None).await;
    if attached.missed.iter().any(|seq| matches(&seq.entry)) {
        return;
    }
    within(async {
        loop {
            match attached.events.recv().await {
                Ok(SessionEvent::Entry(seq)) if matches(&seq.entry) => break,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("session closed"),
            }
        }
    })
    .await;
}

async fn delivered(session: &Session, job: &str) {
    entry(session, |entry| {
        matches!(entry, Entry::Message { delivered_jobs, .. } if delivered_jobs.iter().any(|id| id == job))
    })
    .await;
}

fn request_text(request: &RecordedRequest) -> String {
    request
        .transcript
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(UserBlock::as_text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn request(task: &str) -> SpawnRequest {
    SpawnRequest {
        task: task.into(),
        model: Some("test/child".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn spawned_child_delivers_once_and_parks() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("noted"))],
    )
    .await;
    host.api
        .lane("child", vec![ScriptedTurn::Reply(reply_text("child done"))]);
    let opts = options(dir.path());
    let cwd = opts.cwd.clone();
    let parent = host.axo.create_session(opts).await.unwrap();
    let mut events = parent.attach(None).await.events;
    let handle = parent
        .spawn(SpawnRequest {
            spec: Some(InlineSpec {
                prompt: "You check things.".into(),
                tools: Some(vec!["read".into()]),
                ..Default::default()
            }),
            ..request("check")
        })
        .await
        .unwrap();
    assert_eq!(handle.job, "agent_1");
    let child_id = handle.session.id().to_owned();
    within(async {
        loop {
            match events.recv().await {
                Ok(SessionEvent::Jobs { jobs }) => {
                    if let Some(JobInfo { id, kind, .. }) =
                        jobs.iter().find(|job| job.id == "agent_1")
                    {
                        assert_eq!(id, "agent_1");
                        assert_eq!(
                            kind,
                            &JobKind::Child {
                                session_id: child_id.clone(),
                                agent: None,
                                model: "test/child".into(),
                                team: None,
                            }
                        );
                        break;
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("session closed"),
            }
        }
    })
    .await;
    delivered(&parent, "agent_1").await;
    entry(&parent, |entry| matches!(entry, Entry::RunEnd { .. })).await;
    let lines = journal(&parent);
    let end = only(&lines, "child result", |v| v["type"] == "job_end");
    assert_eq!(lines[end]["jobId"], "agent_1");
    assert_eq!(lines[end]["text"], "child done");
    assert!(lines[end]["runId"].is_string());
    let delivery = only(&lines, "child delivery", |v| {
        v["deliveredJobs"] == json!(["agent_1"])
    });
    assert!(
        lines[delivery]["message"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[agent_1] subagent\nchild done")
    );

    let requests = host.api.requests();
    let child_request = requests.iter().find(|r| r.model.id == "child").unwrap();
    let system = child_request
        .transcript
        .iter()
        .find_map(|message| match message {
            Message::System(system) => Some(system),
            _ => None,
        })
        .unwrap();
    let sections = system.sections.as_ref().unwrap();
    assert!(sections.contains_key("delegation"));
    assert_eq!(
        sections["system"].as_deref(),
        Some("<system>\nYou check things.\n</system>")
    );
    assert_eq!(
        system
            .tools_added
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read"]
    );
    let bytes = std::fs::read_to_string(handle.session.path()).unwrap();
    let header: Value = serde_json::from_str(bytes.lines().next().unwrap()).unwrap();
    assert_eq!(header["spawn"]["parentSessionId"], parent.id());
    assert_eq!(header["spawn"]["depth"], 1);
    let sessions = host.axo.list_sessions(&cwd).await.unwrap();
    assert!(sessions.iter().any(|s| s.id == child_id));
    assert!(sessions.iter().any(|s| s.id == parent.id()));
    for session in sessions {
        assert_eq!(session.spawn.is_some(), session.id == child_id);
    }

    let weak = Arc::downgrade(&handle.session);
    drop(handle);
    until(|| weak.upgrade().is_none()).await;
    let revived = within(host.axo.session(&child_id)).await.unwrap();
    assert_eq!(revived.id(), child_id);
    assert_eq!(revived.agent().unwrap().depth, 1);
    parent.shutdown().await;
}

#[tokio::test]
async fn undelivered_child_message_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![
            ScriptedTurn::HangUntilCancel,
            ScriptedTurn::Reply(reply_text("after")),
        ],
    )
    .await;
    host.api
        .lane("child", vec![ScriptedTurn::Reply(reply_text("child done"))]);
    let parent = Arc::new(host.axo.create_session(options(dir.path())).await.unwrap());
    let path = parent.path().to_path_buf();
    let prompt = tokio::spawn({
        let parent = parent.clone();
        async move { parent.prompt("hang").await }
    });
    until(|| host.api.requests().len() == 1).await;
    let handle = parent.spawn(request("check")).await.unwrap();
    entry(
        &parent,
        |entry| matches!(entry, Entry::JobEnd { job_id, .. } if job_id == "agent_1"),
    )
    .await;
    parent.shutdown().await;
    within(prompt).await.unwrap().unwrap();
    drop(handle);
    drop(parent);

    let parent = host
        .axo
        .open_session(
            &path,
            SessionOptions {
                recovery: RecoveryPolicy::MarkInterrupted,
                ..options(dir.path())
            },
        )
        .await
        .unwrap();
    within(parent.prompt("next")).await.unwrap();
    let requests = host.api.requests();
    let resumed = requests.iter().rfind(|r| r.model.id == "m").unwrap();
    assert_eq!(request_text(resumed).matches("child done").count(), 1);
    let lines = journal(&parent);
    only(&lines, "child result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_1"
    });
    only(&lines, "child delivery", |v| {
        v["deliveredJobs"] == json!(["agent_1"])
    });
    parent.shutdown().await;
}

struct Gate(Arc<Notify>);

#[async_trait]
impl Tool for Gate {
    fn name(&self) -> &str {
        "gate"
    }

    fn description(&self) -> &str {
        "Waits for the host to release the child."
    }

    fn parameters(&self) -> &Value {
        static PARAMETERS: LazyLock<Value> =
            LazyLock::new(|| json!({"type": "object", "properties": {}}));
        &PARAMETERS
    }

    async fn execute(&self, _cx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.0.notified().await;
        Ok(ToolOutput::default())
    }
}

#[tokio::test]
async fn child_finishing_while_parent_closed_is_reconciled() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ok"))],
    )
    .await;
    host.api.lane(
        "child",
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[("g1", "gate", json!({}))])),
            ScriptedTurn::Reply(reply_text("late result")),
        ],
    );
    let notify = Arc::new(Notify::new());
    let factory: ToolFactory = Arc::new({
        let notify = notify.clone();
        move |_: &Arc<SessionTools>| -> Arc<dyn Tool> { Arc::new(Gate(notify.clone())) }
    });
    let parent = host
        .axo
        .create_session(SessionOptions {
            tools: vec![factory],
            ..options(dir.path())
        })
        .await
        .unwrap();
    let path = parent.path().to_path_buf();
    let handle = parent
        .spawn(SpawnRequest {
            spec: Some(InlineSpec {
                tools: Some(vec!["gate".into()]),
                ..Default::default()
            }),
            ..request("wait for the gate")
        })
        .await
        .unwrap();
    until(|| host.api.requests().iter().any(|r| r.model.id == "child")).await;
    drop(parent);
    notify.notify_one();
    entry(&handle.session, |entry| {
        matches!(entry, Entry::RunEnd { .. })
    })
    .await;
    let parent = within(async {
        loop {
            match host.axo.open_session(&path, options(dir.path())).await {
                Ok(session) => break session,
                Err(Error::SessionBusy { .. }) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("reopen failed: {error}"),
            }
        }
    })
    .await;
    let lines = journal(&parent);
    let end = only(&lines, "reconciled result", |v| v["type"] == "job_end");
    assert_eq!(lines[end]["text"], "late result");
    within(parent.prompt("next")).await.unwrap();
    let lines = journal(&parent);
    only(&lines, "reconciled result", |v| v["type"] == "job_end");
    only(&lines, "child delivery", |v| {
        v["deliveredJobs"] == json!(["agent_1"])
    });
    parent.shutdown().await;
}

#[tokio::test]
async fn parent_shutdown_resumes_child_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ack"))],
    )
    .await;
    host.api.lane(
        "child",
        vec![
            ScriptedTurn::HangUntilCancel,
            ScriptedTurn::Reply(reply_text("resumed result")),
        ],
    );
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let path = parent.path().to_path_buf();
    let handle = parent.spawn(request("check")).await.unwrap();
    until(|| host.api.requests().iter().any(|r| r.model.id == "child")).await;
    parent.shutdown().await;
    let lines = journal(&handle.session);
    assert!(lines.iter().any(|v| v["type"] == "run_start"));
    assert!(!lines.iter().any(|v| v["type"] == "run_end"));
    drop(handle);
    drop(parent);

    let parent = host
        .axo
        .open_session(&path, options(dir.path()))
        .await
        .unwrap();
    delivered(&parent, "agent_1").await;
    entry(&parent, |entry| matches!(entry, Entry::RunEnd { .. })).await;
    let lines = journal(&parent);
    let end = only(&lines, "resumed child result", |v| v["type"] == "job_end");
    assert_eq!(lines[end]["text"], "resumed result");
    only(&lines, "child delivery", |v| {
        v["deliveredJobs"] == json!(["agent_1"])
    });
    only(&lines, "idle delivery run", |v| v["type"] == "run_start");
    assert_eq!(
        host.api
            .requests()
            .iter()
            .filter(|r| r.model.id == "child")
            .count(),
        2
    );
    parent.shutdown().await;
}

struct BlockSpawn;

#[async_trait]
impl SpawnHook for BlockSpawn {
    async fn before_spawn(&self, _spawn: &SpawnContext<'_>) -> SpawnDecision {
        SpawnDecision::Block("no".into())
    }
}

struct ReplaceSpawn;

#[async_trait]
impl SpawnHook for ReplaceSpawn {
    async fn before_spawn(&self, _spawn: &SpawnContext<'_>) -> SpawnDecision {
        SpawnDecision::Replace(AgentSpec {
            body: "Replaced.".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn spawn_policies_block() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ok")); 3],
    )
    .await;
    host.api
        .lane("child", vec![ScriptedTurn::Reply(reply_text("ok")); 3]);
    let blocked = host
        .axo
        .create_session(SessionOptions {
            spawn_hooks: vec![Arc::new(BlockSpawn)],
            ..options(dir.path())
        })
        .await
        .unwrap();
    let error = match blocked.spawn(request("check")).await {
        Err(error) => error,
        Ok(_) => panic!("spawn policy allowed the child"),
    };
    assert!(matches!(&error, Error::SpawnBlocked(_)));
    assert_eq!(error.to_string(), "no");
    assert!(blocked.jobs().is_empty());

    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let handle = parent.spawn(request("check")).await.unwrap();
    let error = match handle.session.spawn(request("nested")).await {
        Err(error) => error,
        Ok(_) => panic!("spawn exceeded the depth limit"),
    };
    assert!(matches!(&error, Error::SpawnBlocked(_)));
    assert!(error.to_string().contains("depth limit 1"));

    let replaced = host
        .axo
        .create_session(SessionOptions {
            spawn_hooks: vec![Arc::new(ReplaceSpawn)],
            ..options(dir.path())
        })
        .await
        .unwrap();
    let replacement = replaced.spawn(request("check")).await.unwrap();
    assert_eq!(replacement.session.agent().unwrap().spec.body, "Replaced.");
    delivered(&parent, "agent_1").await;
    delivered(&replaced, "agent_1").await;
    parent.shutdown().await;
    replaced.shutdown().await;
    blocked.shutdown().await;
}

#[tokio::test]
async fn turn_budget_wraps_up_and_flags_partial() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({"subagents":{"maxTurns":2}}),
        vec![ScriptedTurn::Reply(reply_text("ok"))],
    )
    .await;
    let opts = options(dir.path());
    std::fs::write(opts.cwd.join("a.txt"), "first read").unwrap();
    let mut wrap_up = reply_tool_calls(&[("r3", "read", json!({"path":"c.txt"}))]);
    wrap_up.content.insert(
        0,
        AssistantBlock::Text {
            text: "wrapped up".into(),
            text_signature: None,
        },
    );
    host.api.lane(
        "child",
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[("r1", "read", json!({"path":"a.txt"}))])),
            ScriptedTurn::Reply(reply_tool_calls(&[("r2", "read", json!({"path":"b.txt"}))])),
            ScriptedTurn::Reply(wrap_up),
        ],
    );
    let parent = host.axo.create_session(opts).await.unwrap();
    let handle = parent.spawn(request("read the files")).await.unwrap();
    delivered(&parent, "agent_1").await;
    let lines = journal(&handle.session);
    for call in ["r2", "r3"] {
        let result = only(&lines, call, |v| {
            v["type"] == "message" && v["message"]["toolCallId"] == call
        });
        assert_eq!(lines[result]["message"]["isError"], true);
        assert_eq!(
            lines[result]["message"]["content"][0]["text"],
            "turn budget exhausted"
        );
    }
    let notice = only(&lines, "turn budget notice", |v| {
        v["message"]["customType"] == "turn_budget"
    });
    let notice = lines[notice]["message"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let requests = host.api.requests();
    let child_requests: Vec<_> = requests.iter().filter(|r| r.model.id == "child").collect();
    assert_eq!(child_requests.len(), 3);
    // Custom messages lower to user messages in the model-facing transcript.
    assert!(request_text(child_requests[2]).contains(notice));
    let lines = journal(&parent);
    let end = only(&lines, "partial child result", |v| v["type"] == "job_end");
    assert_eq!(lines[end]["partial"], true);
    assert_eq!(lines[end]["text"], "wrapped up");
    let delivery = only(&lines, "partial child delivery", |v| {
        v["deliveredJobs"] == json!(["agent_1"])
    });
    assert!(
        lines[delivery]["message"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("(partial: turn budget exhausted)")
    );
    parent.shutdown().await;
}

#[tokio::test]
async fn max_concurrent_queues_child_runs() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({"subagents":{"maxConcurrent":1}}),
        vec![
            ScriptedTurn::Reply(reply_text("p1")),
            ScriptedTurn::Reply(reply_text("p2")),
        ],
    )
    .await;
    host.api.lane(
        "child",
        vec![
            ScriptedTurn::HangUntilCancel,
            ScriptedTurn::Reply(reply_text("second")),
        ],
    );
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let first = parent.spawn(request("first")).await.unwrap();
    until(|| host.api.requests().iter().any(|r| r.model.id == "child")).await;
    let second = parent.spawn(request("second")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        host.api
            .requests()
            .iter()
            .filter(|r| r.model.id == "child")
            .count(),
        1
    );
    first.session.abort();
    entry(
        &parent,
        |entry| matches!(entry, Entry::JobEnd { job_id, .. } if job_id == "agent_2"),
    )
    .await;
    assert_eq!(
        host.api
            .requests()
            .iter()
            .filter(|r| r.model.id == "child")
            .count(),
        2
    );
    let lines = journal(&parent);
    let first_end = only(&lines, "first child result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_1"
    });
    assert_eq!(lines[first_end]["text"], "Run stopped before finishing.");
    assert_eq!(lines[first_end]["isError"], true);
    let second_end = only(&lines, "second child result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_2"
    });
    assert_eq!(lines[second_end]["text"], "second");
    drop(second);
    parent.shutdown().await;
}
