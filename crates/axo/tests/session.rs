//! Session runs outlive their callers, the read record survives reopening, and input
//! stranded by a crash runs when the session is opened again.

mod common;

use std::time::Duration;

use axo::SessionEvent;
use axo::store::{
    self, Entry, EntryBase, EntryId, JsonlStore, SeqEntry, SessionStore, SubmissionKind,
};
use axo_agent::{AgentMessage, RunOutcome};
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{AssistantBlock, Message, ScriptedTurn, UserBlock};
use common::{axo, options, within};
use serde_json::json;

#[tokio::test]
async fn dropping_a_prompt_future_still_settles_its_run() {
    let dir = tempfile::tempdir().unwrap();
    let axo = axo(
        dir.path(),
        vec![
            ScriptedTurn::HangUntilCancel,
            ScriptedTurn::Reply(reply_text("second")),
        ],
    )
    .await;
    let session = axo.create_session(options(dir.path())).await.unwrap();

    assert!(
        tokio::time::timeout(Duration::from_millis(200), session.prompt("first"))
            .await
            .is_err(),
        "the first run hangs until cancelled"
    );
    let mut events = session.attach(None).await.events;
    session.abort().await;
    within(async {
        loop {
            if let SessionEvent::Entry(SeqEntry {
                entry:
                    Entry::RunEnd {
                        outcome: store::RunOutcome::Aborted,
                        ..
                    },
                ..
            }) = events.recv().await.unwrap()
            {
                break;
            }
        }
    })
    .await;

    assert_eq!(
        within(session.prompt("again")).await.unwrap(),
        RunOutcome::Done
    );
}

#[tokio::test]
async fn reopened_session_edits_a_file_it_read_before() {
    let dir = tempfile::tempdir().unwrap();
    let notes = options(dir.path()).cwd.join("notes.txt");
    std::fs::write(&notes, "hello\n").unwrap();

    let path = {
        let axo = axo(
            dir.path(),
            vec![
                ScriptedTurn::Reply(reply_tool_calls(&[(
                    "c1",
                    "read",
                    json!({ "path": "notes.txt" }),
                )])),
                ScriptedTurn::Reply(reply_text("read")),
            ],
        )
        .await;
        let session = axo.create_session(options(dir.path())).await.unwrap();
        assert_eq!(
            within(session.prompt("read the notes")).await.unwrap(),
            RunOutcome::Done
        );
        session.path().to_path_buf()
    };

    let axo = axo(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "c2",
                "edit",
                json!({ "path": "notes.txt", "edits": [{ "oldText": "hello", "newText": "bye" }] }),
            )])),
            ScriptedTurn::Reply(reply_text("edited")),
        ],
    )
    .await;
    let session = axo.open_session(&path, options(dir.path())).await.unwrap();
    assert_eq!(
        within(session.prompt("edit the notes")).await.unwrap(),
        RunOutcome::Done
    );

    let (_, entries) = axo.open_session_read(&path).await.unwrap();
    let result = entries
        .iter()
        .find_map(|seq| match &seq.entry {
            Entry::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
                ..
            } if result.tool_call_id == "c2" => Some(result),
            _ => None,
        })
        .expect("the edit's result");
    assert!(!result.is_error, "{:?}", result.content);
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "bye\n");
}

#[tokio::test]
async fn stranded_prompt_runs_when_the_session_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = {
        let axo = axo(dir.path(), vec![ScriptedTurn::Reply(reply_text("one"))]).await;
        let session = axo.create_session(options(dir.path())).await.unwrap();
        assert_eq!(
            within(session.prompt("first")).await.unwrap(),
            RunOutcome::Done
        );
        session.path().to_path_buf()
    };

    // A crash between saving a prompt and starting its run leaves only the submission.
    {
        let sessions = JsonlStore::new(dir.path().join("sessions"));
        let (_, entries, mut writer) = sessions.open_write(&path).await.unwrap();
        let parent = entries
            .last()
            .and_then(|last| last.entry.id())
            .map(|id| EntryId(id.to_owned()));
        let id = EntryId::generate(|id| entries.iter().any(|seq| seq.entry.id() == Some(id)));
        writer
            .append(Entry::Submission {
                base: EntryBase::new(id, parent),
                submission_id: "s-stranded".into(),
                kind: SubmissionKind::Prompt,
                content: vec![UserBlock::Text {
                    text: "second".into(),
                }],
                request_id: None,
            })
            .await
            .unwrap();
    }

    let axo = axo(dir.path(), vec![ScriptedTurn::Reply(reply_text("two"))]).await;
    let session = axo.open_session(&path, options(dir.path())).await.unwrap();
    let attach = session.attach(None).await;
    // The resumed run may have settled already: its `run_end` follows the submission.
    let is_done = |seq: &SeqEntry| {
        matches!(
            seq.entry,
            Entry::RunEnd {
                outcome: store::RunOutcome::Done,
                ..
            }
        )
    };
    let submitted = attach
        .missed
        .iter()
        .position(|seq| {
            matches!(&seq.entry, Entry::Submission { submission_id, .. } if submission_id == "s-stranded")
        })
        .expect("the stranded submission");
    if !attach.missed[submitted..].iter().any(is_done) {
        let mut events = attach.events;
        within(async {
            loop {
                if let SessionEvent::Entry(seq) = events.recv().await.unwrap()
                    && is_done(&seq)
                {
                    break;
                }
            }
        })
        .await;
    }

    let (_, entries) = axo.open_session_read(&path).await.unwrap();
    let user = entries
        .iter()
        .position(|seq| {
            matches!(&seq.entry, Entry::Message {
                message: AgentMessage::Llm(Message::User(user)),
                submission_id: Some(id),
                ..
            } if id == "s-stranded"
                && matches!(user.content.as_slice(), [UserBlock::Text { text }] if text == "second"))
        })
        .expect("the stranded prompt's message");
    assert!(
        entries[user..].iter().any(|seq| matches!(&seq.entry, Entry::Message {
            message: AgentMessage::Llm(Message::Assistant(assistant)),
            ..
        } if matches!(assistant.content.as_slice(), [AssistantBlock::Text { text, .. }] if text == "two"))),
        "the stranded prompt was answered"
    );
}
