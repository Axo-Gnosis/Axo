use std::collections::BTreeMap;

use axo_ai::testing::{fixture_model, http_header_values, serve_http};
use axo_ai::{
    Api, ApiKind, AssistantBlock, AssistantEvent, AssistantMessage, GoogleApi, Message, Model,
    ModelAuth, ReasoningReplay, SimpleStreamOptions, StopReason, StreamOptions, SystemMessage,
    TextBlock, ThinkingLevel, ToolCall, ToolResultMessage, ToolSpec, Transcript, UserBlock,
    UserMessage,
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn sse(frames: &[Value]) -> String {
    let body: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn gemini(url: &str, id: &str, mapped: bool) -> Model {
    let mut model = fixture_model(
        "google",
        id,
        id,
        ApiKind::GoogleGenerativeAi,
        url.into(),
        1_000_000,
    );
    model.reasoning = true;
    if mapped {
        model.thinking_level_map = Some(BTreeMap::from([
            (ThinkingLevel::Off, None),
            (ThinkingLevel::Medium, Some("medium".into())),
        ]));
    }
    model
}

async fn run(model: &Model, transcript: Transcript) -> Vec<AssistantEvent> {
    GoogleApi::new(reqwest::Client::new())
        .stream(
            model,
            transcript,
            StreamOptions {
                simple: SimpleStreamOptions {
                    thinking: ThinkingLevel::Medium,
                    ..Default::default()
                },
                auth: ModelAuth {
                    api_key: Some("synthetic-key".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            CancellationToken::new(),
        )
        .collect()
        .await
}

fn terminal(events: &[AssistantEvent]) -> &AssistantMessage {
    match events.last().unwrap() {
        AssistantEvent::Done { message, .. } | AssistantEvent::Error { message, .. } => message,
        event => panic!("not terminal: {event:?}"),
    }
}

fn call(id: &str, signature: Option<&str>) -> AssistantBlock {
    AssistantBlock::ToolCall(ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: json!({"path": id}).as_object().unwrap().clone(),
        arguments_error: None,
        thought_signature: signature.map(Into::into),
    })
}

fn result(id: &str, content: Vec<UserBlock>) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "read".into(),
        content,
        details: None,
        is_error: false,
        timestamp: 0,
        duration_ms: None,
    })
}

#[tokio::test]
async fn google_round_trip() {
    let reply = sse(&[
        json!({"responseId": "r1", "modelVersion": "gemini-test-001", "candidates": [{"content": {"parts": [
            {"text": "plan", "thought": true, "thoughtSignature": "c2lnMQ=="}
        ]}}]}),
        json!({"candidates": [{"content": {"parts": [
            {"text": "Hello", "thoughtSignature": "c2lnMg=="},
            {"functionCall": {"name": "read", "args": {"path": "a"}, "id": "fc1"}, "thoughtSignature": "c2lnMw=="}
        ]}, "finishReason": "STOP"}],
         "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "thoughtsTokenCount": 3,
                           "totalTokenCount": 18, "cachedContentTokenCount": 4}}),
    ]);
    let wrapped = sse(&[json!({"response": {"candidates": [{
        "content": {"parts": [{"text": "done"}]}, "finishReason": "STOP"
    }]}})]);
    let failing =
        sse(&[json!({"error": {"code": 429, "message": "quota", "status": "RESOURCE_EXHAUSTED"}})]);
    let (url, server) = serve_http([reply, wrapped, failing].into_iter());
    let model = gemini(&url, "gemini-test", true);
    let system = Message::System(SystemMessage {
        content: vec![TextBlock {
            text: "Be brief.".into(),
        }],
        tools_added: vec![ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type": "object", "properties": {"mode": {"const": "fast"}}}),
        }],
        ..Default::default()
    });
    let user = Message::User(UserMessage {
        content: vec![UserBlock::Text { text: "hi".into() }],
        timestamp: 0,
    });

    // 1. Thought, signed text and a signed call decode into blocks, usage and ToolUse.
    let events = run(&model, Transcript::new(vec![system.clone(), user.clone()])).await;
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match event {
            AssistantEvent::Start { .. } => "start",
            AssistantEvent::ThinkingStart { .. } => "thinking",
            AssistantEvent::ThinkingDelta { .. } => "thinking-delta",
            AssistantEvent::ThinkingEnd { .. } => "thinking-end",
            AssistantEvent::TextStart { .. } => "text",
            AssistantEvent::TextDelta { .. } => "text-delta",
            AssistantEvent::TextEnd { .. } => "text-end",
            AssistantEvent::ToolCallStart { .. } => "call",
            AssistantEvent::ToolCallDelta { .. } => "call-delta",
            AssistantEvent::ToolCallEnd { .. } => "call-end",
            AssistantEvent::Done { .. } => "done",
            AssistantEvent::Error { .. } => "error",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "start",
            "thinking",
            "thinking-delta",
            "thinking-end",
            "text",
            "text-delta",
            "text-end",
            "call",
            "call-delta",
            "call-end",
            "done"
        ]
    );
    let first = terminal(&events).clone();
    assert_eq!(first.stop_reason, StopReason::ToolUse);
    assert_eq!(first.response_id.as_deref(), Some("r1"));
    assert_eq!(first.response_model.as_deref(), Some("gemini-test-001"));
    assert_eq!(first.provider_thinking_level.as_deref(), Some("medium"));
    let usage = &first.usage;
    assert_eq!(
        (
            usage.input,
            usage.cache_read,
            usage.output,
            usage.reasoning,
            usage.total_tokens
        ),
        (6, 4, 8, Some(3), 18)
    );
    assert!(matches!(
        &first.content[0],
        AssistantBlock::Thinking { replay: Some(ReasoningReplay::GoogleThoughtSignature(s)), .. } if s == "c2lnMQ=="
    ));

    // 2. The replay carries every signature, the call id, merged results and the sentinel
    //    on another model's unsigned call; the reply arrives in the Cloud Code envelope.
    let mut foreign = AssistantMessage::empty(&gemini(&url, "other", false));
    foreign.content = vec![call("x", None), call("y", Some("not base64!"))];
    let transcript = Transcript::new(vec![
        system,
        user,
        Message::Assistant(foreign),
        result("x", vec![UserBlock::Text { text: "one".into() }]),
        result(
            "y",
            vec![UserBlock::Image {
                data: "aW1n".into(),
                mime_type: "image/png".into(),
            }],
        ),
        Message::Assistant(first),
        result("fc1", vec![UserBlock::Text { text: "a".into() }]),
    ]);
    let events = run(&model, transcript.clone()).await;
    let second = terminal(&events);
    assert_eq!(second.stop_reason, StopReason::Stop, "{second:?}");
    assert!(matches!(&second.content[..], [AssistantBlock::Text { text, .. }] if text == "done"));

    // 3. A budget model sends a budget; an in-band error fails the stream with its code.
    let budget = gemini(&url, "gemini-budget", false);
    let events = run(&budget, transcript).await;
    let third = terminal(&events);
    assert_eq!(third.stop_reason, StopReason::Error);
    assert!(
        third
            .error_message
            .as_deref()
            .unwrap()
            .contains("quota (RESOURCE_EXHAUSTED)"),
        "{third:?}"
    );
    assert_eq!(third.failure.as_ref().unwrap().code.as_deref(), Some("429"));

    let requests = server.join().unwrap();
    let (head, body) = &requests[0];
    assert!(
        head.starts_with("POST /models/gemini-test:streamGenerateContent?alt=sse "),
        "{head}"
    );
    assert_eq!(
        http_header_values(head, "x-goog-api-key").collect::<Vec<_>>(),
        ["synthetic-key"]
    );
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be brief.");
    let declaration = &body["tools"][0]["functionDeclarations"][0];
    assert_eq!(
        declaration["parametersJsonSchema"]["properties"]["mode"],
        json!({"enum": ["fast"]})
    );
    assert_eq!(
        body["generationConfig"]["thinkingConfig"],
        json!({"includeThoughts": true, "thinkingLevel": "MEDIUM"})
    );

    let contents = requests[1].1["contents"].as_array().unwrap().clone();
    let roles: Vec<&str> = contents
        .iter()
        .map(|turn| turn["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "model", "user", "user", "model", "user"]);
    assert_eq!(
        contents[1]["parts"],
        json!([
            {"functionCall": {"name": "read", "args": {"path": "x"}, "id": "x"},
             "thoughtSignature": "skip_thought_signature_validator"},
            {"functionCall": {"name": "read", "args": {"path": "y"}, "id": "y"},
             "thoughtSignature": "skip_thought_signature_validator"},
        ])
    );
    assert_eq!(
        contents[2]["parts"],
        json!([
            {"functionResponse": {"name": "read", "id": "x", "response": {"output": "one"}}},
            {"functionResponse": {"name": "read", "id": "y", "response": {"output": "(see attached image)"}}},
        ])
    );
    assert_eq!(
        contents[3]["parts"],
        json!([
            {"text": "Tool result image:"},
            {"inlineData": {"mimeType": "image/png", "data": "aW1n"}},
        ])
    );
    assert_eq!(
        contents[4]["parts"],
        json!([
            {"thought": true, "text": "plan", "thoughtSignature": "c2lnMQ=="},
            {"text": "Hello", "thoughtSignature": "c2lnMg=="},
            {"functionCall": {"name": "read", "args": {"path": "a"}, "id": "fc1"},
             "thoughtSignature": "c2lnMw=="},
        ])
    );

    assert_eq!(
        requests[2].1["generationConfig"]["thinkingConfig"],
        json!({"includeThoughts": true, "thinkingBudget": 8192})
    );
}
