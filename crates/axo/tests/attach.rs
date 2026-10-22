//! A client attaching mid-run catches up exactly, and a resubmitted request id starts
//! nothing.

mod common;

use std::sync::Arc;

use axo::store::{Entry, SeqEntry};
use axo::{PromptRequest, PromptResult, SessionEvent};
use axo_agent::{AgentEvent, AgentMessage, CancelReason, RunOutcome};
use axo_ai::testing::reply_text;
use axo_ai::{AssistantBlock, AssistantEvent, Message, ScriptedTurn};
use common::{axo, options, within};

#[tokio::test]
async fn attach_mid_run_returns_missed_entries_live_state_and_next_entry() {
    let dir = tempfile::tempdir().unwrap();
    let axo = axo(dir.path(), vec![ScriptedTurn::HangUntilCancel]).await;
    let session = Arc::new(axo.create_session(options(dir.path())).await.unwrap());

    let mut probe = session.attach(None).await.events;
    let run = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("go").await }
    });
    within(async {
        loop {
            if let SessionEvent::Agent(AgentEvent::MessageUpdate {
                event: AssistantEvent::TextDelta { .. },
            }) = probe.recv().await.unwrap()
            {
                break;
            }
        }
    })
    .await;

    let all = session.attach(None).await.missed;
    let seq = all[all.len() / 2].seq;
    let attach = session.attach(Some(seq)).await;
    let expected: Vec<SeqEntry> = all
        .iter()
        .filter(|entry| entry.seq > seq)
        .cloned()
        .collect();
    assert!(!expected.is_empty());
    assert_eq!(attach.missed, expected);

    assert!(attach.live.run_id.is_some());
    let streaming = attach.live.agent.streaming.expect("a partial message");
    assert!(
        matches!(streaming.content.as_slice(), [AssistantBlock::Text { text, .. }] if text == "partial"),
        "{streaming:?}"
    );

    session.abort().await;
    let mut events = attach.events;
    let next = within(async {
        loop {
            if let SessionEvent::Entry(entry) = events.recv().await.unwrap() {
                break entry;
            }
        }
    })
    .await;
    assert_eq!(next.seq, all.last().unwrap().seq + 1);
    assert!(matches!(
        next.entry,
        Entry::Message {
            message: AgentMessage::Llm(Message::Assistant(_)),
            ..
        }
    ));
    assert_eq!(
        within(run).await.unwrap().unwrap(),
        RunOutcome::Aborted(CancelReason::User)
    );
}

#[tokio::test]
async fn prompt_with_a_repeated_request_id_submits_and_runs_once() {
    let dir = tempfile::tempdir().unwrap();
    let axo = axo(dir.path(), vec![ScriptedTurn::Reply(reply_text("hi"))]).await;
    let session = axo.create_session(options(dir.path())).await.unwrap();
    let request = PromptRequest {
        text: "hello".into(),
        request_id: Some("r1".into()),
    };

    let PromptResult::Settled {
        submission,
        outcome,
    } = within(session.prompt_with(request.clone())).await.unwrap()
    else {
        panic!("the first request runs");
    };
    assert_eq!(outcome, RunOutcome::Done);
    let PromptResult::Duplicate(existing) = within(session.prompt_with(request)).await.unwrap()
    else {
        panic!("the repeated request is a duplicate");
    };
    assert_eq!(existing, submission);

    let (_, entries) = axo.open_session_read(session.path()).await.unwrap();
    let count = |pick: fn(&Entry) -> bool| entries.iter().filter(|seq| pick(&seq.entry)).count();
    assert_eq!(count(|entry| matches!(entry, Entry::Submission { .. })), 1);
    assert_eq!(count(|entry| matches!(entry, Entry::RunStart { .. })), 1);
    assert_eq!(count(|entry| matches!(entry, Entry::RunEnd { .. })), 1);
    let Entry::Submission { submission_id, .. } = &submission.entry else {
        panic!("a submission entry");
    };
    assert!(entries.iter().any(|seq| matches!(
        &seq.entry,
        Entry::Message { message: AgentMessage::Llm(Message::User(_)), submission_id: Some(id), .. }
            if id == submission_id
    )));
}
