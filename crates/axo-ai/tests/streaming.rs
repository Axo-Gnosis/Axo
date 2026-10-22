//! Streaming primitives: SSE decoding, partial/complete tool-argument JSON, and the
//! event `Accumulator`.

use axo_ai::api::sse::{SseDecoder, SseEvent};
use axo_ai::json::{parse_streaming, parse_tool_arguments};
use axo_ai::testing::{ScriptedApi, ScriptedTurn, reply_thinking_then_text, reply_tool_calls};
use axo_ai::{
    Accumulator, ApiKind, AssistantEvent, Compat, Context, Cost, InputKind, Message, Model,
    SimpleStreamOptions, Streamer, Transcript, UserBlock, UserMessage,
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[test]
fn sse_event_split_across_three_chunks_with_crlf() {
    let mut decoder = SseDecoder::new();
    // The CRLF after the first data line is split between chunks 2 and 3.
    assert!(
        decoder
            .push(b": keep-alive\r\nevent: content_block_delta\r\nda")
            .is_empty()
    );
    assert!(decoder.push(b"ta: {\"a\":1}\r").is_empty());
    assert_eq!(
        decoder.push(b"\ndata: {\"b\":2}\r\n\r\n"),
        [SseEvent {
            event: Some("content_block_delta".to_owned()),
            data: "{\"a\":1}\n{\"b\":2}".to_owned(),
        }]
    );
    assert_eq!(decoder.finish(), None);
}

#[test]
fn parse_streaming_closes_truncated_prefixes() {
    let object = |value: Value| value.as_object().unwrap().clone();
    assert_eq!(parse_streaming(r#"{"a": "x"#), object(json!({ "a": "x" })));
    assert_eq!(
        parse_streaming(r#"{"a": [1, 2"#),
        object(json!({ "a": [1, 2] }))
    );
    assert_eq!(parse_streaming(r#"{"a":"#), object(json!({})));
}

#[test]
fn parse_tool_arguments_reports_non_object_json() {
    let (arguments, error) = parse_tool_arguments("[1, 2]");
    assert!(arguments.is_empty());
    assert_eq!(
        error.as_deref(),
        Some("Tool arguments must be a JSON object\n\nReceived arguments:\n[1, 2]")
    );
}

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
        headers: Default::default(),
        compat: Compat::default(),
    };
    let transcript = Transcript::normalize(Context {
        system_prompt: Some("You are a test.".to_owned()),
        messages: vec![Message::User(UserMessage {
            content: vec![UserBlock::Text {
                text: "hi".to_owned(),
            }],
            timestamp: 0,
        })],
        tools: Vec::new(),
    });
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
                &transcript,
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
        assert_eq!(accumulator.message(), Some(done));
        accumulator.apply(terminal);
        assert_eq!(accumulator.into_message().as_ref(), Some(done));
    }
}
