mod common;

use std::sync::Arc;

use axo::SessionEvent;
use axo_agent::RunOutcome;
use axo_ai::testing::{reply_text, reply_tool_calls};
use axo_ai::{Message, ScriptedTurn, UserBlock};
use common::{host, options, within};
use serde_json::json;

#[tokio::test]
async fn steer_settles_a_pending_question() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        json!([{"id":"m"}]),
        json!({}),
        vec![
            ScriptedTurn::Reply(reply_tool_calls(&[(
                "question",
                "ask",
                json!({"questions":[{
                    "id":"choice", "question":"Which approach?",
                    "options":[{"label":"A"}, {"label":"B"}]
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
            if let SessionEvent::QuestionPending { call_id, .. } = events.recv().await.unwrap() {
                assert_eq!(call_id, "question");
                break;
            }
        }
    })
    .await;
    session.steer("do x instead").await.unwrap();
    within(async {
        loop {
            if let SessionEvent::QuestionResolved { call_id } = events.recv().await.unwrap() {
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
    let result = requests[1]
        .transcript
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "question" => Some(result),
            _ => None,
        })
        .unwrap();
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
