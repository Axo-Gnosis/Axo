//! Event `Accumulator` reconstruction.

use axo_ai::testing::{ScriptedApi, ScriptedTurn, reply_thinking_then_text, reply_tool_calls};
use axo_ai::{
    Accumulator, ApiKind, AssistantEvent, Compat, Cost, InputKind, Message, Model,
    SimpleStreamOptions, Streamer, SystemMessage, TextBlock, Transcript, UserBlock, UserMessage,
};
use futures::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn accumulator_reproduces_done_message() {
    let model = Model {
        id: "scripted".to_owned(),
        name: "Scripted".to_owned(),
        api: ApiKind::AnthropicMessages,
        provider: "test".to_owned(),
        base_url: "http://localhost".to_owned(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: Cost::default(),
        context_window: 200_000,
        max_tokens: 32_000,
        compat: Compat::default(),
    };
    let transcript = Transcript::new(vec![
        Message::System(SystemMessage {
            content: vec![TextBlock {
                text: "You are a test.".to_owned(),
            }],
            ..Default::default()
        }),
        Message::User(UserMessage {
            content: vec![UserBlock::Text {
                text: "hi".to_owned(),
            }],
            timestamp: 0,
        }),
    ]);
    let replies = [
        reply_thinking_then_text("let me think about it", "hello there"),
        reply_tool_calls(&[
            ("call_1", "get_weather", json!({ "city": "Paris" })),
            (
                "call_2",
                "get_weather",
                json!({ "city": "Tokyo", "days": [1, 2] }),
            ),
        ]),
    ];
    let api = ScriptedApi::new(replies.iter().cloned().map(ScriptedTurn::Reply).collect());

    for reply in &replies {
        let events: Vec<AssistantEvent> = api
            .stream(
                &model,
                transcript.clone(),
                SimpleStreamOptions::default(),
                CancellationToken::new(),
            )
            .collect()
            .await;
        let Some((terminal, deltas)) = events.split_last() else {
            panic!("empty stream");
        };
        let AssistantEvent::Done { message: done, .. } = terminal else {
            panic!("stream did not end with Done: {terminal:?}");
        };
        assert_eq!(done.content, reply.content);

        let mut accumulator = Accumulator::new();
        for event in deltas {
            accumulator.apply(event);
        }
        // Rebuilt from `Start` and the block events alone, before `Done` replaces it.
        assert_eq!(accumulator.snapshot().as_ref(), Some(done));
        accumulator.apply(terminal);
        assert_eq!(accumulator.into_message().as_ref(), Some(done));
    }
}
