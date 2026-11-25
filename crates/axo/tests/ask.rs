mod common;

use std::sync::Arc;

use axo::{Answer, AnswerValue, Error, SessionEvent};
use axo_agent::RunOutcome;
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedTurn, ToolResultMessage, UserBlock};
use common::{options, within};
use serde_json::{Value, json};

/// The tool result of call `question` in a request's transcript.
fn question_result(transcript: &[Message]) -> &ToolResultMessage {
    transcript
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "question" => Some(result),
            _ => None,
        })
        .unwrap()
}

/// The result's text and the answers object after its `User answers:` line.
fn answers(result: &ToolResultMessage) -> (String, Value) {
    let text: String = result
        .content
        .iter()
        .filter_map(UserBlock::as_text)
        .collect();
    let (_, json) = text.split_once("User answers:\n").unwrap();
    let answers = serde_json::from_str(json).unwrap();
    (text, answers)
}

#[tokio::test]
async fn steer_settles_a_pending_question() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "question",
                "ask",
                json!({"blocks":[{
                    "type":"choice", "id":"choice", "question":"Which approach?",
                    "options":[{"value":"a","label":"A"}, {"value":"b","label":"B"}]
                }]}),
            )])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let mut opts = options(dir.path());
    opts.ask_enabled = true;
    let session = Arc::new(host.axo.create_session(opts).await.unwrap());
    let mut events = session.attach(None).await.events;
    let run = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("Ask me which approach.").await }
    });
    within(async {
        loop {
            if let SessionEvent::QuestionPending { call_id, .. } = &*events.recv().await.unwrap() {
                assert_eq!(call_id, "question");
                break;
            }
        }
    })
    .await;
    session.steer("do x instead").await.unwrap();
    within(async {
        loop {
            if let SessionEvent::QuestionResolved { call_id } = &*events.recv().await.unwrap() {
                assert_eq!(call_id, "question");
                break;
            }
        }
    })
    .await;
    assert_eq!(within(run).await.unwrap().unwrap(), RunOutcome::Done);
    assert!(!session.attach(None).await.live.auto_paused);

    let requests = host.api.requests();
    assert_eq!(requests.len(), 2);
    let result = question_result(&requests[1].transcript);
    assert!(!result.is_error);
    assert!(
        result
            .content
            .iter()
            .filter_map(UserBlock::as_text)
            .any(|text| text.contains("sent a message instead"))
    );
    let last_user = requests[1]
        .transcript
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => Some(user),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        last_user
            .content
            .iter()
            .filter_map(UserBlock::as_text)
            .collect::<Vec<_>>(),
        ["do x instead"]
    );
    session.shutdown().await;
}

#[tokio::test]
async fn dismiss_settles_a_pending_question_and_the_run_continues() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "question",
                "ask",
                json!({"blocks":[{
                    "type":"choice", "id":"choice", "question":"Which approach?",
                    "options":[{"value":"a","label":"A"}, {"value":"b","label":"B"}]
                }]}),
            )])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let mut opts = options(dir.path());
    opts.ask_enabled = true;
    let session = Arc::new(host.axo.create_session(opts).await.unwrap());
    let mut events = session.attach(None).await.events;
    let run = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("Ask me which approach.").await }
    });
    within(async {
        loop {
            if let SessionEvent::QuestionPending { call_id, .. } = &*events.recv().await.unwrap() {
                assert_eq!(call_id, "question");
                break;
            }
        }
    })
    .await;
    session.dismiss("question").unwrap();
    within(async {
        loop {
            if let SessionEvent::QuestionResolved { call_id } = &*events.recv().await.unwrap() {
                assert_eq!(call_id, "question");
                break;
            }
        }
    })
    .await;
    assert_eq!(within(run).await.unwrap().unwrap(), RunOutcome::Done);
    assert!(!session.attach(None).await.live.auto_paused);

    let requests = host.api.requests();
    assert_eq!(requests.len(), 2);
    let result = question_result(&requests[1].transcript);
    assert!(!result.is_error);
    assert!(
        result
            .content
            .iter()
            .filter_map(UserBlock::as_text)
            .any(|text| text.contains("dismissed these questions"))
    );
    assert_eq!(result.details, Some(json!({"outcome":"dismissed"})));
    session.shutdown().await;
}

#[tokio::test]
async fn invalid_submission_keeps_the_question_pending() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host(
        dir.path(),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "question",
                "ask",
                json!({"blocks":[
                    {"type":"markdown", "text":"Sessions live in memory."},
                    {"type":"choice", "id":"store", "question":"Session store?",
                     "options":[{"value":"sqlite","label":"SQLite"}, {"value":"redis","label":"Redis"}]}
                ]}),
            )])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let mut opts = options(dir.path());
    opts.ask_enabled = true;
    let session = Arc::new(host.axo.create_session(opts).await.unwrap());
    let mut events = session.attach(None).await.events;
    let run = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("Ask me about the store.").await }
    });
    within(async {
        while !matches!(
            &*events.recv().await.unwrap(),
            SessionEvent::QuestionPending { .. }
        ) {}
    })
    .await;

    let answer = |value: &str, note: Option<&str>| {
        vec![Answer {
            id: "store".into(),
            value: Some(AnswerValue::Text(value.into())),
            other: None,
            note: note.map(Into::into),
        }]
    };
    assert!(matches!(
        session.answer("question", answer("nope", None)),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(session.attach(None).await.live.pending_questions.len(), 1);
    session
        .answer("question", answer("sqlite", Some("n")))
        .unwrap();
    assert_eq!(within(run).await.unwrap().unwrap(), RunOutcome::Done);

    let requests = host.api.requests();
    let result = question_result(&requests[1].transcript);
    assert!(!result.is_error);
    assert_eq!(
        answers(result).1,
        json!({"store":{"value":"sqlite","note":"n"}})
    );
    session.shutdown().await;
}

#[tokio::test]
async fn timeout_submits_defaults_only() {
    let dir = tempfile::tempdir().unwrap();
    let host = common::host_with(
        dir.path(),
        json!([{"id":"m"}]),
        json!({"ask":{"timeoutMs":20}}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "question",
                "ask",
                json!({"blocks":[
                    {"type":"choice", "id":"store", "question":"Session store?",
                     "options":[{"value":"redis","label":"Redis"},
                                {"value":"sqlite","label":"SQLite","recommended":true}]},
                    {"type":"number", "id":"ttl", "question":"Lifetime (days)?", "integer":true, "default":30},
                    {"type":"text", "id":"name", "question":"Cookie name?"}
                ]}),
            )])),
            ScriptedTurn::Reply(reply_text("done")),
        ],
    )
    .await;
    let mut opts = options(dir.path());
    opts.ask_enabled = true;
    let session = host.axo.create_session(opts).await.unwrap();
    assert_eq!(
        within(session.prompt("Ask me about sessions."))
            .await
            .unwrap(),
        RunOutcome::Done
    );

    let requests = host.api.requests();
    let result = question_result(&requests[1].transcript);
    assert!(!result.is_error);
    let (text, answers) = answers(result);
    assert!(text.contains("did not answer in time"));
    assert_eq!(
        answers,
        json!({"store":{"value":"sqlite"},"ttl":{"value":30},"name":null})
    );
    session.shutdown().await;
}
