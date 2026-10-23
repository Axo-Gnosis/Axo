use std::io::Write;
use std::net::TcpListener;
use std::thread::JoinHandle;

use axo_ai::testing::read_http_request;
use axo_ai::{
    AnthropicApi, Api, ApiKind, AssistantEvent, AssistantMessage, CodexApi, Compat, CompletionsApi,
    Context, Cost, InputKind, Model, ModelAuth, SimpleStreamOptions, StreamOptions, ToolChoice,
    ToolSpec, Transcript,
};
use base64::Engine;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// One finite request: connection-close avoids a server task surviving a failed assertion.
fn server(status: u16, headers: &str, body: &str) -> (String, JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
        body.len()
    );
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let body = read_http_request(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
        body
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
        context_window: 200_000,
        max_tokens: 4096,
        headers: Default::default(),
        compat: Compat::default(),
    }
}

async fn request(api: ApiKind, url: String, choice: Option<ToolChoice>) -> AssistantMessage {
    let client = reqwest::Client::new();
    let adapter: Box<dyn Api> = match api {
        ApiKind::AnthropicMessages => Box::new(AnthropicApi::new(client)),
        ApiKind::OpenAiCompletions => Box::new(CompletionsApi::new(client)),
        ApiKind::OpenAiCodexResponses => Box::new(CodexApi::new(client)),
    };
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"test"}}"#);
    let events: Vec<_> = adapter
        .stream(
            &model(api, url),
            Transcript::normalize(Context {
                system_prompt: Some("test".into()),
                messages: vec![],
                tools: vec![ToolSpec {
                    name: "read".into(),
                    description: "Read".into(),
                    parameters: json!({"type":"object","properties":{}}),
                }],
            }),
            StreamOptions {
                simple: SimpleStreamOptions {
                    tool_choice: choice,
                    max_tokens: Some(42),
                    ..Default::default()
                },
                auth: ModelAuth {
                    api_key: Some(format!("header.{payload}.signature")),
                    ..Default::default()
                },
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
            let body = server.join().unwrap();
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
