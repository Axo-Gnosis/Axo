//! OpenAI Codex (ChatGPT subscription) Responses API over SSE. Port of pi
//! `api/openai-codex-responses.ts`: `buildRequestBody`, `mapCodexEvents`,
//! `parseErrorResponse` and the SSE headers. Not ported: the WebSocket transport, zstd
//! request compression, service-tier requests and `previous_response_id` continuation.

use std::ops::ControlFlow;

use futures::stream::BoxStream;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::http::{read_sse, send};
use crate::api::responses::{ConvertOptions, ResponsesStream, convert_messages, convert_tools};
use crate::api::{Api, Reply, StreamOptions, header_map, respond};
use crate::auth::codex::{BASE_URL, account_id};
use crate::event::AssistantEvent;
use crate::model::Model;
use crate::thinking::clamp_level;
use crate::transcript::{Transcript, resolve_transcript_tools, system_message_text};
use crate::types::{StopReason, ThinkingLevel, Usage, now_ms};

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

/// The Codex Responses endpoint, authenticated with a ChatGPT OAuth access token.
pub struct CodexApi {
    http: reqwest::Client,
}

impl CodexApi {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

/// A fully built request: everything that can fail before sending has failed here.
struct CodexRequest {
    url: String,
    headers: HeaderMap,
    body: String,
}

impl Api for CodexApi {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: StreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent> {
        let request = build_request(model, &transcript, &opts);
        let http = self.http.clone();
        let owned_model = model.clone();
        respond(model, cancel, move |mut reply, cancel| async move {
            let result = match request {
                Ok(request) => drive(&http, &owned_model, request, &cancel, &mut reply).await,
                Err(message) => Err(message),
            };
            (reply, result)
        })
    }
}

fn build_request(
    model: &Model,
    transcript: &Transcript,
    opts: &StreamOptions,
) -> Result<CodexRequest, String> {
    let token = opts
        .auth
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| format!("No API key for provider: {}", model.provider))?;
    let account_id =
        account_id(token).ok_or_else(|| "Failed to extract accountId from token".to_owned())?;
    let session_id: Option<String> = opts
        .simple
        .session_id
        .as_deref()
        .map(|id| id.chars().take(PROMPT_CACHE_KEY_MAX_CHARS).collect());
    let body = build_request_body(model, transcript, opts, session_id.as_deref());

    // Defaults first so auth and caller headers can override them; the auth and
    // protocol headers always win.
    let mut protocol_headers = vec![
        ("Authorization".to_owned(), Some(format!("Bearer {token}"))),
        ("chatgpt-account-id".to_owned(), Some(account_id)),
        (
            "OpenAI-Beta".to_owned(),
            Some("responses=experimental".to_owned()),
        ),
        ("accept".to_owned(), Some("text/event-stream".to_owned())),
        (
            "content-type".to_owned(),
            Some("application/json".to_owned()),
        ),
    ];
    if let Some(session_id) = session_id.filter(|id| !id.is_empty()) {
        protocol_headers.extend([
            ("session-id".to_owned(), Some(session_id.clone())),
            ("x-client-request-id".to_owned(), Some(session_id)),
        ]);
    }
    let headers = header_map([
        std::iter::once(("originator".to_owned(), Some("axo".to_owned())))
            .chain(opts.configured_headers())
            .chain(protocol_headers),
    ])?;

    let base_url = opts.base_url(model);
    let base_url = if base_url.trim().is_empty() {
        BASE_URL.trim_end_matches('/')
    } else {
        base_url
    };
    let url = if base_url.ends_with("/codex/responses") {
        base_url.to_owned()
    } else if base_url.ends_with("/codex") {
        format!("{base_url}/responses")
    } else {
        format!("{base_url}/codex/responses")
    };

    Ok(CodexRequest {
        url,
        headers,
        body: body.to_string(),
    })
}

/// pi `buildRequestBody`, with `streamSimple`'s thinking-level handling.
fn build_request_body(
    model: &Model,
    transcript: &Transcript,
    opts: &StreamOptions,
    prompt_cache_key: Option<&str>,
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

    let instructions = resolved
        .initial_system_message()
        .map(system_message_text)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "You are a helpful assistant.".to_owned());
    let mut body = json!({
        "model": model.id,
        "store": false,
        "stream": true,
        "instructions": instructions,
        "input": input,
        "text": { "verbosity": "low" },
        "include": ["reasoning.encrypted_content"],
    });
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
    http: &reqwest::Client,
    model: &Model,
    request: CodexRequest,
    cancel: &CancellationToken,
    reply: &mut Reply,
) -> Result<StopReason, String> {
    let response = send(
        || {
            http.post(&request.url)
                .headers(request.headers.clone())
                .body(request.body.clone())
        },
        cancel,
        reply,
        parse_error_response,
    )
    .await?;

    reply.start();
    let mut stream = ResponsesStream::new(model);
    read_sse(response, cancel, |sse| {
        map_codex_event(&mut stream, model, &sse.data, reply)
    })
    .await?;
    stream.finish(reply)?;
    Ok(reply.message.stop_reason)
}

/// pi `mapCodexEvents`: Codex errors fail the response, and its terminal events
/// (`response.done`, `response.completed`, `response.incomplete`) become a
/// `response.completed` with a normalized status. Returns whether the event was terminal.
fn map_codex_event(
    stream: &mut ResponsesStream<'_>,
    model: &Model,
    data: &str,
    reply: &mut Reply,
) -> Result<ControlFlow<()>, String> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(ControlFlow::Continue(()));
    }
    let mut event: Value =
        serde_json::from_str(data).map_err(|error| format!("Invalid Codex SSE JSON: {error}"))?;
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
            Err(format!("Codex error: {detail}"))
        }
        "response.failed" => {
            super::http::record_failure(&mut reply.message, &event);
            Err(event
                .pointer("/response/error/message")
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
                .unwrap_or("Codex response failed")
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
            apply_service_tier_pricing(
                model,
                event
                    .pointer("/response/service_tier")
                    .and_then(Value::as_str),
                &mut reply.message.usage,
            );
            Ok(ControlFlow::Break(()))
        }
        _ => stream
            .handle(&event, reply)
            .map(|()| ControlFlow::Continue(())),
    }
}

/// pi `parseErrorResponse`: the provider's error message, or a friendly usage-limit
/// message.
fn parse_error_response(status: u16, raw: &str, body: Option<&Value>) -> String {
    let mut message = if raw.is_empty() {
        reqwest::StatusCode::from_u16(status)
            .ok()
            .and_then(|status| status.canonical_reason())
            .unwrap_or("Request failed")
            .to_owned()
    } else {
        raw.to_owned()
    };
    let mut friendly = None;
    if let Some(error) = body
        .and_then(|body| body.get("error"))
        .filter(|error| error.is_object())
    {
        let field = |key: &str| {
            error
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        };
        let code = field("code")
            .or_else(|| field("type"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if [
            "usage_limit_reached",
            "usage_not_included",
            "rate_limit_exceeded",
        ]
        .iter()
        .any(|pattern| code.contains(pattern))
            || status == 429
        {
            let plan = field("plan_type")
                .map(|plan| format!(" ({} plan)", plan.to_lowercase()))
                .unwrap_or_default();
            let when = error
                .get("resets_at")
                .and_then(Value::as_f64)
                .filter(|resets_at| *resets_at != 0.0)
                .map(|resets_at| {
                    // Math.round: halves round up.
                    let minutes = ((resets_at * 1000.0 - now_ms() as f64) / 60_000.0 + 0.5).floor();
                    format!(" Try again in ~{} min.", minutes.max(0.0) as i64)
                })
                .unwrap_or_default();
            friendly = Some(
                format!("You have hit your ChatGPT usage limit{plan}.{when}")
                    .trim()
                    .to_owned(),
            );
        }
        message = field("message")
            .map(str::to_owned)
            .or_else(|| friendly.clone())
            .unwrap_or(message);
    }
    match friendly {
        Some(friendly) => format!("{friendly}\n{raw}"),
        None => message,
    }
}

/// pi `applyServiceTierPricing` for the tier the response reports.
fn apply_service_tier_pricing(model: &Model, service_tier: Option<&str>, usage: &mut Usage) {
    let multiplier = match service_tier {
        Some("flex") => 0.5,
        Some("priority") if model.id == "gpt-5.5" => 2.5,
        Some("priority") => 2.0,
        _ => return,
    };
    let cost = &mut usage.cost;
    cost.input *= multiplier;
    cost.output *= multiplier;
    cost.cache_read *= multiplier;
    cost.cache_write *= multiplier;
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
}
