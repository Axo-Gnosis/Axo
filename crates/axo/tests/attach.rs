//! A client attaching mid-run catches up exactly, and a resubmitted request id starts
//! nothing.

mod common;

use std::sync::Arc;

use axo::{
    Entry, JsonlStore, KeywordSpec, PromptPart, PromptRequest, PromptResult, SeqEntry, Session,
    SessionEvent, SubmissionKind,
};
use axo_agent::{AgentEvent, AgentMessage, CancelReason, RunOutcome};
use axo_ai::testing::reply_text;
use axo_ai::{AssistantBlock, AssistantEvent, Message, ScriptedApi, ScriptedTurn};
use common::{GatedStore, WriteGate, axo, axo_with, options, within};
use serde_json::json;
use tokio::task::JoinHandle;

async fn start_hanging_prompt(
    session: &Arc<Session>,
) -> JoinHandle<Result<RunOutcome, axo::Error>> {
    let mut probe = session.attach(None).await.events;
    let run = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("go").await }
    });
    within(async {
        loop {
            if let SessionEvent::Agent(AgentEvent::MessageUpdate {
                event: AssistantEvent::TextDelta { .. },
            }) = &*probe.recv().await.unwrap()
            {
                break;
            }
        }
    })
    .await;
    run
}

#[tokio::test]
async fn attach_mid_run_returns_missed_entries_live_state_and_next_entry() {
    let dir = tempfile::tempdir().unwrap();
    let axo = axo(dir.path(), vec![ScriptedTurn::HangUntilCancel]).await;
    let session = Arc::new(axo.create_session(options(dir.path())).await.unwrap());
    let run = start_hanging_prompt(&session).await;

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
    let streaming = attach.live.streaming.expect("a partial message");
    assert!(
        matches!(streaming.content.as_slice(), [AssistantBlock::Text { text, .. }] if text == "partial"),
        "{streaming:?}"
    );

    session.abort();
    let mut events = attach.events;
    within(async {
        loop {
            if let SessionEvent::Entry(entry) = &*events.recv().await.unwrap() {
                assert_eq!(entry.seq, all.last().unwrap().seq + 1);
                assert!(matches!(
                    entry.entry,
                    Entry::Message {
                        message: AgentMessage::Llm(Message::Assistant(_)),
                        ..
                    }
                ));
                break;
            }
        }
    })
    .await;
    assert_eq!(
        within(run).await.unwrap().unwrap(),
        RunOutcome::Aborted(CancelReason::User)
    );
}

#[tokio::test]
async fn attach_between_submission_and_enqueue_observes_queue_change() {
    let dir = tempfile::tempdir().unwrap();
    let gate = WriteGate::new(
        |entry| matches!(entry, Entry::ModelChange { model_id, .. } if model_id == "second"),
    );
    let (axo, _, _) = axo_with(
        dir.path(),
        json!([{"id":"m"},{"id":"second"}]),
        json!({}),
        Arc::new(GatedStore {
            inner: JsonlStore::new(dir.path().join("sessions")),
            gate: gate.clone(),
        }),
        Arc::new(ScriptedApi::new(vec![ScriptedTurn::HangUntilCancel])),
    )
    .await;
    let mut opts = options(dir.path());
    opts.host.definitions.keywords.push(KeywordSpec {
        name: "switch".into(),
        model: Some("test/second".into()),
        ..Default::default()
    });
    let session = Arc::new(axo.create_session(opts).await.unwrap());
    let run = start_hanging_prompt(&session).await;
    let mut probe = session.attach(None).await.events;
    let follow_up = tokio::spawn({
        let session = session.clone();
        async move { session.follow_up("switch").await }
    });
    within(gate.entered.notified()).await;
    let (seq, id) = within(async {
        loop {
            if let SessionEvent::Entry(seq) = &*probe.recv().await.unwrap()
                && let Entry::Submission {
                    kind: SubmissionKind::FollowUp,
                    submission_id,
                    ..
                } = &seq.entry
            {
                break (seq.seq, submission_id.clone());
            }
        }
    })
    .await;
    let mut attach = within(session.attach(Some(seq))).await;
    assert!(attach.live.queued.is_empty());
    assert!(attach.missed.is_empty());

    gate.release.notify_one();
    within(async {
        while !matches!(
            &*attach.events.recv().await.unwrap(),
            SessionEvent::QueueChanged
        ) {}
    })
    .await;
    let snapshot = within(session.attach(Some(seq))).await;
    assert!(matches!(snapshot.live.queued.as_slice(), [item] if item.submission_id == id));
    within(follow_up).await.unwrap().unwrap();

    within(session.follow_up("later")).await.unwrap();
    within(session.steer("now")).await.unwrap();
    let snapshot = within(session.attach(Some(seq))).await;
    let texts: Vec<_> = snapshot
        .live
        .queued
        .iter()
        .map(|item| {
            let [PromptPart::Text { text }] = item.parts.as_slice() else {
                panic!("expected a text submission");
            };
            text.as_str()
        })
        .collect();
    assert_eq!(texts, ["now", "switch", "later"]);
    session.abort();
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
        input: "hello".into(),
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
