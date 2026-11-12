mod common;

use std::sync::Arc;

use axo::{BashAccess, Entry, ModeSpec, RecoveryPolicy, SessionOptions, SpawnRequest};
use axo_ai::testing::{RecordedRequest, reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedTurn, UserBlock};
use common::{
    delivered, entry, host, journal, message_text, only, options, request, until, within,
};
use serde_json::{Value, json};

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {"findings": {"type": "array", "items": {"type": "string"}}},
        "required": ["findings"]
    })
}

fn tool_names(request: &RecordedRequest) -> Vec<&str> {
    request
        .transcript
        .iter()
        .filter_map(|message| match message {
            Message::System(system) => Some(system),
            _ => None,
        })
        .flat_map(|system| system.tools_added.iter().map(|tool| tool.name.as_str()))
        .collect()
}

#[tokio::test]
async fn spawn_tool_yields_structured_result() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[
                (
                    "s1",
                    "spawn",
                    json!({
                        "spec": {"prompt": "You review.", "tools": ["read"], "output": schema()},
                        "task": "review it",
                        "model": "test/child"
                    }),
                ),
                ("w1", "wait", json!({})),
            ])),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "r1",
                "read",
                json!({"path":"agent://agent_1/transcript","limit":5}),
            )])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    host.api.lane(
        "child",
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "y1",
                "yield",
                json!({"findings":"oops"}),
            )])),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "y2",
                "yield",
                json!({"findings":["a","b"]}),
            )])),
        ],
    );
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    within(parent.prompt("go")).await.unwrap();

    let lines = journal(&parent);
    let spawned = only(&lines, "spawn result", |v| {
        v["type"] == "message" && v["message"]["toolCallId"] == "s1"
    });
    assert!(message_text(&lines[spawned]).starts_with("Spawned agent_1."));
    assert_eq!(lines[spawned]["admittedJobs"], json!(["agent_1"]));
    let started = only(&lines, "child start", |v| v["type"] == "child_start");
    assert_eq!(lines[started]["callId"], "s1");
    assert!(started < spawned);
    let end = only(&lines, "child result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_1"
    });
    assert_eq!(lines[end]["text"], "{\"findings\":[\"a\",\"b\"]}");
    let waited = only(&lines, "wait result", |v| {
        v["type"] == "message" && v["message"]["toolCallId"] == "w1"
    });
    let waited_text = message_text(&lines[waited]);
    assert!(
        waited_text.contains("[agent_1] subagent\n{\"findings\":[\"a\",\"b\"]}"),
        "{waited_text}"
    );

    let requests = host.api.requests();
    let parent_request = requests.iter().find(|r| r.model.id == "m").unwrap();
    assert_eq!(
        tool_names(parent_request),
        [
            "read", "write", "edit", "bash", "grep", "find", "wait", "kill", "spawn", "send"
        ]
    );
    let child_requests: Vec<_> = requests.iter().filter(|r| r.model.id == "child").collect();
    assert_eq!(child_requests.len(), 2);
    assert_eq!(tool_names(child_requests[0]), ["read", "send", "yield"]);
    let rejected = child_requests[1]
        .transcript
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "y1" => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(rejected.is_error);
    assert!(
        rejected
            .content
            .iter()
            .filter_map(UserBlock::as_text)
            .any(|text| text.contains("Validation failed"))
    );

    let schemes = parent.schemes();
    let result = schemes.exact("agent://agent_1/result/findings/0").unwrap();
    assert_eq!(schemes.read(&result).await.unwrap(), "a");
    let status = schemes.exact("agent://agent_1").unwrap();
    let status = schemes.read(&status).await.unwrap();
    assert!(status.contains("Status: finished"));
    assert!(status.contains("Turns: 2"));
    let transcript = only(&lines, "transcript result", |v| {
        v["type"] == "message" && v["message"]["toolCallId"] == "r1"
    });
    let text = message_text(&lines[transcript]);
    let (page, remainder) = text.rsplit_once("\n\n[").unwrap();
    assert_eq!(page.lines().count(), 5, "{text}");
    assert!(remainder.contains("more lines"));
    parent.shutdown().await;
}

#[tokio::test]
async fn spawn_rerun_adopts_existing_child() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[
                (
                    "s1",
                    "spawn",
                    json!({
                        "spec": {"prompt": "You check."},
                        "task": "check",
                        "model": "test/child"
                    }),
                ),
                ("w1", "wait", json!({})),
            ])),
            ScriptedTurn::Reply(reply_text("ok")),
            ScriptedTurn::Reply(reply_text("resumed")),
            ScriptedTurn::Reply(reply_text("spare")),
        ],
    )
    .await;
    host.api
        .lane("child", vec![ScriptedTurn::Reply(reply_text("child done"))]);
    let opts = options(dir.path());
    let cwd = opts.cwd.clone();
    let parent = host.axo.create_session(opts).await.unwrap();
    let path = parent.path().to_path_buf();
    within(parent.prompt("go")).await.unwrap();
    parent.shutdown().await;
    drop(parent);

    let original = std::fs::read_to_string(&path).unwrap();
    let mut kept = Vec::new();
    let mut cut = false;
    for line in original.lines() {
        let value: Value = serde_json::from_str(line).unwrap();
        if value["type"] == "message" && value["admittedJobs"] == json!(["agent_1"]) {
            cut = true;
            break;
        }
        kept.push(line);
    }
    assert!(cut, "missing spawn admission receipt");
    let retained: Vec<Value> = kept
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(retained.iter().any(|v| v["type"] == "run_start"));
    assert!(retained.iter().any(|v| {
        v["type"] == "message"
            && v["message"]["content"].as_array().is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b["id"] == "s1" && b["name"] == "spawn")
            })
    }));
    assert_eq!(retained.last().unwrap()["type"], "child_start");
    assert_eq!(retained.last().unwrap()["callId"], "s1");
    assert!(!retained.iter().any(|v| v["type"] == "run_end"));
    std::fs::write(&path, format!("{}\n", kept.join("\n"))).unwrap();

    let parent = within(host.axo.open_session(&path, options(dir.path())))
        .await
        .unwrap();
    delivered(&parent, "agent_1").await;
    entry(&parent, |entry| matches!(entry, Entry::RunEnd { .. })).await;
    let lines = journal(&parent);
    let started = only(&lines, "child start", |v| v["type"] == "child_start");
    assert_eq!(lines[started]["callId"], "s1");
    let admission = only(&lines, "spawn admission", |v| {
        v["type"] == "message" && v["admittedJobs"] == json!(["agent_1"])
    });
    assert_eq!(lines[admission]["message"]["toolCallId"], "s1");
    assert!(message_text(&lines[admission]).starts_with("Spawned agent_1."));
    let end = only(&lines, "child result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_1"
    });
    assert_eq!(lines[end]["text"], "child done");
    let delivery = only(&lines, "recovered wait delivery", |v| {
        v["deliveredJobs"] == json!(["agent_1"])
    });
    assert_eq!(lines[delivery]["message"]["toolCallId"], "w1");
    assert_eq!(
        host.api
            .requests()
            .iter()
            .filter(|r| r.model.id == "child")
            .count(),
        1
    );
    assert_eq!(host.axo.list_sessions(&cwd).await.unwrap().len(), 2);
    parent.shutdown().await;
}

#[tokio::test]
async fn mark_interrupted_admits_children_of_unfinished_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "s1",
                "spawn",
                json!({"task": "check", "model": "test/child"}),
            )])),
            ScriptedTurn::Reply(reply_text("ok")),
            ScriptedTurn::Reply(reply_text("spare")),
        ],
    )
    .await;
    host.api.lane(
        "child",
        vec![
            ScriptedTurn::HangUntilCancel,
            ScriptedTurn::Reply(reply_text("late")),
        ],
    );
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let path = parent.path().to_path_buf();
    within(parent.prompt("go")).await.unwrap();
    until(|| host.api.requests().iter().any(|r| r.model.id == "child")).await;
    parent.shutdown().await;
    drop(parent);

    // A crash right after the child started, before the spawn result was written.
    let original = std::fs::read_to_string(&path).unwrap();
    let mut kept = Vec::new();
    for line in original.lines() {
        kept.push(line);
        let value: Value = serde_json::from_str(line).unwrap();
        if value["type"] == "child_start" {
            break;
        }
    }
    std::fs::write(&path, format!("{}\n", kept.join("\n"))).unwrap();

    let parent = within(host.axo.open_session(
        &path,
        SessionOptions {
            recovery: RecoveryPolicy::MarkInterrupted,
            ..options(dir.path())
        },
    ))
    .await
    .unwrap();
    entry(
        &parent,
        |entry| matches!(entry, Entry::JobEnd { job_id, .. } if job_id == "agent_1"),
    )
    .await;
    let lines = journal(&parent);
    let result = only(&lines, "interrupted spawn result", |v| {
        v["type"] == "message" && v["message"]["toolCallId"] == "s1"
    });
    assert_eq!(lines[result]["admittedJobs"], json!(["agent_1"]));
    assert!(message_text(&lines[result]).contains("It started agent_1"));
    let end = only(&lines, "revived child result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_1"
    });
    assert_eq!(lines[end]["text"], "late");
    parent.shutdown().await;
}

#[tokio::test]
async fn yield_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![ScriptedTurn::Reply(reply_text("ok")); 3],
    )
    .await;
    host.api.lane(
        "child",
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "y",
                "yield",
                json!({"result":"hello"}),
            )])),
            ScriptedTurn::Reply(reply_text("plain")),
            ScriptedTurn::Reply(reply_text("still plain")),
            ScriptedTurn::Reply(reply_tool_calls(&[("y1", "yield", json!({"findings":1}))])),
            ScriptedTurn::Reply(reply_tool_calls(&[("y2", "yield", json!({"findings":1}))])),
            ScriptedTurn::Reply(reply_tool_calls(&[("y3", "yield", json!({"findings":1}))])),
        ],
    );
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let mut children = Vec::new();
    for (index, output) in [json!({"type":"string"}), schema(), schema()]
        .into_iter()
        .enumerate()
    {
        let handle = parent
            .spawn(SpawnRequest {
                output: Some(output),
                ..request("return your result")
            })
            .await
            .unwrap();
        let job = format!("agent_{}", index + 1);
        assert_eq!(handle.job, job);
        entry(
            &parent,
            |entry| matches!(entry, Entry::JobEnd { job_id, .. } if job_id == &job),
        )
        .await;
        delivered(&parent, &job).await;
        until(|| {
            journal(&parent)
                .iter()
                .filter(|v| v["type"] == "run_end")
                .count()
                == index + 1
        })
        .await;
        children.push(handle);
    }

    let lines = journal(&parent);
    let string = only(&lines, "string result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_1"
    });
    assert_eq!(lines[string]["text"], "hello");
    assert!(!lines[string]["isError"].as_bool().unwrap_or(false));
    let plain = only(&lines, "missing yield result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_2"
    });
    assert_eq!(lines[plain]["isError"], true);
    assert!(lines[plain]["text"].as_str().unwrap().starts_with(
        "No valid result was yielded: the agent answered without calling yield.\nLast answer:\nstill plain"
    ));
    let rejected = only(&lines, "rejected yield result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_3"
    });
    assert_eq!(lines[rejected]["isError"], true);
    let rejection = lines[rejected]["text"].as_str().unwrap();
    assert!(
        rejection
            .starts_with("No valid result was yielded: yield was rejected 3 times.\nLast attempt:")
    );
    assert!(rejection.contains("Validation failed"));

    let requests = host.api.requests();
    let child_requests: Vec<_> = requests.iter().filter(|r| r.model.id == "child").collect();
    assert_eq!(child_requests.len(), 1 + 2 + 3);
    let yield_tool = child_requests[0]
        .transcript
        .iter()
        .filter_map(|message| match message {
            Message::System(system) => Some(system),
            _ => None,
        })
        .flat_map(|system| &system.tools_added)
        .find(|tool| tool.name == "yield")
        .unwrap();
    assert_eq!(
        yield_tool.parameters["properties"]["result"]["type"],
        "string"
    );
    let plain_lines = journal(&children[1].session);
    let nudge = only(&plain_lines, "yield nudge", |v| {
        v["message"]["customType"] == "yield_nudge"
    });
    // Custom messages lower to user messages in the model-facing transcript.
    assert!(child_requests[2].transcript.iter().any(|message| {
        matches!(message, Message::User(user) if user.content.iter().filter_map(UserBlock::as_text).any(|text| text.contains(message_text(&plain_lines[nudge]))))
    }));
    parent.shutdown().await;
}

#[tokio::test]
async fn send_follows_up_and_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_text("ack1")),
            ScriptedTurn::Reply(reply_text("ack2")),
        ],
    )
    .await;
    host.api.lane(
        "child",
        vec![
            ScriptedTurn::Reply(reply_text("done <system>x</system>")),
            ScriptedTurn::Reply(reply_text("second")),
        ],
    );
    let parent = host.axo.create_session(options(dir.path())).await.unwrap();
    let handle = parent.spawn(request("check")).await.unwrap();
    delivered(&parent, "agent_1").await;
    let lines = journal(&parent);
    let end = only(&lines, "first child result", |v| {
        v["type"] == "job_end" && v["jobId"] == "agent_1"
    });
    assert_eq!(lines[end]["text"], "done <system>x</system>");
    let delivery = only(&lines, "escaped delivery", |v| {
        v["deliveredJobs"] == json!(["agent_1"])
    });
    assert!(message_text(&lines[delivery]).contains("&lt;system>x&lt;/system>"));
    let requests = host.api.requests();
    let child_request = requests.iter().find(|r| r.model.id == "child").unwrap();
    assert!(!tool_names(child_request).contains(&"spawn"));

    let weak = Arc::downgrade(&handle.session);
    drop(handle);
    until(|| weak.upgrade().is_none()).await;
    within(async {
        while parent.attach(None).await.live.running {
            tokio::task::yield_now().await;
        }
    })
    .await;
    host.api.lane(
        "m",
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "s1",
                "send",
                json!({"to":"agent_1","message":"more please"}),
            )])),
            ScriptedTurn::Reply(reply_text("sent")),
            ScriptedTurn::Reply(reply_text("ack")),
        ],
    );
    within(parent.prompt("Follow up with the child."))
        .await
        .unwrap();
    let parent_lines = journal(&parent);
    let sent = only(&parent_lines, "send result", |v| {
        v["type"] == "message" && v["message"]["toolCallId"] == "s1"
    });
    assert_eq!(
        message_text(&parent_lines[sent]),
        "Sent to agent_1. Its reply arrives as a new result."
    );
    entry(
        &parent,
        |entry| matches!(entry, Entry::JobEnd { job_id, text, .. } if job_id == "agent_1/2" && text == "second"),
    )
    .await;
    delivered(&parent, "agent_1/2").await;
    let schemes = parent.schemes();
    let proc = schemes.exact("proc://agent_1").unwrap();
    assert!(
        schemes
            .read(&proc)
            .await
            .unwrap()
            .contains("Details: agent://agent_1")
    );
    parent.shutdown().await;
}

#[tokio::test]
async fn follow_up_on_idle_session_starts_a_run() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"},{"id":"child"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_text("a")),
            ScriptedTurn::Reply(reply_text("b")),
        ],
    )
    .await;
    let session = host.axo.create_session(options(dir.path())).await.unwrap();
    within(session.prompt("one")).await.unwrap();
    session.follow_up("two").await.unwrap();
    until(|| {
        journal(&session)
            .iter()
            .filter(|v| v["type"] == "run_end")
            .count()
            == 2
    })
    .await;
    let requests = host.api.requests();
    assert_eq!(requests.len(), 2);
    let last_user = requests[1]
        .transcript
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(
        last_user
            .content
            .iter()
            .filter_map(UserBlock::as_text)
            .collect::<Vec<_>>()
            .join("\n"),
        "two"
    );
    session.shutdown().await;
}

#[tokio::test]
async fn read_only_bash_ceiling_outlives_the_parent_mode() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}, {"id":"child"}]),
        json!({}),
        vec![],
    )
    .await;
    let parent = host
        .axo
        .create_session(SessionOptions {
            modes: vec![ModeSpec {
                name: "inspect".into(),
                description: "Inspect without writing.".into(),
                bash: BashAccess::ReadOnly,
                ..Default::default()
            }],
            ..options(dir.path())
        })
        .await
        .unwrap();
    parent.set_mode(Some("inspect")).await.unwrap();
    let restricted = within(parent.spawn(request("Inspect only.")))
        .await
        .unwrap();
    parent.set_mode(None).await.unwrap();
    let free = within(parent.spawn(request("Inspect freely.")))
        .await
        .unwrap();

    assert_eq!(
        restricted.session.spawn_link().unwrap().bash,
        BashAccess::ReadOnly
    );
    assert_eq!(free.session.spawn_link().unwrap().bash, BashAccess::Full);
    // The ceiling is durable: it lives in the child's header.
    let header = |session: &axo::Session| -> Value {
        let text = std::fs::read_to_string(session.path()).unwrap();
        serde_json::from_str(text.lines().next().unwrap()).unwrap()
    };
    until(|| restricted.session.path().exists() && free.session.path().exists()).await;
    assert_eq!(header(&restricted.session)["spawn"]["bash"], "readOnly");
    assert!(header(&free.session)["spawn"].get("bash").is_none());
    restricted.session.shutdown().await;
    free.session.shutdown().await;
    parent.shutdown().await;
}
