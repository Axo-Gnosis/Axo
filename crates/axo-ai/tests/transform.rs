//! `transform_messages`: cross-model replay rules.

use axo_ai::{
    ApiKind, AssistantBlock, AssistantMessage, Compat, Context, Cost, InputKind, Message, Model,
    ReasoningReplay, StopReason, ToolCall, ToolResultMessage, Transcript, UserBlock, UserMessage,
    transform_messages,
};
use serde_json::json;

fn model(api: ApiKind, provider: &str, id: &str, input: Vec<InputKind>) -> Model {
    Model {
        id: id.to_owned(),
        name: id.to_owned(),
        api,
        provider: provider.to_owned(),
        base_url: "http://localhost".to_owned(),
        reasoning: true,
        thinking_level_map: None,
        input,
        cost: Cost::default(),
        context_window: 200_000,
        max_tokens: 32_000,
        headers: Default::default(),
        compat: Compat::default(),
    }
}

fn claude() -> Model {
    model(
        ApiKind::AnthropicMessages,
        "anthropic",
        "claude-haiku-5-5",
        vec![InputKind::Text, InputKind::Image],
    )
}

fn qwen() -> Model {
    model(
        ApiKind::OpenAiCompletions,
        "zebes",
        "qwen3.8-27b",
        vec![InputKind::Text, InputKind::Image],
    )
}

fn assistant(from: &Model, content: Vec<AssistantBlock>, stop_reason: StopReason) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        stop_reason,
        ..AssistantMessage::empty(from)
    })
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        content: vec![UserBlock::Text {
            text: text.to_owned(),
        }],
        timestamp: 0,
    })
}

fn tool_call(id: &str) -> AssistantBlock {
    AssistantBlock::ToolCall(ToolCall {
        id: id.to_owned(),
        name: "get_weather".to_owned(),
        arguments: json!({ "city": "Paris" }).as_object().unwrap().clone(),
        arguments_error: None,
    })
}

fn tool_result(id: &str, content: Vec<UserBlock>) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.to_owned(),
        tool_name: "get_weather".to_owned(),
        content,
        details: None,
        is_error: false,
        timestamp: 0,
        duration_ms: None,
    })
}

fn transcript(messages: Vec<Message>) -> Transcript {
    Transcript::normalize(Context {
        messages,
        ..Context::default()
    })
}

fn text(text: &str) -> AssistantBlock {
    AssistantBlock::Text {
        text: text.to_owned(),
        text_signature: None,
    }
}

#[test]
fn foreign_model_thinking_is_dropped() {
    let target = claude();
    let foreign_thinking = AssistantBlock::Thinking {
        thinking: "qwen reasoning".to_owned(),
        replay: Some(ReasoningReplay::ReasoningField(
            "reasoning_content".to_owned(),
        )),
        redacted: false,
    };
    let own_thinking = AssistantBlock::Thinking {
        thinking: "claude reasoning".to_owned(),
        replay: Some(ReasoningReplay::AnthropicSignature("sig".to_owned())),
        redacted: false,
    };
    let input = transcript(vec![
        user("one"),
        assistant(
            &qwen(),
            vec![foreign_thinking, text("from qwen")],
            StopReason::Stop,
        ),
        user("two"),
        assistant(
            &target,
            vec![own_thinking.clone(), text("from claude")],
            StopReason::Stop,
        ),
    ]);

    let output = transform_messages(input, &target);

    let contents: Vec<&Vec<AssistantBlock>> = output
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(&assistant.content),
            _ => None,
        })
        .collect();
    assert_eq!(
        contents,
        [
            &vec![text("from qwen")],
            &vec![own_thinking, text("from claude")]
        ]
    );
}

#[test]
fn errored_and_aborted_assistant_turns_are_omitted() {
    let target = claude();
    let input = transcript(vec![
        user("one"),
        assistant(&target, vec![text("partial")], StopReason::Error),
        user("two"),
        assistant(&target, vec![text("cut off")], StopReason::Aborted),
        user("three"),
        assistant(&target, vec![text("answer")], StopReason::Stop),
    ]);

    let output = transform_messages(input.clone(), &target);

    let expected = [&input[0], &input[2], &input[4], &input[5]].map(Clone::clone);
    assert_eq!(output.messages(), expected);
}

#[test]
fn pipe_tool_call_ids_are_normalized_for_anthropic() {
    let target = claude();
    let codex = model(
        ApiKind::OpenAiCodexResponses,
        "openai-codex",
        "gpt-6-sol",
        vec![InputKind::Text],
    );
    let input = transcript(vec![
        user("weather?"),
        assistant(
            &codex,
            vec![tool_call("call_abc|fc_123")],
            StopReason::ToolUse,
        ),
        tool_result(
            "call_abc|fc_123",
            vec![UserBlock::Text {
                text: "21".to_owned(),
            }],
        ),
    ]);

    let output = transform_messages(input, &target);

    let Message::Assistant(replayed) = &output[1] else {
        panic!("expected assistant, got {:?}", output[1]);
    };
    let call_ids: Vec<&str> = replayed.tool_calls().map(|call| call.id.as_str()).collect();
    assert_eq!(call_ids, ["call_abc_fc_123"]);
    let Message::ToolResult(result) = &output[2] else {
        panic!("expected tool result, got {:?}", output[2]);
    };
    assert_eq!(result.tool_call_id, "call_abc_fc_123");
    assert_eq!(output.len(), 3);
}

#[test]
fn orphan_tool_call_gets_no_result_provided() {
    let target = claude();
    let input = transcript(vec![
        user("weather?"),
        assistant(
            &target,
            vec![tool_call("call_1"), tool_call("call_2")],
            StopReason::ToolUse,
        ),
        tool_result(
            "call_1",
            vec![UserBlock::Text {
                text: "21".to_owned(),
            }],
        ),
        user("next"),
    ]);

    let output = transform_messages(input, &target);

    assert_eq!(output.len(), 5);
    let Message::ToolResult(synthetic) = &output[3] else {
        panic!("expected synthetic tool result, got {:?}", output[3]);
    };
    assert_eq!(synthetic.tool_call_id, "call_2");
    assert_eq!(synthetic.tool_name, "get_weather");
    assert!(synthetic.is_error);
    assert_eq!(
        synthetic.content,
        [UserBlock::Text {
            text: "No result provided".to_owned(),
        }]
    );
    assert_eq!(output[4], user("next"));
}

#[test]
fn images_become_placeholders_for_text_only_model() {
    let target = model(
        ApiKind::OpenAiCompletions,
        "zebes",
        "text-only",
        vec![InputKind::Text],
    );
    let image = || UserBlock::Image {
        data: "aGVsbG8=".into(),
        mime_type: "image/png".to_owned(),
    };
    let input = transcript(vec![
        Message::User(UserMessage {
            content: vec![
                UserBlock::Text {
                    text: "look".to_owned(),
                },
                image(),
                image(),
            ],
            timestamp: 0,
        }),
        assistant(&target, vec![tool_call("call_1")], StopReason::ToolUse),
        tool_result("call_1", vec![image()]),
    ]);

    let output = transform_messages(input, &target);

    let Message::User(user) = &output[0] else {
        panic!("expected user, got {:?}", output[0]);
    };
    assert_eq!(
        user.content,
        [
            UserBlock::Text {
                text: "look".to_owned(),
            },
            UserBlock::Text {
                text: "(image omitted: model does not support images)".to_owned(),
            },
        ]
    );
    let Message::ToolResult(result) = &output[2] else {
        panic!("expected tool result, got {:?}", output[2]);
    };
    assert_eq!(
        result.content,
        [UserBlock::Text {
            text: "(tool image omitted: model does not support images)".to_owned(),
        }]
    );
}
