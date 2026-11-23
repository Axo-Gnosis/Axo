use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpListener;
use std::thread::JoinHandle;

use axo_ai::testing::read_http_request_parts;
use axo_ai::{
    AnthropicApi, Api, ApiKind, AssistantEvent, AssistantMessage, CodexApi, Compat, CompletionsApi,
    Cost, InputKind, Message, Model, ModelAuth, ServiceTier, SimpleStreamOptions, StreamOptions,
    SystemMessage, TextBlock, ToolChoice, ToolSpec, Transcript,
};
use base64::Engine;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// One finite request: connection-close avoids a server task surviving a failed assertion.
fn server(status: u16, headers: &str, body: &str) -> (String, JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
        body.len()
    );
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request_parts(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
        request
    });
    (url, thread)
}

fn model(api: ApiKind, base_url: String) -> Model {
    Model {
        id: "test".into(),
        name: "Test".into(),
        api,
        provider: "test".into(),
        base_url,
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputKind::Text],
        cost: Cost::default(),
        service_tiers: Default::default(),
        context_window: 200_000,
        max_tokens: 4096,
        compat: Compat::default(),
    }
}

async fn request(api: ApiKind, url: String, choice: Option<ToolChoice>) -> AssistantMessage {
    request_with(
        model(api, url),
        SimpleStreamOptions {
            tool_choice: choice,
            ..Default::default()
        },
        BTreeMap::new(),
    )
    .await
}

async fn request_with(
    model: Model,
    simple: SimpleStreamOptions,
    headers: BTreeMap<String, String>,
) -> AssistantMessage {
    let client = reqwest::Client::new();
    let adapter: Box<dyn Api> = match model.api {
        ApiKind::AnthropicMessages => Box::new(AnthropicApi::new(client)),
        ApiKind::OpenAiCompletions => Box::new(CompletionsApi::new(client)),
        ApiKind::OpenAiCodexResponses => Box::new(CodexApi::new(client)),
    };
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"test"}}"#);
    let events: Vec<_> = adapter
        .stream(
            &model,
            Transcript::new(vec![Message::System(SystemMessage {
                content: vec![TextBlock {
                    text: "test".into(),
                }],
                tools_added: vec![ToolSpec {
                    name: "read".into(),
                    description: "Read".into(),
                    parameters: json!({"type":"object","properties":{}}),
                }],
                ..Default::default()
            })]),
            StreamOptions {
                simple: SimpleStreamOptions {
                    max_tokens: Some(42),
                    ..simple
                },
                auth: ModelAuth {
                    api_key: Some(format!("header.{payload}.signature")),
                    headers,
                    ..Default::default()
                },
                rewrite: None,
            },
            CancellationToken::new(),
        )
        .collect()
        .await;
    match events.into_iter().last().unwrap() {
        AssistantEvent::Error { message, .. } | AssistantEvent::Done { message, .. } => message,
        event => panic!("not terminal: {event:?}"),
    }
}

#[tokio::test]
async fn tool_disabling_keeps_definitions_and_supported_wire_fields() {
    for api in [
        ApiKind::AnthropicMessages,
        ApiKind::OpenAiCompletions,
        ApiKind::OpenAiCodexResponses,
    ] {
        for choice in [None, Some(ToolChoice::None), Some(ToolChoice::Auto)] {
            let (url, server) = server(
                400,
                "Content-Type: application/json\r\n",
                r#"{"error":{"message":"unsupported request"}}"#,
            );
            request(api, url, choice).await;
            let (_, body) = server.join().unwrap();
            assert!(!body["tools"].as_array().unwrap().is_empty());
            let expected = match choice {
                Some(ToolChoice::None) => Some("none"),
                Some(ToolChoice::Auto) => Some("auto"),
                None if api == ApiKind::OpenAiCodexResponses => Some("auto"),
                None => None,
            };
            let actual = if api == ApiKind::AnthropicMessages {
                body["tool_choice"]["type"].as_str()
            } else {
                body["tool_choice"].as_str()
            };
            assert_eq!(actual, expected);
            if api == ApiKind::OpenAiCodexResponses {
                assert!(body.get("max_output_tokens").is_none());
                assert!(body.get("max_tokens").is_none());
                assert_eq!(body["parallel_tool_calls"], true);
            }
        }
    }
}

#[tokio::test]
async fn final_http_metadata_survives_body_consumption_and_friendly_errors() {
    for api in [
        ApiKind::AnthropicMessages,
        ApiKind::OpenAiCompletions,
        ApiKind::OpenAiCodexResponses,
    ] {
        let (url, server) = server(
            429,
            "Content-Type: application/json\r\nx-should-retry: false\r\nretry-after-ms: 1234\r\nretry-after: 99\r\n",
            r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"original provider diagnostic"}}"#,
        );
        let message = request(api, url, Some(ToolChoice::None)).await;
        server.join().unwrap();
        let failure = message.failure.as_ref().unwrap();
        assert_eq!(failure.status, Some(429));
        assert_eq!(
            failure.code.as_deref(),
            Some("subscription_sharing_usage_limit_exceeded")
        );
        assert_eq!(failure.retry_after_ms, Some(1234));
        assert_eq!(failure.should_retry, Some(false));
        assert!(
            message
                .error_message
                .as_ref()
                .unwrap()
                .contains("original provider diagnostic")
        );
        assert!(!axo_ai::is_retryable_error(&message));
    }
}

#[tokio::test]
async fn sse_errors_preserve_codes_including_codex_interception() {
    for (api, event) in [
        (
            ApiKind::AnthropicMessages,
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n",
        ),
        (
            ApiKind::OpenAiCompletions,
            "data: {\"error\":{\"code\":\"upstream_stream_read_error\",\"message\":\"busy\"}}\n\n",
        ),
        (
            ApiKind::OpenAiCodexResponses,
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"upstream_stream_read_error\",\"message\":\"busy\"}}}\n\n",
        ),
        (
            ApiKind::OpenAiCodexResponses,
            "data: {\"type\":\"error\",\"code\":\"upstream_stream_read_error\",\"message\":\"busy\"}\n\n",
        ),
    ] {
        let (url, server) = server(200, "Content-Type: text/event-stream\r\n", event);
        let message = request(api, url, None).await;
        server.join().unwrap();
        assert!(message.failure.as_ref().unwrap().code.is_some());
        assert!(axo_ai::is_retryable_error(&message));
    }
}

#[tokio::test]
async fn codex_preserves_context_overflow_failure() {
    let (url, server) = server(
        200,
        "Content-Type: text/event-stream\r\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"too large\"}}}\n\n",
    );
    let output = request(ApiKind::OpenAiCodexResponses, url, None).await;
    server.join().unwrap();
    assert_eq!(output.stop_reason, axo_ai::StopReason::Error);
    assert_eq!(
        output.failure.as_ref().unwrap().code.as_deref(),
        Some("context_length_exceeded")
    );
    assert!(axo_ai::is_context_overflow(&output, 200_000));
}

#[tokio::test]
async fn codex_account_id_header_comes_from_the_credential_else_the_token() {
    for (headers, expected) in [
        (
            BTreeMap::from([(
                "chatgpt-account-id".to_owned(),
                "from-credential".to_owned(),
            )]),
            "from-credential",
        ),
        (BTreeMap::new(), "test"),
    ] {
        let (url, server) = server(400, "", r#"{"error":{"message":"stop"}}"#);
        request_with(
            model(ApiKind::OpenAiCodexResponses, url),
            SimpleStreamOptions::default(),
            headers,
        )
        .await;
        let (head, _) = server.join().unwrap();
        let values: Vec<_> = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case("chatgpt-account-id"))
            .map(|(_, value)| value.trim())
            .collect();
        assert_eq!(values, [expected]);
    }
}

#[tokio::test]
async fn service_tiers_reach_the_wire_and_the_bill() {
    use ServiceTier::{Fast, Standard};

    for api in [
        ApiKind::OpenAiCodexResponses,
        ApiKind::OpenAiCompletions,
        ApiKind::AnthropicMessages,
    ] {
        let anthropic = api == ApiKind::AnthropicMessages;
        // Anthropic need not report a speed on standard requests. OpenAI explicitly
        // reports "default", even when asked for priority.
        let baseline_choice = if anthropic { Standard } else { Fast };
        for (chosen, supported, served) in [
            (baseline_choice, true, Standard),
            (Fast, true, Fast),
            (Standard, true, Fast),
            (Fast, false, Standard),
        ] {
            let echo = if served == Fast {
                "priority"
            } else {
                "default"
            };
            let response = match api {
                ApiKind::OpenAiCodexResponses => {
                    let event = json!({"type": "response.completed", "response": {
                        "status": "completed", "service_tier": echo, "usage": {"input_tokens": 4}
                    }});
                    format!("data: {event}\n\n")
                }
                ApiKind::OpenAiCompletions => {
                    let finish =
                        json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]});
                    let usage = json!({"prompt_tokens": 4});
                    let billing = if supported && chosen != served {
                        let counts = if chosen == Fast {
                            json!({"usage": usage})
                        } else {
                            json!({"choices": [{"usage": usage}]})
                        };
                        let tier = json!({"service_tier": echo});
                        format!("data: {counts}\n\ndata: {tier}\n\n")
                    } else {
                        let chunk = json!({"service_tier": echo, "usage": usage});
                        format!("data: {chunk}\n\n")
                    };
                    format!("{billing}data: {finish}\n\ndata: [DONE]\n\n")
                }
                ApiKind::AnthropicMessages => {
                    let mut usage = json!({"input_tokens": 4});
                    if served == Fast {
                        usage["speed"] = json!("fast");
                    }
                    let start = json!({"type": "message_start", "message": {"usage": usage}});
                    let delta =
                        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}});
                    format!(
                        "event: message_start\ndata: {start}\n\nevent: message_delta\ndata: {delta}\n\n\
                         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
                    )
                }
            };
            let (url, server) = server(200, "Content-Type: text/event-stream\r\n", &response);
            let mut model = model(api, url);
            model.cost.input = 1_000_000.0; // One dollar per input token.
            if supported {
                model.service_tiers.insert(Fast, 2.0);
            }
            let options = SimpleStreamOptions {
                service_tier: chosen,
                ..Default::default()
            };
            let message = request_with(model, options, BTreeMap::new()).await;
            let (head, body) = server.join().unwrap();
            let fast = chosen == Fast && supported;
            let (field, value) = if anthropic {
                let beta =
                    head.lines()
                        .filter_map(|line| line.split_once(':'))
                        .any(|(name, values)| {
                            name.eq_ignore_ascii_case("anthropic-beta")
                                && values
                                    .split(',')
                                    .any(|v| v.trim() == "fast-mode-2026-02-01")
                        });
                assert_eq!(beta, fast);
                ("speed", "fast")
            } else {
                ("service_tier", "priority")
            };
            assert_eq!(
                body.get(field),
                fast.then(|| json!(value)).as_ref(),
                "{api:?}"
            );
            assert_eq!(message.stop_reason, axo_ai::StopReason::Stop, "{message:?}");
            assert_eq!(message.service_tier, served, "{api:?}");
            assert_eq!(message.usage.input, 4, "{api:?}");
            assert_eq!(
                message.usage.cost.total,
                if served == Fast { 8.0 } else { 4.0 },
                "{api:?}"
            );
        }
    }
}
