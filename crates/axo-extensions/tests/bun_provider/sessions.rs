//! Session capabilities through a real runner: `tests/fixtures/lab-session` is composed into
//! and attached to a real session whose model turns come from a [`ScriptedApi`]. One case
//! covers the replaced built-in `read` and its update, a policy block, handler-written branch
//! state, `/hello` reaching the journal, a virtual URL read, journal-entry events, and a host
//! abort reaching the `probe` handler's signal, and a `burst` of updates past the peer's callback cap
//! that the runner queues instead of ending the generation.
//!
//! Barriers are the fixture's files in its data directory (`probe-started`, `canceled`,
//! `entries`) and the `block` switch the test writes there.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axo::{Entry, PromptPart, Session, SessionEvent, SubmittedCustom, Supervision};
use axo_agent::{AgentEvent, AgentMessage, CancelReason, RunOutcome};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedApi, ScriptedTurn, ToolResultMessage, UserBlock};
use axo_extensions::ExtensionPhase;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::TryRecvError;

use crate::host::{Host, SESSION, WAIT_LIMIT, wait_for};

const ORIGIN: &str = "extension lab-session";
const FOCUS: &str = "keep the plan and open questions";

pub async fn run(bun: &Path, mode: Supervision) {
    let (host, api) = Host::sessions(bun, mode).await;
    let sdk = std::process::Command::new(bun)
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .args([
            "build",
            "--no-bundle",
            "packages/axo-extension-sdk/index.ts",
            "packages/axo-extension-sdk/bindings.ts",
            "--outdir",
        ])
        .arg(host.path("sdk"))
        .output()
        .unwrap();
    assert!(
        sdk.status.success(),
        "{}",
        String::from_utf8_lossy(&sdk.stderr)
    );
    let data = host.data_dir(SESSION);
    let mut options = host.session_options(Some("test/m"));
    options.exec_env = Some(Arc::new(axo::exec::StaticEnv::host()));
    let options = host.extensions.compose(options);
    let session = Arc::new(host.axo.create_session(options).await.unwrap());
    host.extensions.attach(&session).await;

    // The manifest's `read` replaces the built-in; the runner starts on the first call.
    let read = session
        .tools()
        .into_iter()
        .find(|tool| tool.name == "read")
        .expect("read is listed");
    assert_eq!(read.origin.as_deref(), Some(ORIGIN));
    let mut events = session.attach(None).await.events;
    turn(
        &session,
        &api,
        &[("r1", "read", json!({"path": "a"}))],
        "read a",
    )
    .await;
    let r1 = result(&session, "r1").await;
    assert!(!r1.is_error, "{:?}", r1.content);
    assert_eq!(text(&r1), r#"ext:read:{"path":"a"}"#);
    let mut updated = false;
    loop {
        match events.try_recv().as_deref() {
            Ok(SessionEvent::Agent(AgentEvent::ToolExecutionUpdate {
                call_id, update, ..
            })) if call_id == "r1" => {
                assert_eq!(
                    update.content,
                    vec![UserBlock::Text {
                        text: "partial".into()
                    }]
                );
                updated = true;
            }
            Ok(SessionEvent::Agent(AgentEvent::ToolExecutionEnd { call_id, .. }))
                if call_id == "r1" =>
            {
                break;
            }
            Ok(_) => {}
            Err(TryRecvError::Empty | TryRecvError::Closed) => {
                panic!("no ToolExecutionEnd for r1")
            }
            Err(TryRecvError::Lagged(missed)) => panic!("lagged by {missed} events"),
        }
    }
    assert!(updated, "no update preceded r1's end");

    // The policy blocks while `block` exists.
    std::fs::write(data.join("block"), "").unwrap();
    turn(
        &session,
        &api,
        &[("r2", "read", json!({"path": "a"}))],
        "read again",
    )
    .await;
    let r2 = result(&session, "r2").await;
    assert!(r2.is_error, "policy did not block: {:?}", r2.content);
    assert!(text(&r2).contains("no"), "{:?}", r2.content);
    std::fs::remove_file(data.join("block")).unwrap();

    turn(
        &session,
        &api,
        &[("delegate", "spawn", json!({"task": "review the change"}))],
        "delegate the review",
    )
    .await;
    let spawn = result(&session, "delegate").await;
    assert!(
        spawn.is_error && text(&spawn).contains("spawn recorded"),
        "{spawn:?}"
    );
    let recorded: Value =
        serde_json::from_slice(&std::fs::read(data.join("spawn.json")).unwrap()).unwrap();
    assert_eq!(
        recorded,
        json!({"task": [{"type": "text", "text": "review the change"}], "callId": "delegate"})
    );

    // Branch state written by the handler through unparented session actions.
    turn(
        &session,
        &api,
        &[(
            "s1",
            "call",
            json!({"tool": "probe", "args": {"mode": "state"}}),
        )],
        "state",
    )
    .await;
    let s1 = result(&session, "s1").await;
    assert!(!s1.is_error, "{:?}", s1.content);
    assert!(text(&s1).contains(r#"{"n":1}"#), "{:?}", s1.content);
    // The fixture says `lab`; the host stores it under `<ext-id>:lab`.
    assert_eq!(session.custom("lab-session:lab"), Some(json!({"n": 1})));

    // `/hello hi` runs in the runner, which posts a quiet `lab` message back.
    let command = session
        .parse_command("/hello hi")
        .expect("a command")
        .unwrap();
    bounded(session.run_command(command)).await.unwrap();
    assert_posted(&session, "hi").await;

    let url = session.schemes().exact("lab://a").unwrap();
    assert_eq!(
        bounded(session.schemes().read(&url)).await.unwrap(),
        "read:a"
    );

    let count = host.axo.models().all().len();
    assert!(count > 0);
    let reply = "A one-off answer.";
    api.lane("m", vec![ScriptedTurn::Reply(reply_text(reply))]);
    let ask = session.parse_command("/ask-model hello").unwrap().unwrap();
    bounded(session.run_command(ask)).await.unwrap();
    assert_posted(&session, &format!("{count}: {reply}")).await;

    api.lane("m", vec![ScriptedTurn::Error("offline".into())]);
    let report = session
        .parse_command("/ask-model report-error")
        .unwrap()
        .unwrap();
    bounded(session.run_command(report)).await.unwrap();
    let entries = session.attach(None).await.missed;
    let posted = entries
        .iter()
        .rev()
        .find_map(|seq| lab_text(&seq.entry))
        .expect("no reported completion error");
    let error: Value = serde_json::from_str(posted).unwrap();
    assert_eq!(error["code"], "completion");
    assert_eq!(error["message"], "Completion failed: offline");
    let details = &error["details"];
    assert_eq!(details["stopReason"], "error");
    assert_eq!(details.get("failure"), Some(&Value::Null));
    assert_eq!(
        serde_json::from_value::<axo_ai::Usage>(details["usage"].clone()).unwrap(),
        axo_ai::Usage::default()
    );

    let fork_here = session.parse_command("/fork-here").unwrap().unwrap();
    bounded(session.run_command(fork_here)).await.unwrap();
    assert_eq!(host.lifecycle.forks.load(Ordering::Relaxed), 1);
    let fork_id = {
        let held = host.lifecycle.sessions.lock().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].name().as_deref(), Some("forked"));
        held[0].id().to_owned()
    };
    assert_ne!(fork_id, session.id());
    assert_posted(&session, &fork_id).await;

    let again = session.parse_command("/again").unwrap().unwrap();
    tokio::time::timeout(WAIT_LIMIT, session.run_command(again))
        .await
        .expect("recursive command hung")
        .unwrap();
    assert_posted(&session, "command /again: already running in this session").await;

    let compact = session.parse_command("/compact-light").unwrap().unwrap();

    // Every entry appended since attach reached `onEntry` with its stored `type`.
    let entries = data.join("entries");
    wait_for("the submission entry event", || {
        std::fs::read_to_string(&entries).is_ok_and(|seen| seen.lines().any(|t| t == "submission"))
    })
    .await;
    let seen = std::fs::read_to_string(&entries).unwrap();
    assert!(
        seen.lines().all(|t| t != "undefined"),
        "an event had no entry type:\n{seen}"
    );

    // A held entry handler must settle before the next entry is delivered.
    let order = data.join("entry-order");
    bounded(session.append_custom("entry-hold", None))
        .await
        .unwrap();
    wait_for("held entry handler", || {
        std::fs::read_to_string(&order).is_ok_and(|seen| seen == "start\n")
    })
    .await;
    bounded(session.append_custom("entry-after", None))
        .await
        .unwrap();
    let barrier = session
        .parse_command("/hello order-barrier")
        .unwrap()
        .unwrap();
    bounded(session.run_command(barrier)).await.unwrap();
    assert_eq!(std::fs::read_to_string(&order).unwrap(), "start\n");
    std::fs::write(data.join("entry-release"), "").unwrap();
    wait_for("delivery after rejected entry handler", || {
        std::fs::read_to_string(&order).is_ok_and(|seen| seen.contains("after\n"))
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(order).unwrap(),
        "start\nreject\nafter\n"
    );

    // A host abort cancels the hanging probe; the handler sees its signal abort.
    api.lane(
        "m",
        vec![ScriptedTurn::Reply(reply_tool_calls(&[(
            "h1",
            "call",
            json!({"tool": "probe", "args": {"mode": "hang"}}),
        )]))],
    );
    let (outcome, ()) = tokio::join!(bounded(session.prompt("hang")), async {
        host.wait_file(SESSION, "probe-started").await;
        bounded(session.run_command(compact.clone())).await.unwrap();
        assert_posted(&session, "busy").await;
        session.abort();
    });
    assert!(
        matches!(outcome, Ok(RunOutcome::Aborted(CancelReason::User))),
        "{outcome:?}"
    );
    host.wait_file(SESSION, "canceled").await;

    // 300 updates exceed the peer's 256 live callbacks; the runner queues them.
    let generation = host.status(SESSION).generation;
    turn(
        &session,
        &api,
        &[("b1", "call", json!({"tool": "burst", "args": {}}))],
        "burst",
    )
    .await;
    let b1 = result(&session, "b1").await;
    assert!(!b1.is_error, "{:?}", b1.content);
    assert_eq!(text(&b1), "burst");
    let status = host.status(SESSION);
    assert_eq!(
        (status.phase, status.generation),
        (ExtensionPhase::Ready, generation)
    );

    // An idle compact summarizes history, and can also issue a separate split-turn
    // request for the last tool call. Both must carry the extension's focus.
    let before = api.requests().len();
    api.lane(
        "m",
        vec![ScriptedTurn::Reply(reply_text("Lab summary.")); 2],
    );
    bounded(session.run_command(compact)).await.unwrap();
    assert!(session.attach(None).await.missed.iter().any(|seq| matches!(
        &seq.entry,
        Entry::Compaction { focus: Some(focus), .. } if focus == FOCUS
    )));
    assert_summary_focus(&api, before, FOCUS);

    let watch = session.parse_command("/watch").unwrap().unwrap();
    bounded(session.run_command(watch)).await.unwrap();
    let cursor = session.attach(None).await.missed.last().unwrap().seq;
    api.lane("m", vec![ScriptedTurn::Reply(reply_text("watched"))]);
    assert_eq!(
        bounded(session.prompt("watch this turn")).await.unwrap(),
        RunOutcome::Done
    );
    let expected: Vec<_> = session
        .attach(Some(cursor))
        .await
        .missed
        .into_iter()
        .map(|entry| entry.seq)
        .collect();
    assert!(expected.len() >= 2 && expected.windows(2).all(|pair| pair[0] < pair[1]));
    let log = data.join("watch.log");
    wait_for("the watched journal entries", || {
        std::fs::read_to_string(&log).is_ok_and(|seen| seen.lines().count() >= expected.len())
    })
    .await;
    let seen: Vec<u64> = std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect();
    assert_eq!(seen, expected);

    let flood = session.parse_command("/flood").unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(40), async {
        session.run_command(flood).await.unwrap();
        assert_posted(&session, "stalled").await;
    })
    .await
    .expect("the queued command did not report a stall");

    tool_access(&session, &api, &host.path("work")).await;

    host.extensions.detach(session.id());
    session.shutdown().await;
    drop(session);

    let mut options = host.session_options(Some("test/m"));
    options.host.definitions.modes.push(axo::ModeSpec {
        name: "plan".into(),
        description: "Plan".into(),
        body: "Plan first.".into(),
        ..Default::default()
    });
    let session = Arc::new(
        host.axo
            .create_session(host.extensions.compose(options))
            .await
            .unwrap(),
    );
    host.extensions.attach(&session).await;
    bounded(plan_task(&session, &api)).await;
    bounded(skill_input(&session, &api)).await;
    bounded(skill_focus(&session, &api)).await;
    host.extensions.detach(session.id());
    session.shutdown().await;
    drop(session);
    host.close().await;
}

async fn tool_access(session: &Session, api: &ScriptedApi, cwd: &Path) {
    turn(
        session,
        api,
        &[("touch", "touch-two", json!({}))],
        "write two files",
    )
    .await;
    let touch = result(session, "touch").await;
    assert!(!touch.is_error, "{:?}", touch.content);
    let files = touch.details.as_ref().unwrap()["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let sha = files[0]["change"]["after"]["sha256"].as_str().unwrap();

    turn(
        session,
        api,
        &[(
            "edit-a",
            "edit",
            json!({
                "path": "a.txt", "edits": [{"oldText": "first", "newText": "edited"}]
            }),
        )],
        "edit the first file without a read",
    )
    .await;
    let edit = result(session, "edit-a").await;
    assert!(!edit.is_error, "{:?}", edit.content);
    assert_eq!(std::fs::read(cwd.join("a.txt")).unwrap(), b"edited");
    assert_eq!(std::fs::read(cwd.join("b.txt")).unwrap(), b"second");
    assert_eq!(session.read_snapshot(sha).await.unwrap(), b"first");

    std::fs::write(cwd.join("seen.txt"), "old").unwrap();
    turn(
        session,
        api,
        &[("stale", "stale", json!({}))],
        "overwrite an unread file",
    )
    .await;
    let stale = result(session, "stale").await;
    assert!(stale.is_error);
    assert!(
        text(&stale).contains("File has not been read in this session"),
        "{:?}",
        stale.content
    );
    assert_eq!(std::fs::read(cwd.join("seen.txt")).unwrap(), b"old");

    turn(session, api, &[("shell", "sh", json!({}))], "say hi").await;
    let shell = result(session, "shell").await;
    assert!(!shell.is_error, "{:?}", shell.content);
    assert_eq!(text(&shell).trim(), "hi");

    // Only promoted commands have a persistent job row after the foreground call returns.
    turn(
        session,
        api,
        &[("shell-bg", "sh", json!({"background": true}))],
        "start a job",
    )
    .await;
    assert!(
        session
            .jobs()
            .iter()
            .any(|job| job.call_id.as_deref() == Some("shell-bg")
                && matches!(job.kind, axo::JobKind::Command { .. }))
    );
}

async fn plan_task(session: &Session, api: &ScriptedApi) {
    assert_command_submits(session, api, "/plan-task do X", "do X").await;
    assert_eq!(session.mode().unwrap().name, "plan");
}

async fn skill_input(session: &Session, api: &ScriptedApi) {
    let rendered = review_block(session).await;
    assert_command_submits(session, api, "/skill-input a b", &rendered).await;
}

async fn skill_focus(session: &Session, api: &ScriptedApi) {
    let before = api.requests().len();
    api.lane(
        "m",
        vec![ScriptedTurn::Reply(reply_text("Review summary.")); 2],
    );
    let command = session.parse_command("/skill-focus a b").unwrap().unwrap();
    session.run_command(command).await.unwrap();
    assert_summary_focus(api, before, &review_block(session).await);
}

async fn review_block(session: &Session) -> String {
    session.render_skill("review", "a b").await.unwrap()
}

async fn assert_command_submits(session: &Session, api: &ScriptedApi, input: &str, expected: &str) {
    let before = api.requests().len();
    api.lane("m", vec![ScriptedTurn::Reply(reply_text("done"))]);
    let command = session.parse_command(input).unwrap().unwrap();
    session.run_command(command).await.unwrap();
    wait_for(input, || {
        !session.is_running() && api.requests().len() > before
    })
    .await;
    // Context notices (mode entry, deferred-tool changes) may follow the submission in the same
    // request, so look for it among every user message rather than only the last one.
    let submitted = api.requests()[before].transcript.iter().any(|message| {
        matches!(message, Message::User(user)
            if user.content.iter().filter_map(UserBlock::as_text).any(|text| text == expected))
    });
    assert!(
        submitted,
        "{input}: next request did not submit {expected:?}"
    );
}

fn assert_summary_focus(api: &ScriptedApi, before: usize, focus: &str) {
    let requests = api.requests();
    assert!(requests.len() > before, "no summary request");
    let instruction = format!("Focus from the user: {focus}\n");
    for request in &requests[before..] {
        assert!(
            last_user_text(request).any(|text| text.contains(&instruction)),
            "summary request omitted the focus"
        );
    }
}

fn last_user_text(request: &axo_ai::testing::RecordedRequest) -> impl Iterator<Item = &str> {
    request
        .transcript
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => Some(user),
            _ => None,
        })
        .expect("request has a user message")
        .content
        .iter()
        .filter_map(UserBlock::as_text)
}

fn lab_text(entry: &Entry) -> Option<&str> {
    if let Entry::Submission {
        custom: Some(SubmittedCustom { custom_type, .. }),
        parts,
        ..
    } = entry
        && custom_type == "lab-session:lab"
        && let [PromptPart::Text { text }] = parts.as_slice()
    {
        Some(text)
    } else {
        None
    }
}

async fn assert_posted(session: &Session, expected: &str) {
    let posted = session
        .attach(None)
        .await
        .missed
        .iter()
        .any(|seq| lab_text(&seq.entry) == Some(expected));
    assert!(
        posted,
        "no lab submission with text {expected:?} was journaled"
    );
}

/// Runs one prompt whose model calls `calls` once, then answers `done`.
pub async fn turn(
    session: &Session,
    api: &ScriptedApi,
    calls: &[(&str, &str, Value)],
    prompt: &str,
) {
    api.lane(
        "m",
        vec![
            ScriptedTurn::Reply(reply_tool_calls(calls)),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    );
    let outcome = bounded(session.prompt(prompt)).await.unwrap();
    assert_eq!(outcome, RunOutcome::Done, "{prompt}");
}

/// The journaled result of `call_id`.
pub async fn result(session: &Session, call_id: &str) -> ToolResultMessage {
    session
        .attach(None)
        .await
        .missed
        .into_iter()
        .find_map(|seq| match seq.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } if result.tool_call_id == call_id => Some(result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {call_id}"))
}

pub fn text(result: &ToolResultMessage) -> String {
    result
        .content
        .iter()
        .filter_map(UserBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT_LIMIT + Duration::from_secs(30), future)
        .await
        .expect("timed out")
}
