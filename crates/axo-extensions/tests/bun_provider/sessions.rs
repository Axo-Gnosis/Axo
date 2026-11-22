//! Session capabilities through a real runner: `tests/fixtures/lab-session` is composed into
//! and attached to a real session whose model turns come from a [`ScriptedApi`]. One case
//! covers the replaced built-in `read` and its update, a policy block, branch state across a
//! fork and a reopen, `/hello` reaching the journal, journal-entry events, and a host abort
//! reaching the `probe` handler's signal, and a `burst` of updates past the peer's callback cap
//! that the runner queues instead of ending the generation.
//!
//! Barriers are the fixture's files in its data directory (`probe-started`, `canceled`,
//! `entries`) and the `block` switch the test writes there.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axo::{
    Command, CommandKind, Entry, EntryId, PromptPart, Session, SessionEvent, SubmittedCustom,
    Supervision,
};
use axo_agent::{AgentEvent, AgentMessage, CancelReason, RunOutcome};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedApi, ScriptedTurn, ToolResultMessage, UserBlock};
use axo_extensions::ExtensionPhase;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::TryRecvError;

use crate::host::{Host, SESSION, WAIT_LIMIT, wait_for};

const ORIGIN: &str = "extension lab-session";

pub async fn run(bun: &Path, mode: Supervision) {
    let (host, api) = Host::sessions(bun, mode).await;
    let data = host.data_dir(SESSION);
    let options = host
        .extensions
        .compose(host.session_options(Some("test/m")));
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
        match events.try_recv() {
            Ok(SessionEvent::Agent(AgentEvent::ToolExecutionUpdate { call_id, update }))
                if call_id == "r1" =>
            {
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
    let leaf = session
        .attach(None)
        .await
        .missed
        .iter()
        .rev()
        .find_map(|seq| seq.entry.id().map(|id| EntryId(id.to_owned())))
        .expect("the journal has entries");
    let fork = Arc::new(session.fork(&leaf).await.unwrap().session);
    host.extensions.attach(&fork).await;
    assert_eq!(fork.custom("lab-session:lab"), Some(json!({"n": 1})));
    host.extensions.detach(fork.id());
    fork.shutdown().await;
    drop(fork);

    // `/hello hi` runs in the runner, which posts a quiet `lab` message back.
    let hello = session
        .commands()
        .into_iter()
        .find(|command| command.name == "hello")
        .expect("hello is listed");
    assert!(
        matches!(hello.kind, CommandKind::Registered { .. }),
        "{hello:?}"
    );
    let command = session
        .parse_command("/hello hi")
        .expect("a command")
        .unwrap();
    assert_eq!(
        command,
        Command::Registered {
            name: "hello".into(),
            args: "hi".into()
        }
    );
    bounded(session.run_command(command)).await.unwrap();
    let posted = session.attach(None).await.missed.iter().any(|seq| {
        matches!(
            &seq.entry,
            Entry::Submission { custom: Some(SubmittedCustom { custom_type, .. }), parts, .. }
                if custom_type == "lab-session:lab"
                    && matches!(parts.as_slice(), [PromptPart::Text { text }] if text == "hi")
        )
    });
    assert!(posted, "no lab submission with text hi was journaled");

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

    // The state survives a reopen.
    let path = session.path().to_path_buf();
    host.extensions.detach(session.id());
    session.shutdown().await;
    drop(session);
    let options = host.extensions.compose(host.session_options(None));
    let reopened = host.axo.open_session(&path, options).await.unwrap();
    assert_eq!(reopened.custom("lab-session:lab"), Some(json!({"n": 1})));
    reopened.shutdown().await;
    drop(reopened);
    host.close().await;
}

/// Runs one prompt whose model calls `calls` once, then answers `done`.
async fn turn(session: &Session, api: &ScriptedApi, calls: &[(&str, &str, Value)], prompt: &str) {
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
async fn result(session: &Session, call_id: &str) -> ToolResultMessage {
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

fn text(result: &ToolResultMessage) -> String {
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
