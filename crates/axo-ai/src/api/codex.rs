//! OpenAI Codex (ChatGPT subscription) Responses API over SSE. Port of pi
//! `api/openai-codex-responses.ts`: `buildRequestBody`, `mapCodexEvents`,
//! `parseErrorResponse` and the SSE headers. Not ported: the WebSocket transport, zstd
//! request compression, service-tier requests and `previous_response_id` continuation.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD_INDIFFERENT;
use futures::channel::mpsc::{self, UnboundedSender};
use futures::stream::{self, BoxStream};
use futures::{StreamExt, future};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::http::{RetryPolicy, error_body, send_with_retry};
use crate::api::responses::{ConvertOptions, ResponsesStream, convert_messages, convert_tools};
use crate::api::sse::SseDecoder;
use crate::api::{ABORTED, Api, StreamOptions};
use crate::error::Error;
use crate::event::AssistantEvent;
use crate::model::Model;
use crate::thinking::clamp_level;
use crate::transcript::{
    Transcript, TranscriptToolSupport, resolve_transcript_tools, system_message_text,
};
use crate::types::{AssistantMessage, StopReason, ThinkingLevel, Usage, now_ms};

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
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
        let model = model.clone();
        let (tx, rx) = mpsc::unbounded();
        // The producer runs while the consumer polls; dropping the stream drops the request.
        let producer = async move { run(&http, &model, request, &cancel, &tx).await };
        stream::select(
            rx,
            stream::once(producer).filter_map(|()| future::ready(None)),
        )
        .boxed()
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
    let account_id = extract_account_id(token)?;
    let session_id: Option<String> = opts
        .simple
        .session_id
        .as_deref()
        .map(|id| id.chars().take(PROMPT_CACHE_KEY_MAX_CHARS).collect());
    let body = build_request_body(model, transcript, opts, session_id.as_deref());

    // Defaults first so model, auth and caller headers can override them; the auth and
    // protocol headers always win.
    let bearer = format!("Bearer {token}");
    let mut ops: Vec<(&str, Option<&str>)> = vec![("originator", Some("axo"))];
    ops.extend(
        model
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), Some(value.as_str()))),
    );
    ops.extend(
        opts.auth
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), Some(value.as_str()))),
    );
    ops.extend(
        opts.simple
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_deref())),
    );
    ops.extend([
        ("Authorization", Some(bearer.as_str())),
        ("chatgpt-account-id", Some(account_id.as_str())),
        ("OpenAI-Beta", Some("responses=experimental")),
        ("accept", Some("text/event-stream")),
        ("content-type", Some("application/json")),
    ]);
    if let Some(session_id) = session_id.as_deref().filter(|id| !id.is_empty()) {
        ops.extend([
            ("session-id", Some(session_id)),
            ("x-client-request-id", Some(session_id)),
        ]);
    }
    let mut headers = HeaderMap::new();
    for (name, value) in ops {
        let header = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("Invalid header name: {name}"))?;
        match value {
            Some(value) => {
                let value = HeaderValue::from_str(value)
                    .map_err(|_| format!("Invalid value for header: {name}"))?;
                headers.insert(header, value);
            }
            None => {
                headers.remove(header);
            }
        }
    }

    let base_url = opts.auth.base_url.as_deref().unwrap_or(&model.base_url);
    let base_url = if base_url.trim().is_empty() {
        DEFAULT_BASE_URL
    } else {
        base_url
    };
    let base_url = base_url.trim_end_matches('/');
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
        include_system_prompt: false,
        supports_mid_convo_system_messages: compat
            .supports_mid_convo_system_messages
            .unwrap_or(false),
        supports_additional_tools: compat.supports_additional_tools.unwrap_or(false),
        supports_tool_search: compat.supports_tool_search.unwrap_or(false),
        supports_strict_mode: compat.supports_strict_mode.unwrap_or(true),
    };
    let resolved = transcript.resolve(options.supports_mid_convo_system_messages);
    let support: TranscriptToolSupport = options.tool_support();
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
    body["tool_choice"] = json!("auto");
    body["parallel_tool_calls"] = json!(true);
    if let Some(temperature) = opts.simple.temperature {
        // Through the shortest decimal form, so 0.7f32 is sent as 0.7.
        body["temperature"] = json!(
            temperature
                .to_string()
                .parse::<f64>()
                .unwrap_or(f64::from(temperature))
        );
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

/// The ChatGPT account id claim of the access token (a JWT).
fn extract_account_id(token: &str) -> Result<String, String> {
    let parts: Vec<&str> = token.split('.').collect();
    let account_id = (parts.len() == 3)
        .then(|| URL_SAFE_NO_PAD_INDIFFERENT.decode(parts[1]).ok())
        .flatten()
        .and_then(|payload| serde_json::from_slice::<Value>(&payload).ok())
        .and_then(|payload| {
            payload
                .get(JWT_CLAIM_PATH)?
                .get("chatgpt_account_id")?
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        });
    account_id.ok_or_else(|| "Failed to extract accountId from token".to_owned())
}

/// Send the request and stream the response into `tx`, ending with exactly one terminal
/// event.
async fn run(
    http: &reqwest::Client,
    model: &Model,
    request: Result<CodexRequest, String>,
    cancel: &CancellationToken,
    tx: &UnboundedSender<AssistantEvent>,
) {
    let mut output = AssistantMessage::empty(model);
    let result = match request {
        Ok(request) => drive(http, model, request, cancel, tx, &mut output).await,
        Err(message) => Err(message),
    };
    let event = match result {
        Ok(()) => AssistantEvent::Done {
            reason: output.stop_reason,
            message: output,
        },
        Err(message) => {
            let reason = if cancel.is_cancelled() {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            tracing::debug!(%message, ?reason, "codex request failed");
            output.stop_reason = reason;
            output.error_message = Some(message);
            AssistantEvent::Error {
                reason,
                message: output,
            }
        }
    };
    // A closed receiver means nobody is listening any more.
    let _ = tx.unbounded_send(event);
}

async fn drive(
    http: &reqwest::Client,
    model: &Model,
    request: CodexRequest,
    cancel: &CancellationToken,
    tx: &UnboundedSender<AssistantEvent>,
    output: &mut AssistantMessage,
) -> Result<(), String> {
    // pi's Codex transport does not retry by default.
    let policy = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    let response = send_with_retry(
        || {
            http.post(&request.url)
                .headers(request.headers.clone())
                .body(request.body.clone())
        },
        &policy,
        cancel,
    )
    .await
    .map_err(|error| match error {
        Error::Aborted => ABORTED.to_owned(),
        error => error.to_string(),
    })?;
    if !response.status().is_success() {
        return Err(parse_error_response(response, cancel).await);
    }

    let _ = tx.unbounded_send(AssistantEvent::Start {
        partial: output.clone(),
    });
    let mut stream = ResponsesStream::new(model, output, Some(apply_service_tier_pricing));
    let mut decoder = SseDecoder::new();
    let mut body = response.bytes_stream();
    let mut events = Vec::new();
    'read: loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ABORTED.to_owned()),
            chunk = body.next() => chunk,
        };
        let (sse_events, eof) = match chunk {
            Some(Ok(bytes)) => (decoder.push(&bytes), false),
            Some(Err(error)) => return Err(error.to_string()),
            None => (
                std::mem::take(&mut decoder).finish().into_iter().collect(),
                true,
            ),
        };
        for sse in sse_events {
            let terminal = map_codex_event(&mut stream, &sse.data, &mut events);
            for event in events.drain(..) {
                let _ = tx.unbounded_send(event);
            }
            if terminal? {
                break 'read;
            }
        }
        if eof {
            break;
        }
    }
    stream.finish()?;
    if cancel.is_cancelled() {
        return Err(ABORTED.to_owned());
    }
    match stream.output.stop_reason {
        StopReason::Error | StopReason::Aborted => Err(stream
            .output
            .error_message
            .clone()
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| "An unknown error occurred".to_owned())),
        _ => Ok(()),
    }
}

/// pi `mapCodexEvents`: Codex errors fail the response, and its terminal events
/// (`response.done`, `response.completed`, `response.incomplete`) become a
/// `response.completed` with a normalized status. Returns whether the event was terminal.
fn map_codex_event(
    stream: &mut ResponsesStream<'_>,
    data: &str,
    events: &mut Vec<AssistantEvent>,
) -> Result<bool, String> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(false);
    }
    let mut event: Value =
        serde_json::from_str(data).map_err(|error| format!("Invalid Codex SSE JSON: {error}"))?;
    let Some(kind) = event.get("type").and_then(Value::as_str) else {
        return Ok(false);
    };
    match kind {
        "error" => {
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
        "response.failed" => Err(event
            .pointer("/response/error/message")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .unwrap_or("Codex response failed")
            .to_owned()),
        "response.done" | "response.completed" | "response.incomplete" => {
            if let Some(end_turn) = event.pointer("/response/end_turn").and_then(Value::as_bool) {
                stream.output.end_turn = Some(end_turn);
            }
            if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut)
                && !response
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| RESPONSE_STATUSES.contains(&status))
            {
                response.remove("status");
            }
            event["type"] = json!("response.completed");
            stream.handle(&event, events)?;
            Ok(true)
        }
        _ => stream.handle(&event, events).map(|()| false),
    }
}

/// pi `parseErrorResponse`: the provider's error message, or a friendly usage-limit
/// message.
async fn parse_error_response(response: reqwest::Response, cancel: &CancellationToken) -> String {
    let status = response.status();
    let Some(raw) = error_body(response, cancel).await else {
        return ABORTED.to_owned();
    };
    let mut message = if raw.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("Request failed")
            .to_owned()
    } else {
        raw.clone()
    };
    let mut friendly = None;
    if let Ok(parsed) = serde_json::from_str::<Value>(&raw)
        && let Some(error) = parsed.get("error").filter(|error| error.is_object())
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
            || status.as_u16() == 429
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
    friendly.unwrap_or(message)
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
