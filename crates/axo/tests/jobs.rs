//! Background jobs: commands whose processes outlive them stay supervised and stoppable,
//! and promoted commands deliver their results exactly once.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axo::store::{Entry, SeqEntry};
use axo::{Axo, Session, SessionEvent};
use axo_agent::{AgentEvent, RunOutcome};
use axo_ai::testing::{RecordedRequest, reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedApi, ScriptedTurn, UserBlock};
use common::{axo, bash_options, dead, tool_text, within};
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// An `Axo` streaming from `turns`, plus the API that records every request.
async fn scripted(dir: &Path, turns: Vec<ScriptedTurn>) -> (Axo, Arc<ScriptedApi>) {
    let host = common::host(dir, json!([{"id":"m"}]), json!({}), turns).await;
    (host.axo, host.api)
}

/// The text of every user message in `request`, in order.
fn user_texts(request: &RecordedRequest) -> Vec<String> {
    request
        .transcript
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect()
}

/// The session file's entries as raw JSON, header excluded.
fn journal(session: &Session) -> Vec<Value> {
    std::fs::read_to_string(session.path())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|value| value["type"] != "session")
        .collect()
}

/// The index of the only entry matching `pred`.
fn only(lines: &[Value], what: &str, pred: impl Fn(&Value) -> bool) -> usize {
    let found: Vec<usize> = (0..lines.len()).filter(|&i| pred(&lines[i])).collect();
    assert_eq!(found.len(), 1, "expected exactly one {what}: {lines:#?}");
    found[0]
}

fn count(lines: &[Value], kind: &str) -> usize {
    lines.iter().filter(|value| value["type"] == kind).count()
}

/// Checks `bg_1`'s `job_start`, its admitting tool result and its `job_end`, each written
/// once and in that order, and returns the index of the `job_end`.
fn assert_job_journaled(
    lines: &[Value],
    session: &Session,
    call_id: &str,
    command: &str,
    output: &str,
) -> usize {
    let start = only(lines, "job_start", |v| v["type"] == "job_start");
    assert_eq!(lines[start]["jobId"], "bg_1");
    assert_eq!(lines[start]["sessionId"], session.id());
    assert_eq!(lines[start]["callId"], call_id);
    assert_eq!(lines[start]["command"], command);
    assert!(lines[start]["artifact"].is_u64(), "{:#?}", lines[start]);

    let admitted = only(lines, "admitting entry", |v| {
        v.get("admittedJobs").is_some()
    });
    assert_eq!(lines[admitted]["admittedJobs"], json!(["bg_1"]));
    assert_eq!(lines[admitted]["message"]["role"], "toolResult");
    assert_eq!(lines[admitted]["message"]["toolCallId"], call_id);

    let end = only(lines, "job_end", |v| v["type"] == "job_end");
    assert_eq!(lines[end]["jobId"], "bg_1");
    assert_eq!(lines[end]["isError"], false);
    let text = lines[end]["text"].as_str().unwrap();
    assert!(text.contains(output), "{text}");

    assert!(start < admitted && admitted < end, "{lines:#?}");
    end
}

/// Checks that `bg_1` was delivered once, by a `job_result` custom message after the
/// job ended and after the prompt's own run settled, and returns its index.
fn assert_delivered_by_message(lines: &[Value], end: usize, output: &str) -> usize {
    let delivered = only(lines, "delivering entry", |v| {
        v.get("deliveredJobs").is_some()
    });
    let entry = &lines[delivered];
    assert_eq!(entry["type"], "message");
    assert_eq!(entry["deliveredJobs"], json!(["bg_1"]));
    assert!(entry.get("submissionId").is_none(), "{entry:#}");
    assert_eq!(entry["message"]["role"], "custom");
    assert_eq!(entry["message"]["customType"], "job_result");
    assert_eq!(entry["message"]["details"]["jobs"], json!(["bg_1"]));
    let text = entry["message"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Background job results:"), "{text}");
    assert!(text.contains(output), "{text}");
    assert!(end < delivered, "{lines:#?}");

    // Delivered by a run of its own, started after the prompt's run settled.
    let first_end = lines.iter().position(|v| v["type"] == "run_end").unwrap();
    assert!(first_end < delivered, "{lines:#?}");
    assert_eq!(count(lines, "run_end"), 2, "{lines:#?}");
    delivered
}

/// The last seq written so far.
async fn last_seq(session: &Session) -> u64 {
    session.attach(None).await.missed.last().unwrap().seq
}

/// Waits for a run that ends after `after`, without missing one that already ended.
async fn await_run_end(session: &Session, after: u64) {
    let is_end = |seq: &SeqEntry| matches!(seq.entry, Entry::RunEnd { .. });
    let mut attach = session.attach(Some(after)).await;
    if attach.missed.iter().any(is_end) {
        return;
    }
    within(async {
        loop {
            match attach.events.recv().await {
                Ok(SessionEvent::Entry(seq)) if is_end(&seq) => break,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("session closed"),
            }
        }
    })
    .await;
}

#[tokio::test]
async fn async_job_result_is_delivered_once_by_idle_wake() {
    let dir = tempfile::tempdir().unwrap();
    let command = "sleep 1; echo done";
    let (axo, api) = scripted(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c1",
                "bash",
                json!({"command": command, "async": true}),
            )])),
            ScriptedTurn::Reply(reply_text("started it")),
            ScriptedTurn::Reply(reply_text("it printed done")),
        ],
    )
    .await;
    let session = axo.create_session(bash_options(dir.path())).await.unwrap();

    assert_eq!(
        within(session.prompt("Run it in the background."))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    let output = tool_text(&session.attach(None).await.missed, "c1");
    assert!(
        output.contains("still running in the background as bg_1"),
        "{output}"
    );
    assert!(!output.contains("done"), "{output}");
    assert_eq!(api.requests().len(), 2);

    // Nobody prompts again: the job's end wakes the idle session.
    await_run_end(&session, last_seq(&session).await).await;
    let requests = api.requests();
    assert_eq!(requests.len(), 3);
    let users = user_texts(&requests[2]);
    let deliveries: Vec<_> = users
        .iter()
        .filter(|text| text.contains("Background job results:"))
        .collect();
    assert_eq!(deliveries.len(), 1, "{users:#?}");
    assert!(deliveries[0].contains("done"), "{users:#?}");
    assert_eq!(users.last(), Some(deliveries[0]), "{users:#?}");
    for request in &requests[..2] {
        assert!(
            !user_texts(request)
                .iter()
                .any(|text| text.contains("Background job results:")),
            "delivered too early"
        );
    }

    // Shutdown awaits any further run: none consumes the result again.
    session.shutdown().await;
    assert_eq!(api.requests().len(), 3);
    let lines = journal(&session);
    let end = assert_job_journaled(&lines, &session, "c1", command, "done");
    assert_delivered_by_message(&lines, end, "done");
}

#[tokio::test]
async fn wait_consumes_the_result_and_auto_delivery_skips_it() {
    let dir = tempfile::tempdir().unwrap();
    let command = "sleep 1; echo done";
    let (axo, api) = scripted(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c1",
                "bash",
                json!({"command": command, "async": true}),
            )])),
            ScriptedTurn::Reply(reply_tool_calls(&[("c2", "wait", json!({}))])),
            ScriptedTurn::Reply(reply_text("it printed done")),
        ],
    )
    .await;
    let session = axo.create_session(bash_options(dir.path())).await.unwrap();

    assert_eq!(
        within(session.prompt("Run it in the background, then wait."))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    let entries = session.attach(None).await.missed;
    let started = tool_text(&entries, "c1");
    assert!(
        started.contains("still running in the background as bg_1"),
        "{started}"
    );
    let waited = tool_text(&entries, "c2");
    assert!(waited.contains("[bg_1] sleep 1; echo done"), "{waited}");
    assert!(waited.contains("done"), "{waited}");

    // Shutdown awaits any automatic run, so one that delivered again would show here.
    session.shutdown().await;
    let requests = api.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert!(
            !user_texts(request)
                .iter()
                .any(|text| text.contains("Background job results:")),
            "the result was also delivered as a message"
        );
    }
    assert!(requests[2].transcript.iter().any(|message| matches!(
        message,
        Message::ToolResult(result) if result.tool_call_id == "c2"
    )));

    let lines = journal(&session);
    let end = assert_job_journaled(&lines, &session, "c1", command, "done");
    let delivered = only(&lines, "delivering entry", |v| {
        v.get("deliveredJobs").is_some()
    });
    assert_eq!(lines[delivered]["deliveredJobs"], json!(["bg_1"]));
    assert_eq!(lines[delivered]["message"]["role"], "toolResult");
    assert_eq!(lines[delivered]["message"]["toolCallId"], "c2");
    assert!(end < delivered, "{lines:#?}");
    assert!(
        !lines
            .iter()
            .any(|v| v["message"]["customType"] == "job_result"),
        "{lines:#?}"
    );
    assert_eq!(count(&lines, "run_end"), 1, "{lines:#?}");
}

#[tokio::test]
async fn wait_sees_a_job_promoted_in_its_batch() {
    let dir = tempfile::tempdir().unwrap();
    let (axo, _api) = scripted(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[
                (
                    "c1",
                    "bash",
                    json!({"command": "sleep 1; echo done", "async": true}),
                ),
                ("c2", "wait", json!({})),
            ])),
            ScriptedTurn::Reply(reply_text("it printed done")),
        ],
    )
    .await;
    let session = axo.create_session(bash_options(dir.path())).await.unwrap();

    assert_eq!(
        within(session.prompt("Run it in the background and wait for it."))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    let waited = tool_text(&session.attach(None).await.missed, "c2");
    assert!(waited.contains("[bg_1] sleep 1; echo done"), "{waited}");
    session.shutdown().await;
}

#[tokio::test]
async fn steer_promotes_a_foreground_command() {
    let dir = tempfile::tempdir().unwrap();
    let command = "sleep 2; echo late";
    let (axo, api) = scripted(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c1",
                "bash",
                json!({"command": command}),
            )])),
            ScriptedTurn::Reply(reply_text("hello")),
            ScriptedTurn::Reply(reply_text("it printed late")),
        ],
    )
    .await;
    let session = Arc::new(axo.create_session(bash_options(dir.path())).await.unwrap());
    let mut events = session.attach(None).await.events;
    let prompt = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("Run the slow command.").await }
    });

    within(async {
        loop {
            match events.recv().await {
                Ok(SessionEvent::Agent(AgentEvent::ToolExecutionStart { name, .. }))
                    if name == "bash" =>
                {
                    break;
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("session closed"),
            }
        }
    })
    .await;
    session.steer("hi").await.unwrap();
    assert_eq!(within(prompt).await.unwrap().unwrap(), RunOutcome::Done);

    let output = tool_text(&session.attach(None).await.missed, "c1");
    assert!(
        output.contains("still running in the background as bg_1"),
        "{output}"
    );
    assert!(!output.contains("late"), "{output}");
    let requests = api.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        user_texts(&requests[1]).last().map(String::as_str),
        Some("hi")
    );

    await_run_end(&session, last_seq(&session).await).await;
    let requests = api.requests();
    assert_eq!(requests.len(), 3);
    let users = user_texts(&requests[2]);
    let last = users.last().unwrap();
    assert!(last.starts_with("Background job results:"), "{users:#?}");
    assert!(last.contains("late"), "{users:#?}");

    session.shutdown().await;
    assert_eq!(api.requests().len(), 3);
    let lines = journal(&session);
    let end = assert_job_journaled(&lines, &session, "c1", command, "late");
    assert_delivered_by_message(&lines, end, "late");
}

#[tokio::test]
async fn retained_helpers_return_promptly_and_are_killable() {
    let dir = tempfile::tempdir().unwrap();
    let host = axo(
        dir.path(),
        vec![
            // The helper records its own pid for the test to watch.
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c1",
                "bash",
                json!({"command": "sh -c 'echo $$ > sleep.pid; exec sleep 300' > /dev/null 2>&1 & echo started"}),
            )])),
            ScriptedTurn::Reply(reply_text("started a helper")),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c2",
                "read",
                json!({"path": "proc://"}),
            )])),
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c3",
                "write",
                json!({"path": "proc://bg_1/kill", "content": ""}),
            )])),
            ScriptedTurn::Reply(reply_text("stopped it")),
        ],
    )
    .await;
    let options = bash_options(dir.path());
    let pid_file = options.cwd.join("sleep.pid");
    let session = host.create_session(options).await.unwrap();

    let started = Instant::now();
    assert_eq!(
        within(session.prompt("Start a detached helper."))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the command returned after {:?}",
        started.elapsed()
    );
    let entries = session.attach(None).await.missed;
    let output = tool_text(&entries, "c1");
    assert!(output.contains("started"), "{output}");
    assert!(output.contains("still running as bg_1"), "{output}");

    let deadline = Instant::now() + Duration::from_secs(2);
    let pid: u32 = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = text.trim().parse()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "the helper never wrote its pid");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(!dead(pid), "the helper {pid} must outlive its command");

    assert_eq!(
        within(session.prompt("List the jobs, then stop the helper."))
            .await
            .unwrap(),
        RunOutcome::Done
    );
    let entries = session.attach(None).await.missed;
    let listing = tool_text(&entries, "c2");
    assert!(listing.contains("bg_1  helpers running"), "{listing}");
    let stopped = tool_text(&entries, "c3");
    assert!(stopped.contains("Stopping bg_1."), "{stopped}");

    let deadline = Instant::now() + Duration::from_secs(3);
    while !dead(pid) {
        assert!(
            Instant::now() < deadline,
            "the helper {pid} survived its kill"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    session.shutdown().await;
}
