mod common;

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use axo::{
    AgentSpec, Entry, Error, InlineSpec, JobInfo, JobKind, JobStatus, RecoveryPolicy, SessionEvent,
    SessionOptions, SessionTools, SpawnContext, SpawnDecision, SpawnHook, SpawnRequest,
    ToolFactory,
};
use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::testing::{RecordedRequest, reply_text, reply_tool_calls};
use axo_ai::{AssistantBlock, Message, ScriptedTurn, Usage, UserBlock};
use common::{delivered, entry, host, journal, only, options, request, run_ends, until, within};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

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
                Ok(SessionEvent::Job(JobInfo { id, kind, .. })) if id == "agent_1" => {
                    let JobKind::Child {
                        session_id,
                        path,
                        depth,
                        agent,
                        model,
                        team,
                        resident,
                        progress,
                        unread_mail,
                        tree_usage: _,
                    } = kind
                    else {
                        panic!("expected a child job");
                    };
                    assert_eq!(session_id, child_id);
                    assert_eq!(path, handle.session.path());
                    assert_eq!(depth, 1);
                    assert_eq!(agent, None);
                    assert_eq!(model, "test/child");
                    assert_eq!(team, None);
                    assert!(resident);
                    assert_eq!(unread_mail, 0);
                    if let Some(progress) = progress {
                        assert!(progress.turns <= progress.max_turns + 1);
                    }
                    break;
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
    let keys: Vec<&str> = sections.keys().map(String::as_str).collect();
    assert_eq!(&keys[..3], ["delegation", "system", "agent"]);
    assert!(
        sections["system"]
            .as_deref()
            .unwrap()
            .starts_with("<system>\nYou are a coding agent running in ")
    );
    assert_eq!(
        sections["agent"].as_deref(),
        Some("<agent>\nYou check things.\n</agent>")
    );
    assert_eq!(
        system
            .tools_added
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read", "send"]
    );
    let bytes = std::fs::read_to_string(handle.session.path()).unwrap();
    let header: Value = serde_json::from_str(bytes.lines().next().unwrap()).unwrap();
    assert_eq!(header["spawn"]["parentSessionId"], parent.id());
    assert_eq!(header["spawn"]["depth"], 1);
    assert!(
        bytes
            .lines()
            .skip(1)
            .all(|line| { serde_json::from_str::<Value>(line).unwrap()["type"] != "agent_change" }),
        "a child's persona is its spawn link, never an agent_change"
    );
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
    assert_eq!(revived.spawn_link().unwrap().depth, 1);
    parent.shutdown().await;
}

#[tokio::test]
async fn opening_child_with_closed_parent_names_root() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("noted"))],
    )
    .await;
    host.api
        .lane("child", vec![ScriptedTurn::Reply(reply_text("done"))]);
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let root_path = parent.path().to_path_buf();
    let handle = parent.spawn(request("check")).await.unwrap();
    let child_id = handle.session.id().to_owned();
    let child_path = handle.session.path().to_path_buf();
    delivered(&parent, &handle.job).await;
    entry(&parent, |entry| matches!(entry, Entry::RunEnd { .. })).await;
    parent.shutdown().await;
    drop(handle);
    drop(parent);

    match host
        .axo
        .open_session(&child_path, options(dir.path()))
        .await
    {
        Err(Error::ChildSession { path, root }) => {
            assert_eq!(path, child_path);
            assert_eq!(root, Some(root_path.clone()));
        }
        Err(error) => panic!("unexpected error: {error}"),
        Ok(_) => panic!("child opened as a root"),
    }
    let parent = host
        .axo
        .open_session(&root_path, options(dir.path()))
        .await
        .unwrap();
    let child = within(host.axo.session(&child_id)).await.unwrap();
    assert_eq!(child.id(), child_id);
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
            tools: vec![axo::HostTool::direct(factory)],
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
    // Reconciliation runs in the background after open returns.
    entry(&parent, |entry| matches!(entry, Entry::JobEnd { .. })).await;
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

#[tokio::test]
async fn reopen_survives_child_that_cannot_revive() {
    let dir = tempfile::tempdir().unwrap();
    let first = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ack"))],
    )
    .await;
    first.api.lane("child", vec![ScriptedTurn::HangUntilCancel]);
    let parent = first.axo.create_session(options(dir.path())).await.unwrap();
    let path = parent.path().to_path_buf();
    let handle = parent.spawn(request("check")).await.unwrap();
    until(|| first.api.requests().iter().any(|r| r.model.id == "child")).await;
    parent.shutdown().await;
    drop(handle);
    drop(parent);

    // The child's model is gone, so reviving its unfinished run fails.
    let second = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ok"))],
    )
    .await;
    let parent = second
        .axo
        .open_session(&path, options(dir.path()))
        .await
        .unwrap();
    within(parent.prompt("next")).await.unwrap();
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
    assert_eq!(
        replacement.session.spawn_link().unwrap().spec.body,
        "Replaced."
    );
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
async fn queued_children_publish_running_and_per_run_usage() {
    let dir = tempfile::tempdir().unwrap();
    let mut parent_reply = reply_text("ack");
    parent_reply.usage = Usage {
        input: 101,
        output: 13,
        total_tokens: 114,
        ..Default::default()
    };
    let mut parent_tool = reply_tool_calls(&[("r1", "read", json!({"path":"missing.txt"}))]);
    parent_tool.usage = Usage {
        input: 31,
        output: 7,
        total_tokens: 38,
        ..Default::default()
    };
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({"subagents":{"maxConcurrent":1}}),
        vec![
            ScriptedTurn::Reply(parent_tool.clone()),
            ScriptedTurn::Reply(parent_reply.clone()),
            ScriptedTurn::Reply(parent_tool),
            ScriptedTurn::Reply(parent_reply),
        ],
    )
    .await;
    let mut gate = reply_tool_calls(&[("g1", "gate", json!({}))]);
    gate.usage = Usage {
        input: 17,
        output: 3,
        total_tokens: 20,
        ..Default::default()
    };
    // The second child reads before blocking, so its row shows the live tool.
    let mut read_gate = reply_tool_calls(&[
        ("r0", "read", json!({"path":"missing.txt"})),
        ("g1", "gate", json!({})),
    ]);
    read_gate.usage = gate.usage.clone();
    let mut done = reply_text("done");
    done.usage = Usage {
        input: 23,
        output: 5,
        total_tokens: 28,
        ..Default::default()
    };
    host.api.lane(
        "child",
        vec![
            ScriptedTurn::Reply(gate.clone()),
            ScriptedTurn::Reply(done.clone()),
            ScriptedTurn::Reply(read_gate),
            ScriptedTurn::Reply(done),
        ],
    );
    let release = Arc::new(Notify::new());
    let factory: ToolFactory = Arc::new({
        let release = release.clone();
        move |_: &Arc<SessionTools>| -> Arc<dyn Tool> { Arc::new(Gate(release.clone())) }
    });
    let parent = host
        .axo
        .create_session(SessionOptions {
            tools: vec![axo::HostTool::direct(factory)],
            ..options(dir.path())
        })
        .await
        .unwrap();
    let spawn = |task| SpawnRequest {
        spec: Some(InlineSpec {
            tools: Some(vec!["gate".into(), "read".into()]),
            ..Default::default()
        }),
        ..request(task)
    };
    let first = parent.spawn(spawn("first")).await.unwrap();
    until(|| host.api.requests().iter().any(|r| r.model.id == "child")).await;
    let second = parent.spawn(spawn("second")).await.unwrap();
    let queued = parent
        .jobs()
        .into_iter()
        .find(|job| job.id == second.job)
        .unwrap();
    assert_eq!(queued.status, JobStatus::Queued);
    assert!(matches!(
        queued.kind,
        JobKind::Child {
            resident: true,
            depth: 1,
            ..
        }
    ));
    let mut events = parent.attach(None).await.events;
    release.notify_one();
    let mut running = false;
    within(async {
        loop {
            match events.recv().await {
                Ok(SessionEvent::Job(job)) if job.id == second.job => {
                    running |= job.status == JobStatus::Running;
                    let JobKind::Child {
                        progress: Some(progress),
                        ..
                    } = job.kind
                    else {
                        continue;
                    };
                    // The first live tool row is the read; the gate keeps the run open.
                    if !progress.tools.is_empty() {
                        assert!(running);
                        assert_eq!(progress.tools, ["read"]);
                        break;
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("session closed"),
            }
        }
    })
    .await;
    // The second child's first turn also blocks, so Running cannot race its completion.
    release.notify_one();
    for child in [&first, &second] {
        delivered(&parent, &child.job).await;
        let child_lines = journal(&child.session);
        let mut expected = Usage::default();
        let mut turns = 0;
        for line in &child_lines {
            if line["type"] == "message" && line["message"]["role"] == "assistant" {
                let usage: Usage =
                    serde_json::from_value(line["message"]["usage"].clone()).unwrap();
                expected += &usage;
                turns += 1;
            }
        }
        assert_eq!(turns, 2);
        assert_eq!(expected.total_tokens, 48);
        let lines = journal(&parent);
        let end = only(&lines, "child usage report", |v| {
            v["type"] == "job_end" && v["jobId"] == child.job
        });
        assert_eq!(
            lines[end]["usage"],
            serde_json::to_value(&expected).unwrap()
        );
    }
    // After the run, no tool is executing on either row.
    for child in [&first, &second] {
        until(|| {
            parent.jobs().iter().any(|job| {
                job.id == child.job
                    && matches!(
                        &job.kind,
                        JobKind::Child { progress: Some(progress), .. } if progress.tools.is_empty()
                    )
            })
        })
        .await;
    }
    // Wait for the run consuming the final child result, not an earlier idle wake.
    until(|| {
        let lines = journal(&parent);
        let delivered = lines
            .iter()
            .rposition(|v| v["deliveredJobs"].is_array())
            .unwrap();
        lines
            .iter()
            .enumerate()
            .any(|(index, v)| index > delivered && v["type"] == "run_end")
    })
    .await;
    let mut expected = Usage::default();
    let mut runs = 0;
    for line in journal(&parent) {
        match line["type"].as_str() {
            Some("run_start") => expected = Usage::default(),
            Some("message") if line["message"]["role"] == "assistant" => {
                let usage: Usage =
                    serde_json::from_value(line["message"]["usage"].clone()).unwrap();
                expected += &usage;
            }
            Some("run_end") => {
                assert!(expected.total_tokens >= 152);
                assert_eq!(line["usage"], serde_json::to_value(&expected).unwrap());
                runs += 1;
            }
            _ => {}
        }
    }
    assert!(runs > 0);
    parent.shutdown().await;
}

fn costed(text: &str, total: u64) -> ScriptedTurn {
    let mut reply = reply_text(text);
    reply.usage = Usage {
        input: total - 1,
        output: 1,
        total_tokens: total,
        ..Default::default()
    };
    ScriptedTurn::Reply(reply)
}

/// Σ `Entry::usage` over a session's full journal, maintenance included.
async fn own_usage(axo: &axo::Axo, path: &std::path::Path) -> Usage {
    let (_, entries) = axo.open_session_read(path).await.unwrap();
    let mut usage = Usage::default();
    for seq in &entries {
        if let Some(spent) = seq.entry.usage() {
            usage += spent;
        }
    }
    usage
}

fn sum(parts: &[&Usage]) -> Usage {
    let mut total = Usage::default();
    for part in parts {
        total += part;
    }
    total
}

#[tokio::test]
async fn tree_usage_counts_descendants_and_child_compaction_once() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([
            {"id":"m"},
            {"id":"child", "contextWindow":100_000, "maxTokens":4096},
            {"id":"grand"},
        ]),
        json!({
            "subagents": {"maxDepth": 2},
            "compaction": {
                "enabled": false,
                "maxContext": null,
                "reserveTokens": 1024,
                "keepRecentTokens": 0,
                "summaryRequest": "separate",
            },
        }),
        // One idle wake per delivered child run.
        vec![costed("ack one", 100), costed("ack two", 200)],
    )
    .await;
    // The child's summaries come from its own model lane (`Selection` of the compacting
    // session). With `keepRecentTokens: 0` the cut lands on "child ack", splitting its turn:
    // one request summarizes the history, a second the split turn's prefix (the grandchild
    // result). Each reply must be smaller than what it replaces, so the history is padded.
    // The measured turns are realistic: compaction must shrink the context below the last
    // measured total, and the estimate after it still carries the replayed system prompt
    // and tool definitions, which a toy baseline would not cover.
    host.api.lane(
        "child",
        vec![
            costed(&"child findings ".repeat(20), 10_000),
            // The idle wake consuming the grandchild's result.
            costed("child ack", 20_000),
            costed("brief", 50),
            costed("gist", 40),
        ],
    );
    host.api.lane("grand", vec![costed("grand done", 10_000)]);
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let path = parent.path().to_path_buf();
    let mut events = parent.attach(None).await.events;

    let child = parent.spawn(request("check")).await.unwrap();
    delivered(&parent, &child.job).await;
    until(|| run_ends(&parent) == 1).await;
    let grand = child
        .session
        .spawn(SpawnRequest {
            model: Some("test/grand".into()),
            ..request("dig deeper")
        })
        .await
        .unwrap();
    assert_eq!(grand.session.spawn_link().unwrap().depth, 2);
    delivered(&child.session, &grand.job).await;
    until(|| run_ends(&child.session) == 2).await;
    until(|| run_ends(&parent) == 2).await;
    within(child.session.compact(None)).await.unwrap();

    let parent_own = own_usage(&host.axo, parent.path()).await;
    let child_own = own_usage(&host.axo, child.session.path()).await;
    let grand_own = own_usage(&host.axo, grand.session.path()).await;
    assert_eq!(parent_own.total_tokens, 300);
    assert_eq!(grand_own.total_tokens, 10_000);
    // The child's own spend includes its compaction requests, which `JobEnd` excludes.
    let child_lines = journal(&child.session);
    let compactions: Vec<_> = child_lines
        .iter()
        .filter(|v| v["type"] == "compaction")
        .collect();
    assert_eq!(compactions.len(), 1, "{child_lines:#?}");
    // Both summary requests succeed on the first attempt: no failed-attempt usage.
    assert!(
        !child_lines.iter().any(|v| v["type"] == "usage"),
        "{child_lines:#?}"
    );
    let compaction: Usage = serde_json::from_value(compactions[0]["usage"].clone()).unwrap();
    assert_eq!(compaction.total_tokens, 90);
    assert_eq!(child_own.total_tokens, 30_000 + 90);
    // Each child run reports `JobEnd` (`agent_1`, then `agent_1/2`), never maintenance.
    let run_ref = format!("{}/", child.job);
    let mut reported = Usage::default();
    let mut runs = 0;
    for line in journal(&parent).iter().filter(|v| {
        v["type"] == "job_end"
            && v["jobId"]
                .as_str()
                .is_some_and(|id| id == child.job || id.starts_with(&run_ref))
    }) {
        let usage: Usage = serde_json::from_value(line["usage"].clone()).unwrap();
        reported += &usage;
        runs += 1;
    }
    assert_eq!(runs, 2);
    assert_eq!(
        reported.total_tokens, 30_000,
        "job_end usage omits maintenance"
    );

    let child_tree = sum(&[&child_own, &grand_own]);
    let expected = sum(&[&parent_own, &child_tree]);
    until(|| parent.tree_usage() == expected).await;
    assert_eq!(child.session.tree_usage(), child_tree);
    assert_eq!(grand.session.tree_usage(), grand_own);
    assert_eq!(child.session.attach(None).await.live.tree_usage, child_tree);
    let row_usage = |session: &axo::Session, job: &str| {
        session
            .jobs()
            .into_iter()
            .find(|row| row.id == job)
            .and_then(|row| match row.kind {
                JobKind::Child { tree_usage, .. } => Some(tree_usage),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(row_usage(&parent, &child.job), child_tree);
    assert_eq!(row_usage(&child.session, &grand.job), grand_own);
    // Emitted totals only grow, never past the full tree, and end at it.
    within(async {
        let mut last: Option<Usage> = None;
        loop {
            match events.recv().await {
                Ok(SessionEvent::TreeUsage { usage }) => {
                    assert!(usage.total_tokens <= expected.total_tokens, "{usage:?}");
                    if let Some(last) = &last {
                        assert_ne!(last, &usage, "unchanged totals are not re-emitted");
                        assert!(usage.total_tokens >= last.total_tokens);
                    }
                    if usage == expected {
                        break;
                    }
                    last = Some(usage);
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("session closed"),
            }
        }
    })
    .await;
    assert_eq!(parent.attach(None).await.live.tree_usage, expected);

    parent.shutdown().await;
    drop(grand);
    drop(child);
    drop(parent);
    let parent = host
        .axo
        .open_session(&path, options(dir.path()))
        .await
        .unwrap();
    // Seeded recursively from the parked journals: no compaction lost, nothing doubled.
    assert_eq!(parent.attach(None).await.live.tree_usage, expected);
    assert_eq!(parent.tree_usage(), expected);
    let child_row = parent
        .jobs()
        .into_iter()
        .find(|row| matches!(row.kind, JobKind::Child { .. }))
        .unwrap();
    let JobKind::Child { tree_usage, .. } = child_row.kind else {
        unreachable!()
    };
    assert_eq!(tree_usage, child_tree);
    parent.shutdown().await;
}

#[tokio::test]
async fn kill_before_child_run_starts_aborts_it() {
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
    host.api.lane("child", vec![ScriptedTurn::HangUntilCancel]);
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let held = parent.spawn(request("hold")).await.unwrap();
    until(|| host.api.requests().iter().any(|r| r.model.id == "child")).await;
    // Claimed, but waiting for the only run slot: no agent run exists yet.
    let queued = parent.spawn(request("queued")).await.unwrap();
    assert!(parent.kill_job(&queued.job).stopping);
    assert!(parent.kill_job(&held.job).stopping);
    entry(&queued.session, |entry| {
        matches!(entry, Entry::RunEnd { .. })
    })
    .await;
    entry(
        &parent,
        |entry| matches!(entry, Entry::JobEnd { job_id, .. } if job_id == "agent_2"),
    )
    .await;
    let lines = journal(&parent);
    let end = only(&lines, "queued child result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_2"
    });
    assert_eq!(lines[end]["text"], "Run stopped before finishing.");
    assert_eq!(
        host.api
            .requests()
            .iter()
            .filter(|r| r.model.id == "child")
            .count(),
        1,
        "the stopped child never reached the model"
    );
    parent.shutdown().await;
}

#[tokio::test]
async fn steer_right_after_spawn_keeps_the_task_first() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ok")); 8],
    )
    .await;
    host.api
        .lane("child", vec![ScriptedTurn::Reply(reply_text("done")); 4]);
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let handle = parent.spawn(request("the task")).await.unwrap();
    let sent = parent.send(&handle.job, "extra", true).await.unwrap();
    assert_eq!(sent.session_id, handle.session.id());
    assert_eq!(sent.kind, axo::SendKind::Steer);
    let axo::SendIdentity::Submission(id) = sent.receipt else {
        panic!("child steer must identify its durable submission");
    };
    let lines = journal(&handle.session);
    let submitted = only(&lines, "routed steer", |v| {
        v["type"] == "submission" && v["submissionId"] == id
    });
    assert_eq!(lines[submitted]["provenance"]["sessionId"], parent.id());
    assert_eq!(lines[submitted]["provenance"]["route"], "steer");
    assert!(lines[submitted].get("sender").is_none());
    delivered(&parent, "agent_1").await;
    let requests = host.api.requests();
    let first = requests
        .iter()
        .find(|r| r.model.id == "child")
        .expect("a child request");
    assert!(request_text(first).contains("the task"));
    let sent = parent.send(&handle.job, "follow-up", false).await.unwrap();
    assert_eq!(sent.kind, axo::SendKind::FollowUp);
    let axo::SendIdentity::Submission(id) = sent.receipt else {
        panic!("child follow-up must identify its durable submission");
    };
    let lines = journal(&handle.session);
    let submitted = only(&lines, "routed follow-up", |v| v["submissionId"] == id);
    assert_eq!(lines[submitted]["provenance"]["sessionId"], parent.id());
    assert_eq!(lines[submitted]["provenance"]["route"], "followUp");
    assert!(lines[submitted].get("sender").is_none());
    let sent = handle
        .session
        .send("parent", "a note", false)
        .await
        .unwrap();
    assert_eq!(sent.session_id, parent.id());
    assert_eq!(sent.kind, axo::SendKind::Note);
    let axo::SendIdentity::Message(id) = sent.receipt else {
        panic!("parent note must identify its durable result message");
    };
    let lines = journal(&parent);
    let note = only(&lines, "routed note", |v| {
        v["type"] == "job_end" && v["jobId"] == id
    });
    assert_eq!(lines[note]["text"], "a note");
    assert_eq!(lines[note]["note"], true);
    parent.shutdown().await;
}
