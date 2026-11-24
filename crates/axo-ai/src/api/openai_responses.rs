//! OpenAI Responses API over SSE (`POST {base}/responses`), authenticated with a bearer API
//! key. Port of pi `api/openai-responses.ts` with the event mapping of
//! `openai-codex-responses.ts`, so providers that rename the terminal events (Codex) decode
//! too. Not ported: the WebSocket transport and `previous_response_id` continuation.

use std::ops::ControlFlow;

use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::http::{dispatch, read_sse};
use crate::api::responses::{ConvertOptions, ResponsesStream, convert_messages, convert_tools};
use crate::api::{
    Api, ProviderRequest, ProviderResponse, Reply, StreamOptions, header_map, respond,
};
use crate::event::AssistantEvent;
use crate::model::{ApiKind, Model, tier_wire};
use crate::thinking::clamp_level;
use crate::transcript::{Transcript, resolve_transcript_tools, system_message_text};
use crate::types::{ServiceTier, StopReason, ThinkingLevel};

/// pi `OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH`.
const PROMPT_CACHE_KEY_MAX_CHARS: usize = 64;
/// Statuses kept on terminal responses; anything else is treated as absent.
const RESPONSE_STATUSES: [&str; 6] = [
    "completed",
    "incomplete",
    "failed",
    "cancelled",
    "queued",
    "in_progress",
];

/// Streams OpenAI Responses replies.
pub struct ResponsesApi {
    http: reqwest::Client,
}

impl ResponsesApi {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

impl Api for ResponsesApi {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: StreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent> {
        let tier = model.effective_tier(opts.simple.service_tier);
        let request = build_request(model, &transcript, &opts, tier);
        let http = self.http.clone();
        let owned_model = model.clone();
        respond(model, tier, cancel, move |mut reply, cancel| async move {
            let result = async {
                let response = dispatch(
                    &http,
                    &opts,
                    &owned_model,
                    request?,
                    &cancel,
                    &mut reply,
                    describe,
                )
                .await?;
                drive(&owned_model, response, &cancel, &mut reply).await
            }
            .await;
            (reply, result)
        })
    }
}

fn build_request(
    model: &Model,
    transcript: &Transcript,
    opts: &StreamOptions,
    tier: ServiceTier,
) -> Result<ProviderRequest, String> {
    let token = opts.auth.api_key.as_deref().filter(|key| !key.is_empty());
    let session_id: Option<String> = opts
        .simple
        .session_id
        .as_deref()
        .map(|id| id.chars().take(PROMPT_CACHE_KEY_MAX_CHARS).collect());
    let body = build_request_body(model, transcript, opts, session_id.as_deref(), tier);
    let headers = header_map([
        vec![
            ("accept".to_owned(), Some("text/event-stream".to_owned())),
            (
                "content-type".to_owned(),
                Some("application/json".to_owned()),
            ),
        ],
        opts.configured_headers().into_iter().collect(),
        token
            .map(|token| ("authorization".to_owned(), Some(format!("Bearer {token}"))))
            .into_iter()
            .collect(),
    ])?;
    Ok(ProviderRequest {
        url: format!("{}/responses", opts.base_url(model)),
        headers,
        body,
    })
}

/// pi `buildRequestBody`, with `streamSimple`'s thinking-level handling.
fn build_request_body(
    model: &Model,
    transcript: &Transcript,
    opts: &StreamOptions,
    prompt_cache_key: Option<&str>,
    tier: ServiceTier,
) -> Value {
    let compat = &model.compat;
    let options = ConvertOptions {
        supports_mid_convo_system_messages: compat
            .supports_mid_convo_system_messages
            .unwrap_or(false),
        supports_strict_mode: compat.supports_strict_mode.unwrap_or(true),
    };
    let resolved = transcript.resolve(options.supports_mid_convo_system_messages);
    let support = model.tool_support();
    let request_tools = resolve_transcript_tools(&resolved, support).request_tools;
    let input = convert_messages(model, &resolved, &options);

    let mut body = json!({
        "model": model.id,
        "store": false,
        "stream": true,
        "input": input,
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(instructions) = resolved
        .initial_system_message()
        .map(system_message_text)
        .filter(|text| !text.is_empty())
    {
        body["instructions"] = json!(instructions);
    }
    if let Some(max_tokens) = opts.simple.max_tokens {
        body["max_output_tokens"] = json!(max_tokens);
    }
    if let Some(key) = prompt_cache_key {
        body["prompt_cache_key"] = json!(key);
    }
    body["tool_choice"] = json!(
        opts.simple
            .tool_choice
            .map_or("auto", crate::ToolChoice::as_str)
    );
    body["parallel_tool_calls"] = json!(true);
    if let Some(temperature) = opts.simple.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(name) = tier_wire(ApiKind::OpenAiResponses, tier) {
        body["service_tier"] = json!(name);
    }
    if !request_tools.is_empty() {
        body["tools"] = json!(convert_tools(
            &request_tools,
            options.supports_strict_mode,
            false
        ));
    }

    let map = model.thinking_level_map.as_ref();
    match clamp_level(model, opts.simple.thinking) {
        ThinkingLevel::Off => {
            if model.reasoning
                && let Some(effort) = match map.and_then(|map| map.get(&ThinkingLevel::Off)) {
                    Some(mapped) => mapped.as_deref(),
                    None => Some("none"),
                }
            {
                body["reasoning"] = json!({ "effort": effort });
            }
        }
        level => {
            if let Some(effort) = match map.and_then(|map| map.get(&level)) {
                Some(mapped) => mapped.as_deref(),
                None => Some(level.as_str()),
            } {
                body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
            }
        }
    }
    body
}

async fn drive(
    model: &Model,
    response: ProviderResponse,
    cancel: &CancellationToken,
    reply: &mut Reply,
) -> Result<StopReason, String> {
    reply.start();
    let mut stream = ResponsesStream::new(model);
    read_sse(response, cancel, |sse| {
        map_event(&mut stream, &sse.data, reply)
    })
    .await?;
    stream.finish(reply)?;
    Ok(reply.message.stop_reason)
}

/// pi `mapCodexEvents`: errors fail the response, and the terminal events
/// (`response.done`, `response.completed`, `response.incomplete`) become a
/// `response.completed` with a normalized status. Returns whether the event was terminal.
fn map_event(
    stream: &mut ResponsesStream<'_>,
    data: &str,
    reply: &mut Reply,
) -> Result<ControlFlow<()>, String> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(ControlFlow::Continue(()));
    }
    let mut event: Value = serde_json::from_str(data)
        .map_err(|error| format!("Invalid Responses SSE JSON: {error}"))?;
    let Some(kind) = event.get("type").and_then(Value::as_str) else {
        return Ok(ControlFlow::Continue(()));
    };
    match kind {
        "error" => {
            super::http::record_failure(&mut reply.message, &event);
            let field = |key: &str| {
                event
                    .get(key)
                    .or_else(|| event.get("error").and_then(|error| error.get(key)))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            };
            let detail = field("message")
                .or_else(|| field("code"))
                .map_or_else(|| event.to_string(), str::to_owned);
            Err(format!("Responses error: {detail}"))
        }
        "response.failed" => {
            super::http::record_failure(&mut reply.message, &event);
            Err(event
                .pointer("/response/error/message")
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
                .unwrap_or("Responses request failed")
                .to_owned())
        }
        "response.done" | "response.completed" | "response.incomplete" => {
            if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut)
                && !response
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| RESPONSE_STATUSES.contains(&status))
            {
                response.remove("status");
            }
            event["type"] = json!("response.completed");
            stream.handle(&event, reply)?;
            Ok(ControlFlow::Break(()))
        }
        _ => stream
            .handle(&event, reply)
            .map(|()| ControlFlow::Continue(())),
    }
}

/// The HTTP failure message: the body's `error.message`, else the raw body, else the
/// status's reason.
fn describe(status: u16, raw: &str, body: Option<&Value>) -> String {
    if let Some(message) = body
        .and_then(|body| body.pointer("/error/message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
    {
        return message.to_owned();
    }
    if !raw.is_empty() {
        return raw.to_owned();
    }
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("Request failed")
        .to_owned()
}
