use super::*;
use std::collections::HashSet;

use futures::StreamExt;

use crate::api::ToolChoice;
use crate::api::sse::SseDecoder;
use crate::model::{Compat, InputKind};
use crate::types::{SystemMessage, TextBlock};

fn model() -> Model {
    Model {
        id: "claude-sonnet-4-5".into(),
        name: "Prefix fixture".into(),
        api: ApiKind::AnthropicMessages,
        provider: "lab".into(),
        base_url: "https://api.anthropic.com".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: None,
        service_tiers: Default::default(),
        context_window: 200_000,
        max_tokens: 4096,
        compat: Compat {
            supports_mid_convo_system_messages: Some(true),
            supports_mid_convo_tool_changes: Some(true),
            supports_eager_tool_input_streaming: Some(false),
            tool_name_prefix: Some("_".into()),
            ..Default::default()
        },
    }
}

fn options() -> StreamOptions {
    let mut opts = StreamOptions::default();
    opts.auth.api_key = Some("synthetic-access".into());
    opts
}

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("Run {name}"),
        parameters: json!({
            "type": "object",
            "properties": {"name": {"type": "string", "enum": ["read", "_read"]}},
            "required": ["name"]
        }),
    }
}

fn transcript(names: &[&str]) -> Transcript {
    Transcript::new(vec![Message::System(SystemMessage {
        content: vec![
            TextBlock {
                text: "Host instructions.".into(),
            },
            TextBlock {
                text: "Keep the trust boundary.".into(),
            },
        ],
        tools_added: names.iter().map(|name| tool(name)).collect(),
        ..Default::default()
    })])
}

fn prepare(model: &Model, transcript: &Transcript) -> PreparedRequest {
    prepare_request(model, transcript, &options(), ServiceTier::Standard).unwrap()
}

// Exercise the production parser and mapper within the same terminal-event owner as HTTP.
async fn decode(model: &Model, prepared: PreparedRequest, name: &str) -> Vec<AssistantEvent> {
    let frames = [
        json!({"type": "message_start", "message": {"id": "msg_fixture", "usage": {}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {
            "type": "tool_use", "id": "call_read", "name": name, "input": {}
        }}),
        json!({"type": "content_block_delta", "index": 0, "delta": {
            "type": "input_json_delta", "partial_json": "{\"name\":\"read\"}"
        }}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {}}),
        json!({"type": "message_stop"}),
    ];
    let wire = frames
        .iter()
        .map(|frame| {
            format!(
                "event: {}\r\ndata: {frame}\r\n\r\n",
                frame["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    let owned_model = model.clone();
    respond(
        model,
        ServiceTier::Standard,
        CancellationToken::new(),
        move |mut reply, _| async move {
            reply.start();
            let mut blocks = BlockMapper {
                tool_names: prepared.tool_names,
                ..Default::default()
            };
            let mut decoder = SseDecoder::new();
            let result = (|| {
                for chunk in wire.as_bytes().chunks(7) {
                    for event in decoder.push(chunk) {
                        blocks.handle(&event, &owned_model, &mut reply)?;
                    }
                }
                if let Some(event) = decoder.finish() {
                    blocks.handle(&event, &owned_model, &mut reply)?;
                }
                assert!(blocks.saw_start && blocks.saw_stop);
                blocks
                    .stop
                    .ok_or_else(|| "Missing terminal reason".to_owned())
            })();
            (reply, result)
        },
    )
    .collect()
    .await
}

fn terminal_call(events: &[AssistantEvent], expected: &str) -> AssistantMessage {
    assert!(matches!(events.first(), Some(AssistantEvent::Start { .. })));
    let starts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AssistantEvent::ToolCallStart { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(starts, [("call_read", expected)]);
    let Some(AssistantEvent::Done { message, .. }) = events.last() else {
        panic!("Expected successful terminal event: {events:?}");
    };
    let calls: Vec<_> = message.tool_calls().collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, expected);
    assert_eq!(calls[0].id, "call_read");
    assert_eq!(
        calls[0].arguments,
        json!({"name": "read"}).as_object().unwrap().clone()
    );
    message.clone()
}

fn wire_blocks(body: &Value) -> impl Iterator<Item = &Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
}

#[tokio::test]
async fn tool_name_prefix_round_trips_through_history() {
    let model = model();
    let original = transcript(&["read", "bash"]);
    let saved = original.clone();
    let prepared = prepare(&model, &original);
    let request = &prepared.request;
    assert_eq!(
        request.url,
        "https://api.anthropic.com/v1/messages?beta=true"
    );
    assert_eq!(request.headers["x-api-key"], "synthetic-access");
    let body = &request.body;
    assert_eq!(body["tools"][0]["name"], "_read");
    assert_eq!(body["tools"][1]["name"], "_bash");
    assert_eq!(body["tools"][0]["input_schema"], tool("read").parameters);
    assert_eq!(
        body["tools"][1]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(original, saved, "Wire names must not modify history");
    let assistant = terminal_call(&decode(&model, prepared, "_read").await, "read");
    let mut messages = original.clone().into_messages();
    messages.push(Message::Assistant(assistant));
    messages.push(Message::ToolResult(ToolResultMessage {
        tool_call_id: "call_read".into(),
        tool_name: "read".into(),
        content: vec![UserBlock::Text {
            text: "file contents".into(),
        }],
        details: None,
        is_error: false,
        timestamp: 0,
        duration_ms: None,
    }));
    let history = Transcript::new(messages);
    let next = prepare(&model, &history);
    let blocks: Vec<_> = wire_blocks(&next.request.body).collect();
    assert_eq!(blocks[0]["type"], "tool_use");
    assert_eq!(blocks[0]["name"], "_read");
    assert_eq!(blocks[0]["input"], json!({"name": "read"}));
    assert_eq!(blocks[1]["type"], "tool_result");
    assert_eq!(blocks[1]["tool_use_id"], blocks[0]["id"]);
    assert_eq!(blocks[1]["content"], "file contents");
    let mut inactive_history = history.into_messages();
    inactive_history.push(Message::System(SystemMessage {
        tools_removed: vec!["read".into()],
        ..Default::default()
    }));
    let inactive = prepare(&model, &Transcript::new(inactive_history));
    assert!(
        wire_blocks(&inactive.request.body)
            .any(|block| block["type"] == "tool_use" && block["name"] == "_read")
    );
    assert!(!inactive.tool_names.as_ref().unwrap().contains_key("_read"));

    // Without a prefix, names stay canonical.
    let mut model = model.clone();
    model.compat.tool_name_prefix = None;
    let plain = prepare(&model, &original);
    assert!(plain.tool_names.is_none());
    assert_eq!(plain.request.body["tools"][0]["name"], "read");
    terminal_call(&decode(&model, plain, "read").await, "read");
    for choice in [ToolChoice::Auto, ToolChoice::None] {
        let mut opts = options();
        opts.simple.tool_choice = Some(choice);
        let request = prepare_request(&model, &original, &opts, ServiceTier::Standard).unwrap();
        assert_eq!(request.request.body["tool_choice"]["type"], choice.as_str());
    }
}

#[tokio::test]
async fn tool_name_prefix_inline_changes_and_inactive_tools() {
    let model = model();
    let mut messages = transcript(&["read", "_read", "Read"]).into_messages();
    messages.push(Message::System(SystemMessage {
        tools_removed: vec!["read".into()],
        ..Default::default()
    }));
    let removed = Transcript::new(messages.clone());
    let prepared = prepare(&model, &removed);
    assert_eq!(prepared.request.body["tools"][0]["name"], "_read");
    assert_eq!(prepared.request.body["tools"][1]["name"], "__read");
    assert_eq!(prepared.request.body["tools"][2]["name"], "_Read");
    assert!(
        wire_blocks(&prepared.request.body)
            .any(|block| block["type"] == "tool_removal" && block["tool"]["name"] == "_read")
    );
    assert!(!prepared.tool_names.as_ref().unwrap().contains_key("_read"));
    for bad in ["_read", "read", "_unknown", "__deferred_placeholder__"] {
        let events = decode(&model, prepare(&model, &removed), bad).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AssistantEvent::ToolCallStart { .. }))
        );
        let Some(AssistantEvent::Error { message, .. }) = events.last() else {
            panic!("Unknown/inactive name {bad} was accepted: {events:?}");
        };
        assert_eq!(message.tool_calls().count(), 0);
    }
    terminal_call(
        &decode(&model, prepare(&model, &removed), "__read").await,
        "_read",
    );
    terminal_call(
        &decode(&model, prepare(&model, &removed), "_Read").await,
        "Read",
    );

    let mut replacement = tool("read");
    replacement.description = "Re-added read".into();
    messages.push(Message::System(SystemMessage {
        tools_added: vec![replacement.clone()],
        ..Default::default()
    }));
    let readded = Transcript::new(messages);
    let prepared = prepare(&model, &readded);
    let addition = wire_blocks(&prepared.request.body)
        .find(|block| block["type"] == "tool_addition")
        .unwrap();
    assert_eq!(addition["tool"]["definition"]["name"], "_read");
    assert_eq!(
        addition["tool"]["definition"]["description"],
        replacement.description
    );
    assert_eq!(
        addition["tool"]["definition"]["input_schema"],
        replacement.parameters
    );
    terminal_call(&decode(&model, prepared, "_read").await, "read");
}

#[tokio::test]
async fn tool_name_prefix_long_names_and_protocol_placeholder_are_distinct() {
    let model = model();
    let long = "a".repeat(64);
    let boundary = "a".repeat(63);
    let original = transcript(&[
        &long,
        &boundary,
        "_deferred_placeholder__",
        "__deferred_placeholder__",
    ]);
    let prepared = prepare(&model, &original);
    let names: Vec<_> = prepared.request.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "t-_-BU_nrgy23GXDr5th1SCfQ5hR20PQulmXM33xVGaOs",
            &format!("_{boundary}"),
            "t-CPQUL1OAWz4zFSH63pXzvUlR3EKq0f-V8TlqwdF9oWg",
            "___deferred_placeholder__",
            "__deferred_placeholder__",
        ]
    );
    assert_eq!(names.iter().collect::<HashSet<_>>().len(), names.len());
    for prefix in ["", "a b", "a."] {
        let mut invalid = model.clone();
        invalid.compat.tool_name_prefix = Some(prefix.into());
        assert_eq!(
            prepare_request(&invalid, &original, &options(), ServiceTier::Standard)
                .err()
                .as_deref(),
            Some("Invalid toolNamePrefix")
        );
    }
    assert!(names.iter().all(|name| name.len() <= 64));
    assert_eq!(prepared.request.body["tools"][4]["defer_loading"], true);
    let active = prepared.tool_names.as_ref().unwrap();
    assert_eq!(active.len(), 4);
    assert!(!active.contains_key("__deferred_placeholder__"));
    for (wire, canonical) in names[..4].iter().zip([
        long.as_str(),
        boundary.as_str(),
        "_deferred_placeholder__",
        "__deferred_placeholder__",
    ]) {
        terminal_call(
            &decode(&model, prepare(&model, &original), wire).await,
            canonical,
        );
    }
}
