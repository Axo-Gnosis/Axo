//! Anthropic Messages adapter for API keys, header-owned auth and the built-in
//! Claude subscription provider. Subscription shaping stays on the wire.

use std::collections::HashMap;
use std::ops::ControlFlow;

use futures::stream::BoxStream;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::http::{dispatch, read_sse};
use crate::api::sse::SseEvent;
use crate::api::{
    ABORTED, Api, ProviderRequest, Reply, SimpleStreamOptions, StreamOptions, header_map, respond,
    served_tier,
};
use crate::cost::calculate_cost;
use crate::estimate::clamp_max_tokens_to_context;
use crate::event::AssistantEvent;
use crate::json::{parse_tool_arguments, parse_with_repair};
use crate::model::{ApiKind, Model, SessionAffinityFormat, tier_wire};
use crate::thinking::{MIN_ANSWER_TOKENS, default_thinking_budget};
use crate::transcript::{
    Transcript, TranscriptToolSupport, current_tools, render_system_message_update,
    system_message_text,
};
use crate::transform::replace_images;
use crate::types::{
    AssistantBlock, AssistantMessage, Message, ReasoningReplay, ServiceTier, StopReason,
    ThinkingLevel, ToolCall, ToolResultMessage, ToolSpec, UserBlock,
};

#[cfg(test)]
mod tests;
mod tool_names;

use tool_names::{PLACEHOLDER, ToolNames, wire_name};

const SUBSCRIPTION_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const OAUTH_BETA: &str = "oauth-2025-04-20";

const API_VERSION: &str = "2023-06-01";
const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const MID_CONVERSATION_OUTPUT_CONFIG_BETA: &str = "mid-conversation-output-config-2026-07-01";
const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";
const INLINE_TOOLS_BETA: &str = "inline-tools-2026-09-15";
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";

/// The only SSE event types passed on to the block mapper.
const ANTHROPIC_MESSAGE_EVENTS: [&str; 6] = [
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];
const ANTHROPIC_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Streams Anthropic Messages responses (`POST {base}/v1/messages`).
pub struct AnthropicApi {
    http: reqwest::Client,
    subscription: bool,
}

impl AnthropicApi {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            subscription: false,
        }
    }

    pub(crate) fn subscription(http: reqwest::Client) -> Self {
        Self {
            http,
            subscription: true,
        }
    }
}

impl Api for AnthropicApi {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: StreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent> {
        let http = self.http.clone();
        let subscription = self.subscription;
        let owned_model = model.clone();
        let tier = if subscription {
            ServiceTier::Standard
        } else {
            model.effective_tier(opts.simple.service_tier)
        };
        respond(model, tier, cancel, move |mut reply, cancel| async move {
            let result = run(
                &http,
                &owned_model,
                &transcript,
                &opts,
                &cancel,
                &mut reply,
                subscription,
            )
            .await;
            (reply, result)
        })
    }
}

/// pi `getAnthropicCompat`, minus the fields of the dropped branches.
struct AnthropicCompat {
    eager_tool_input_streaming: bool,
    send_session_affinity_headers: bool,
    session_affinity_format: Option<SessionAffinityFormat>,
    cache_control_on_tools: bool,
    temperature: bool,
    allow_empty_signature: bool,
    mid_convo_system_messages: bool,
}

impl AnthropicCompat {
    fn of(model: &Model) -> Self {
        let compat = &model.compat;
        let open_router =
            model.provider == "openrouter" || model.base_url.contains("openrouter.ai");
        Self {
            eager_tool_input_streaming: compat.supports_eager_tool_input_streaming.unwrap_or(true),
            send_session_affinity_headers: compat
                .send_session_affinity_headers
                .unwrap_or(open_router),
            session_affinity_format: compat
                .session_affinity_format
                .or(open_router.then_some(SessionAffinityFormat::Openrouter)),
            cache_control_on_tools: compat.supports_cache_control_on_tools.unwrap_or(true),
            temperature: compat.supports_temperature.unwrap_or(true),
            allow_empty_signature: compat.allow_empty_signature.unwrap_or(false),
            mid_convo_system_messages: compat.supports_mid_convo_system_messages.unwrap_or(false),
        }
    }
}

/// pi `AnthropicOptions` as `streamSimple` fills them.
struct RequestOptions {
    max_tokens: u64,
    temperature: Option<f64>,
    tier: ServiceTier,
    tool_choice: Option<crate::ToolChoice>,
    thinking_enabled: bool,
    thinking_budget_tokens: Option<u64>,
    effort: Option<String>,
}

/// Port of pi `streamSimple`: map the provider-neutral thinking level to adaptive effort
/// or a token budget, and fit `max_tokens` into the context window.
fn request_options(
    model: &Model,
    transcript: &Transcript,
    simple: &SimpleStreamOptions,
    tier: ServiceTier,
) -> RequestOptions {
    let base = RequestOptions {
        max_tokens: clamp_max_tokens_to_context(
            model,
            transcript,
            simple.max_tokens.unwrap_or(model.max_tokens),
        ),
        temperature: simple.temperature,
        tier,
        tool_choice: simple.tool_choice,
        thinking_enabled: false,
        thinking_budget_tokens: None,
        effort: None,
    };
    let level = simple.thinking;
    if level == ThinkingLevel::Off {
        return base;
    }

    if model.compat.force_adaptive_thinking == Some(true) {
        // pi `mapThinkingLevelToEffort`: an explicit map entry wins; unmapped (or
        // unsupported) levels fall back to the nearest Anthropic effort.
        let mapped = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&level).cloned().flatten());
        let effort = mapped.unwrap_or_else(|| {
            match level {
                ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
                ThinkingLevel::Medium => "medium",
                _ => "high",
            }
            .to_owned()
        });
        return RequestOptions {
            thinking_enabled: true,
            effort: Some(effort),
            ..base
        };
    }

    // pi `adjustMaxTokensForThinking` with the default budgets (xhigh/max clamp to high).
    let mut budget = default_thinking_budget(level);
    let adjusted = (base.max_tokens + budget).min(model.max_tokens);
    if adjusted <= budget {
        budget = budget.min(adjusted.saturating_sub(MIN_ANSWER_TOKENS));
    }
    let max_tokens = clamp_max_tokens_to_context(model, transcript, adjusted);
    RequestOptions {
        max_tokens,
        thinking_enabled: true,
        thinking_budget_tokens: Some(budget.min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS))),
        ..base
    }
}

struct PreparedRequest {
    request: ProviderRequest,
    tool_names: Option<HashMap<String, String>>,
    provider_thinking_level: Option<String>,
}

fn prepare_request(
    model: &Model,
    transcript: &Transcript,
    opts: &StreamOptions,
    tier: ServiceTier,
    subscription: bool,
) -> Result<PreparedRequest, String> {
    // Reject overrides before constructing any credential-bearing header.
    if subscription
        && (model.base_url != crate::auth::claude_subscription::BASE_URL
            || opts.auth.base_url.is_some()
            || !opts.auth.headers.is_empty()
            || !opts.simple.headers.is_empty()
            || opts.rewrite.is_some())
    {
        return Err(
            "Claude subscription does not permit destination, header or request overrides"
                .to_owned(),
        );
    }
    let api_key = opts.auth.api_key.as_deref().filter(|key| !key.is_empty());

    let compat = AnthropicCompat::of(model);
    let tier = if subscription {
        ServiceTier::Standard
    } else {
        tier
    };
    let options = request_options(model, transcript, &opts.simple, tier);
    let names = subscription
        .then(|| ToolNames::new(transcript))
        .transpose()?;
    let resolved = transcript.resolve(compat.mid_convo_system_messages);
    let mut built = build_params(model, &resolved, &compat, &options, names.as_ref());
    if subscription {
        let system = built
            .params
            .as_object_mut()
            .expect("request object")
            .entry("system")
            .or_insert_with(|| json!([]));
        system
            .as_array_mut()
            .expect("system blocks")
            .insert(0, json!({ "type": "text", "text": SUBSCRIPTION_IDENTITY }));
    }
    let mut betas = beta_features(
        model,
        &resolved,
        &compat,
        opts,
        &options,
        built.native_tool_changes,
    );
    if subscription && !betas.iter().any(|beta| beta == OAUTH_BETA) {
        betas.push(OAUTH_BETA.to_owned());
    }
    let mut headers = request_headers(
        opts,
        &compat,
        if subscription { None } else { api_key },
        &betas,
    )?;
    if subscription {
        let token = api_key
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| "Log in to anthropic-subscription".to_owned())?;
        let mut bearer = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "Invalid Claude subscription access token".to_owned())?;
        bearer.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, bearer);
    }
    let base = if subscription {
        crate::auth::claude_subscription::BASE_URL
    } else {
        opts.base_url(model)
    };
    Ok(PreparedRequest {
        request: ProviderRequest {
            url: format!("{base}/v1/messages?beta=true"),
            headers,
            body: built.params,
        },
        tool_names: names.map(|names| names.active),
        provider_thinking_level: built.provider_thinking_level,
    })
}

/// One request, from building the body to the last SSE event. `reply` keeps partial content.
async fn run(
    http: &reqwest::Client,
    model: &Model,
    transcript: &Transcript,
    opts: &StreamOptions,
    cancel: &CancellationToken,
    reply: &mut Reply,
    subscription: bool,
) -> Result<StopReason, String> {
    let prepared = prepare_request(
        model,
        transcript,
        opts,
        reply.message.service_tier,
        subscription,
    )?;
    reply.message.provider_thinking_level = prepared.provider_thinking_level;
    let response = dispatch(
        http,
        opts.rewrite.as_deref(),
        model,
        prepared.request,
        cancel,
        reply,
        error_response_message,
    )
    .await?;
    reply.start();

    let mut blocks = BlockMapper {
        tool_names: prepared.tool_names,
        ..Default::default()
    };
    read_sse(response, cancel, |sse| {
        blocks.handle(sse, model, reply)?;
        Ok(ControlFlow::Continue(()))
    })
    .await?;
    if blocks.saw_start && !blocks.saw_stop {
        return Err("Anthropic stream ended before message_stop".to_owned());
    }

    if cancel.is_cancelled() {
        return Err(ABORTED.to_owned());
    }
    blocks
        .stop
        .ok_or_else(|| "Anthropic stream ended without a stop reason".to_owned())
}

/// The Anthropic SDK's `APIError` message: `<status> <body message or JSON or text>`.
fn error_response_message(status: u16, text: &str, body: Option<&Value>) -> String {
    let detail = match body {
        Some(body) => match body.get("message") {
            Some(Value::String(message)) => message.clone(),
            Some(message) => message.to_string(),
            None => body.to_string(),
        },
        None => text.to_owned(),
    };
    if detail.is_empty() {
        format!("{status} status code (no body)")
    } else {
        format!("{status} {detail}")
    }
}

/// Maps Anthropic's block indices onto `output.content` positions while streaming.
#[derive(Default)]
struct BlockMapper {
    /// Only active subscription tools may enter the canonical event stream.
    tool_names: Option<HashMap<String, String>>,
    /// Anthropic `index` of each content block; `None` once the block stopped.
    index: Vec<Option<u64>>,
    /// Stop reason from `message_delta`; `None` is pi's `pending`.
    stop: Option<StopReason>,
    saw_start: bool,
    saw_stop: bool,
}

impl BlockMapper {
    fn handle(&mut self, sse: &SseEvent, model: &Model, reply: &mut Reply) -> Result<(), String> {
        let output = &mut reply.message;
        let name = sse.event.as_deref().unwrap_or_default();
        if name == "error" {
            if let Ok(body) = serde_json::from_str::<Value>(&sse.data) {
                super::http::record_failure(output, &body);
            }
            return Err(sse.data.clone());
        }
        if !ANTHROPIC_MESSAGE_EVENTS.contains(&name) {
            return Ok(());
        }
        let event = parse_with_repair(&sse.data).map_err(|error| {
            let raw = std::iter::once(format!("event: {name}"))
                .chain(sse.data.split('\n').map(|line| format!("data: {line}")))
                .collect::<Vec<_>>()
                .join("\\n");
            format!(
                "Could not parse Anthropic SSE event {name}: {error}; data={}; raw={raw}",
                sse.data
            )
        })?;

        match event["type"].as_str().unwrap_or_default() {
            "message_start" => {
                self.saw_start = true;
                let message = &event["message"];
                if let Some(id) = message["id"].as_str() {
                    output.response_id = Some(id.to_owned());
                }
                if let Some(response_model) = message["model"].as_str()
                    && response_model != model.id
                {
                    output.response_model = Some(response_model.to_owned());
                }
                // Captured now so an early abort still reports the input tokens.
                let usage = &message["usage"];
                if let Some(speed) = usage["speed"].as_str() {
                    output.service_tier = served_tier(speed);
                }
                let tokens = |key: &str| usage[key].as_u64().unwrap_or(0);
                output.usage.input = tokens("input_tokens");
                output.usage.output = tokens("output_tokens");
                output.usage.cache_read = tokens("cache_read_input_tokens");
                output.usage.cache_write = tokens("cache_creation_input_tokens");
                output.usage.cache_write_1h = Some(
                    usage["cache_creation"]["ephemeral_1h_input_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                );
                update_cost(model, output);
            }
            "content_block_start" => self.start_block(&event, reply)?,
            "content_block_delta" => self.delta(&event, reply),
            "content_block_stop" => self.stop_block(&event, reply),
            "message_delta" => {
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    output.raw_stop_reason = Some(reason.to_owned());
                    let (stop, error_message) =
                        map_stop_reason(reason, &event["delta"]["stop_details"])?;
                    output.stop_reason = stop;
                    self.stop = Some(stop);
                    if error_message.is_some() {
                        output.error_message = error_message;
                    }
                }
                // Only present fields overwrite: proxies omit input tokens here.
                let usage = &event["usage"];
                if let Some(speed) = usage["speed"].as_str() {
                    output.service_tier = served_tier(speed);
                }
                let usage_fields = [
                    ("input_tokens", &mut output.usage.input),
                    ("output_tokens", &mut output.usage.output),
                    ("cache_read_input_tokens", &mut output.usage.cache_read),
                    ("cache_creation_input_tokens", &mut output.usage.cache_write),
                ];
                for (key, field) in usage_fields {
                    if let Some(value) = usage[key].as_u64() {
                        *field = value;
                    }
                }
                if let Some(value) = usage["cache_creation"]["ephemeral_1h_input_tokens"].as_u64() {
                    output.usage.cache_write_1h = Some(value);
                }
                // Anthropic reports reasoning tokens as a subset of output tokens.
                if let Some(value) = usage["output_tokens_details"]["thinking_tokens"].as_u64() {
                    output.usage.reasoning = Some(value);
                }
                update_cost(model, output);
            }
            "message_stop" => self.saw_stop = true,
            _ => {}
        }
        Ok(())
    }

    fn position(&self, event: &Value) -> Option<usize> {
        let index = event["index"].as_u64()?;
        self.index
            .iter()
            .position(|tracked| *tracked == Some(index))
    }

    fn start_block(&mut self, event: &Value, reply: &mut Reply) -> Result<(), String> {
        let output = &mut reply.message;
        let block = &event["content_block"];
        let text = |key: &str| block[key].as_str().unwrap_or_default();
        // Text and thinking start empty; initial text arrives as a delta.
        let (new_block, initial) = match block["type"].as_str().unwrap_or_default() {
            "fallback" if !output.content.is_empty() => {
                return Err(
                    "Anthropic performed an unsupported mid-output model fallback".to_owned(),
                );
            }
            "text" => (
                AssistantBlock::Text {
                    text: String::new(),
                    text_signature: None,
                },
                text("text"),
            ),
            "thinking" => (
                AssistantBlock::Thinking {
                    thinking: String::new(),
                    replay: Some(text("signature"))
                        .filter(|signature| !signature.is_empty())
                        .map(|signature| ReasoningReplay::AnthropicSignature(signature.to_owned())),
                    redacted: false,
                },
                text("thinking"),
            ),
            "redacted_thinking" => (
                AssistantBlock::Thinking {
                    thinking: String::new(),
                    replay: Some(ReasoningReplay::AnthropicRedacted(text("data").to_owned())),
                    redacted: true,
                },
                "[Reasoning redacted]",
            ),
            "tool_use" => (
                AssistantBlock::ToolCall(ToolCall {
                    id: text("id").to_owned(),
                    name: match &self.tool_names {
                        Some(names) => names.get(text("name")).cloned().ok_or_else(|| {
                            "Anthropic returned an unknown or inactive subscription tool".to_owned()
                        })?,
                        None => text("name").to_owned(),
                    },
                    arguments: block["input"].as_object().cloned().unwrap_or_default(),
                    arguments_error: None,
                }),
                "",
            ),
            // `fallback` before any content, and unknown block types, are skipped.
            _ => return Ok(()),
        };
        let content_index = output.content.len();
        let start = match &new_block {
            AssistantBlock::Text { .. } => AssistantEvent::TextStart { content_index },
            AssistantBlock::Thinking { .. } => AssistantEvent::ThinkingStart { content_index },
            AssistantBlock::ToolCall(call) => AssistantEvent::ToolCallStart {
                content_index,
                id: call.id.clone(),
                name: call.name.clone(),
            },
        };
        output.content.push(new_block);
        self.index.push(event["index"].as_u64());
        reply.emit(start);
        if !initial.is_empty() {
            append_text(reply, content_index, initial);
        }
        Ok(())
    }

    fn delta(&mut self, event: &Value, reply: &mut Reply) {
        let Some(content_index) = self.position(event) else {
            return;
        };
        let delta = &event["delta"];
        let text = |key: &str| delta[key].as_str().unwrap_or_default();
        match delta["type"].as_str().unwrap_or_default() {
            "text_delta" => append_text(reply, content_index, text("text")),
            "thinking_delta" => append_text(reply, content_index, text("thinking")),
            "input_json_delta" => {
                if matches!(
                    &reply.message.content[content_index],
                    AssistantBlock::ToolCall(_)
                ) {
                    reply.push_tool_args(content_index, text("partial_json"));
                }
            }
            // The signature accumulates silently; it surfaces in `ThinkingEnd`.
            "signature_delta" => {
                if let AssistantBlock::Thinking {
                    replay,
                    redacted: false,
                    ..
                } = &mut reply.message.content[content_index]
                {
                    let part = text("signature");
                    match replay {
                        Some(ReasoningReplay::AnthropicSignature(signature)) => {
                            signature.push_str(part)
                        }
                        _ => *replay = Some(ReasoningReplay::AnthropicSignature(part.to_owned())),
                    }
                }
            }
            _ => {}
        }
    }

    fn stop_block(&mut self, event: &Value, reply: &mut Reply) {
        let Some(content_index) = self.position(event) else {
            return;
        };
        self.index[content_index] = None;
        let json = reply.take_tool_args(content_index);
        // No streamed arguments: keep the `input` the block started with.
        if !json.is_empty()
            && let AssistantBlock::ToolCall(call) = &mut reply.message.content[content_index]
        {
            (call.arguments, call.arguments_error) = parse_tool_arguments(&json);
        }
        reply.end_block(content_index);
    }
}

/// Grow a text or thinking block and emit the matching delta.
fn append_text(reply: &mut Reply, content_index: usize, delta: &str) {
    let event = match &mut reply.message.content[content_index] {
        AssistantBlock::Text { text, .. } => {
            text.push_str(delta);
            AssistantEvent::TextDelta {
                content_index,
                delta: delta.to_owned(),
            }
        }
        AssistantBlock::Thinking { thinking, .. } => {
            thinking.push_str(delta);
            AssistantEvent::ThinkingDelta {
                content_index,
                delta: delta.to_owned(),
            }
        }
        AssistantBlock::ToolCall(_) => return,
    };
    reply.emit(event);
}

/// Anthropic reports no total; it is the sum of the parts. Priced after every update.
fn update_cost(model: &Model, output: &mut AssistantMessage) {
    let usage = &mut output.usage;
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
    calculate_cost(model, output);
}

fn map_stop_reason(
    reason: &str,
    stop_details: &Value,
) -> Result<(StopReason, Option<String>), String> {
    Ok(match reason {
        // `pause_turn`: resubmitting is enough. `stop_sequence`: axo sends none.
        "end_turn" | "pause_turn" | "stop_sequence" => (StopReason::Stop, None),
        "max_tokens" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => (
            StopReason::Error,
            Some(
                stop_details["explanation"]
                    .as_str()
                    .filter(|explanation| !explanation.is_empty())
                    .unwrap_or("The model refused to complete the request")
                    .to_owned(),
            ),
        ),
        "sensitive" => (
            StopReason::Error,
            Some("Provider stopped with: sensitive".to_owned()),
        ),
        other => return Err(format!("Unhandled stop reason: {other}")),
    })
}

struct BuiltParams {
    params: Value,
    native_tool_changes: bool,
    /// Effort of the final effort marker, recorded on the reply so later requests can replay it.
    provider_thinking_level: Option<String>,
}

/// Port of pi `buildParams`, sharing the encoder between credential modes.
fn build_params(
    model: &Model,
    transcript: &Transcript,
    compat: &AnthropicCompat,
    options: &RequestOptions,
    names: Option<&ToolNames>,
) -> BuiltParams {
    let cache_control = json!({ "type": "ephemeral" });
    let initial = transcript.initial_system_message();
    let initial_text = initial.map(system_message_text).unwrap_or_default();
    let initial_tools = initial.map_or(&[][..], |system| &system.tools_added[..]);
    // Native tool changes keep the request-level tool list fixed and define every later
    // tool in a `tool_addition` block. Anthropic rejects a list where every tool is
    // deferred, so an initial active tool must anchor the placeholder.
    let native_tool_changes =
        model.tool_support() == TranscriptToolSupport::Changes && !initial_tools.is_empty();
    let thinking_disabled = model.reasoning
        && !options.thinking_enabled
        && !matches!(
            model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&ThinkingLevel::Off)),
            Some(None)
        );
    let mid_convo_effort = model.compat.supports_mid_convo_effort == Some(true);
    // Effort cannot change between messages while thinking is disabled.
    let effort_messages = mid_convo_effort && !thinking_disabled;
    let (messages, assistant_levels) = convert_messages(
        transcript.without_initial_system_message(),
        &cache_control,
        compat,
        effort_messages.then_some(model.provider.as_str()),
        native_tool_changes,
        names,
    );
    let active_effort = options.effort.clone().unwrap_or_else(|| "high".to_owned());
    let messages = if effort_messages {
        insert_thinking_level_messages(messages, &assistant_levels, &active_effort)
    } else {
        messages
    };

    let mut params = json!({
        "model": model.id,
        "messages": messages,
        "max_tokens": options.max_tokens,
        "stream": true,
    });
    if let Some(choice) = options.tool_choice {
        params["tool_choice"] = json!({ "type": choice.as_str() });
    }
    if !initial_text.is_empty() {
        params["system"] =
            json!([{ "type": "text", "text": initial_text, "cache_control": cache_control }]);
    }
    // Temperature is incompatible with extended thinking and unsupported on Opus 4.7+.
    if let Some(temperature) = options.temperature
        && !options.thinking_enabled
        && !mid_convo_effort
        && compat.temperature
    {
        params["temperature"] = json!(temperature);
    }
    if let Some(word) = tier_wire(ApiKind::AnthropicMessages, options.tier) {
        params["speed"] = json!(word);
    }

    let tool_cache_control = compat.cache_control_on_tools.then_some(&cache_control);
    if native_tool_changes {
        // Initial tools stay active with the cache breakpoint on the last one, followed by
        // a stable deferred placeholder: declaring it from the first request keeps
        // Anthropic's hidden tool-change scaffolding in the cached prefix.
        let mut tools = convert_tools(
            initial_tools,
            compat.eager_tool_input_streaming,
            tool_cache_control,
            names,
        );
        tools.push(json!({
            "name": PLACEHOLDER,
            "description": "Reserved placeholder. Never available. Never call this.",
            "input_schema": { "type": "object", "properties": {}, "required": [] },
            "defer_loading": true,
        }));
        params["tools"] = Value::Array(tools);
    } else {
        let tools = current_tools(transcript);
        if !tools.is_empty() {
            params["tools"] = Value::Array(convert_tools(
                &tools,
                compat.eager_tool_input_streaming,
                tool_cache_control,
                names,
            ));
        }
    }

    if thinking_disabled {
        params["thinking"] = json!({ "type": "disabled" });
    } else if effort_messages {
        // Managed-effort models use adaptive thinking so prefix mismatches drop the block
        // instead of failing with a 400.
        params["thinking"] = json!({
            "type": "adaptive",
            "display": "summarized",
            "block_binding": { "prefix_mismatch_behavior": "drop_block" },
        });
        params["output_config"] = json!({ "effort": "high" });
    } else if model.reasoning && options.thinking_enabled {
        if model.compat.force_adaptive_thinking == Some(true) {
            params["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
            if let Some(effort) = &options.effort {
                params["output_config"] = json!({ "effort": effort });
            }
        } else {
            params["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": options.thinking_budget_tokens.filter(|budget| *budget > 0).unwrap_or(1024),
                "display": "summarized",
            });
        }
    }

    BuiltParams {
        params,
        native_tool_changes,
        provider_thinking_level: effort_messages.then_some(active_effort),
    }
}

/// pi `getBetaFeatures`: an `anthropic-beta` header in the auth or request headers
/// replaces the computed list (`None` sends none).
fn beta_features(
    model: &Model,
    transcript: &Transcript,
    compat: &AnthropicCompat,
    opts: &StreamOptions,
    options: &RequestOptions,
    native_tool_changes: bool,
) -> Vec<String> {
    let configured = opts.configured_headers();
    let mut features: Vec<String> = Vec::new();
    let mut add = |feature: &str| {
        if !features.iter().any(|existing| existing == feature) {
            features.push(feature.to_owned());
        }
    };
    match configured.get("anthropic-beta") {
        Some(None) => {}
        Some(Some(value)) => value
            .split(',')
            .map(str::trim)
            .filter(|feature| !feature.is_empty())
            .for_each(add),
        None => {
            if !compat.eager_tool_input_streaming && !current_tools(transcript).is_empty() {
                add(FINE_GRAINED_TOOL_STREAMING_BETA);
            }
            if model.reasoning
                && options.thinking_enabled
                && model.compat.force_adaptive_thinking != Some(true)
            {
                add(INTERLEAVED_THINKING_BETA);
            }
            if model.compat.supports_mid_convo_effort == Some(true) {
                add(MID_CONVERSATION_OUTPUT_CONFIG_BETA);
                add(THINKING_BINDING_CONTROLS_BETA);
            }
            if native_tool_changes {
                add(INLINE_TOOLS_BETA);
            }
            if options.tier == ServiceTier::Fast {
                add(FAST_MODE_BETA);
            }
        }
    }
    features
}

/// Headers in the SDK's merge order: API defaults and key, then pi's defaults,
/// session affinity, auth headers and request headers (`None` deletes),
/// then the body type and the beta list.
fn request_headers(
    opts: &StreamOptions,
    compat: &AnthropicCompat,
    api_key: Option<&str>,
    betas: &[String],
) -> Result<HeaderMap, String> {
    let mut prefix = vec![("anthropic-version".to_owned(), Some(API_VERSION.to_owned()))];
    if let Some(key) = api_key {
        prefix.push(("x-api-key".to_owned(), Some(key.to_owned())));
    }
    prefix.extend([
        ("accept".to_owned(), Some("application/json".to_owned())),
        (
            "anthropic-dangerous-direct-browser-access".to_owned(),
            Some("true".to_owned()),
        ),
    ]);
    if let Some(session_id) = &opts.simple.session_id
        && compat.send_session_affinity_headers
    {
        let name = match compat.session_affinity_format {
            Some(SessionAffinityFormat::Openrouter) => "x-session-id",
            _ => "x-session-affinity",
        };
        prefix.push((name.to_owned(), Some(session_id.clone())));
    }
    let betas = betas.join(",");
    let suffix = [
        (
            "content-type".to_owned(),
            Some("application/json".to_owned()),
        ),
        (
            "anthropic-beta".to_owned(),
            (!betas.is_empty()).then_some(betas),
        ),
    ];
    header_map([prefix
        .into_iter()
        .chain(opts.configured_headers())
        .chain(suffix)])
}

/// pi `convertTools` for tools without constrained sampling.
fn convert_tools(
    tools: &[ToolSpec],
    eager: bool,
    cache_control: Option<&Value>,
    names: Option<&ToolNames>,
) -> Vec<Value> {
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let mut definition = json!({ "name": wire_name(names, &tool.name), "description": tool.description });
            if eager {
                definition["eager_input_streaming"] = Value::Bool(true);
            }
            definition["input_schema"] = json!({
                "type": "object",
                "properties": tool.parameters.get("properties").cloned().unwrap_or_else(|| json!({})),
                "required": tool.parameters.get("required").cloned().unwrap_or_else(|| json!([])),
            });
            if let Some(cache_control) = cache_control
                && index + 1 == tools.len()
            {
                definition["cache_control"] = cache_control.clone();
            }
            definition
        })
        .collect()
}

fn image_block(data: &str, mime_type: &str) -> Value {
    json!({ "type": "image", "source": { "type": "base64", "media_type": mime_type, "data": data } })
}

fn user_block(block: &UserBlock) -> Value {
    match block {
        UserBlock::Text { text } => json!({ "type": "text", "text": text }),
        UserBlock::Image { data, mime_type } => image_block(data, mime_type),
    }
}

/// Tool result content: text-only results as one joined string; with images, a block
/// list that always has text (`(see attached image)` when there is none).
fn convert_content_blocks(content: &[UserBlock]) -> Value {
    if !content
        .iter()
        .any(|block| matches!(block, UserBlock::Image { .. }))
    {
        return Value::String(
            content
                .iter()
                .filter_map(UserBlock::as_text)
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let mut blocks: Vec<Value> = Vec::with_capacity(content.len() + 1);
    if !content
        .iter()
        .any(|block| matches!(block, UserBlock::Text { .. }))
    {
        blocks.push(json!({ "type": "text", "text": "(see attached image)" }));
    }
    blocks.extend(content.iter().map(user_block));
    Value::Array(blocks)
}

/// An error tool result split into a text-only result and images sent after the results.
struct SplitErrorToolResult {
    content: Vec<UserBlock>,
    caption: String,
    images: Vec<UserBlock>,
}

fn split_error_tool_result_images(message: &ToolResultMessage) -> Option<SplitErrorToolResult> {
    if !message.is_error {
        return None;
    }
    let images: Vec<UserBlock> = message
        .content
        .iter()
        .filter(|block| matches!(block, UserBlock::Image { .. }))
        .cloned()
        .collect();
    if images.is_empty() {
        return None;
    }
    Some(SplitErrorToolResult {
        content: replace_images(&message.content, "(image attached after the tool results)"),
        caption: format!(
            "Images from the error result of tool call {} ({}):",
            message.tool_call_id, message.tool_name
        ),
        images,
    })
}

/// Port of pi `convertMessages`. Returns the wire messages and, for managed-effort
/// models, the effort each replayed assistant message was produced with (by wire index).
fn convert_messages(
    messages: &[Message],
    cache_control: &Value,
    compat: &AnthropicCompat,
    managed_provider: Option<&str>,
    native_tool_changes: bool,
    names: Option<&ToolNames>,
) -> (Vec<Value>, HashMap<usize, String>) {
    let mut params: Vec<Value> = Vec::with_capacity(messages.len());
    let mut assistant_levels = HashMap::new();
    // Later system messages are held back until the next assistant message (or the end):
    // a `tool_result` must directly follow its `tool_use`, so a system message between
    // them would be rejected.
    let mut pending_system: Vec<Value> = Vec::new();

    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            // Only reached when the model accepts system messages mid-conversation;
            // otherwise the transcript was collapsed into the leading message.
            Message::System(system) => {
                let text = render_system_message_update(system);
                let mut blocks = Vec::new();
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
                if native_tool_changes {
                    for name in &system.tools_removed {
                        // A same-name definition replaces the old one; no removal needed.
                        if system.tools_added.iter().any(|tool| &tool.name == name) {
                            continue;
                        }
                        blocks.push(json!({
                            "type": "tool_removal",
                            "tool": { "type": "tool_reference", "name": wire_name(names, name) },
                        }));
                    }
                    for definition in convert_tools(
                        &system.tools_added,
                        compat.eager_tool_input_streaming,
                        None,
                        names,
                    ) {
                        blocks.push(json!({
                            "type": "tool_addition",
                            "tool": { "type": "tool_definition", "definition": definition },
                        }));
                    }
                }
                if !blocks.is_empty() {
                    pending_system.push(json!({ "role": "system", "content": blocks }));
                }
            }
            Message::User(user) => {
                let blocks: Vec<Value> = user
                    .content
                    .iter()
                    .filter(|block| !block.as_text().is_some_and(|text| text.trim().is_empty()))
                    .map(user_block)
                    .collect();
                if !blocks.is_empty() {
                    params.push(json!({ "role": "user", "content": blocks }));
                }
            }
            Message::Assistant(assistant) => {
                params.append(&mut pending_system);
                let mut blocks = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantBlock::Text { text, .. } => {
                            if !text.trim().is_empty() {
                                blocks.push(json!({ "type": "text", "text": text }));
                            }
                        }
                        AssistantBlock::Thinking {
                            replay,
                            redacted: true,
                            ..
                        } => {
                            if let Some(
                                ReasoningReplay::AnthropicRedacted(data)
                                | ReasoningReplay::AnthropicSignature(data),
                            ) = replay
                            {
                                blocks.push(json!({ "type": "redacted_thinking", "data": data }));
                            }
                        }
                        AssistantBlock::Thinking {
                            thinking, replay, ..
                        } => {
                            let signature = match replay {
                                Some(ReasoningReplay::AnthropicSignature(signature))
                                    if !signature.trim().is_empty() =>
                                {
                                    Some(signature)
                                }
                                _ => None,
                            };
                            match signature {
                                Some(signature) => blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": thinking,
                                    "signature": signature,
                                })),
                                None if thinking.trim().is_empty() => {}
                                // No signature (e.g. an aborted stream): replay as text,
                                // unless the provider accepts empty signatures.
                                None if compat.allow_empty_signature => blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": thinking,
                                    "signature": "",
                                })),
                                None => blocks.push(json!({ "type": "text", "text": thinking })),
                            }
                        }
                        AssistantBlock::ToolCall(call) => blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": wire_name(names, &call.name),
                            "input": call.arguments,
                        })),
                    }
                }
                if !blocks.is_empty() {
                    if let Some(provider) = managed_provider
                        && assistant.api == ApiKind::AnthropicMessages
                        && assistant.provider == provider
                        && let Some(level) = &assistant.provider_thinking_level
                        && ANTHROPIC_EFFORTS.contains(&level.as_str())
                    {
                        assistant_levels.insert(params.len(), level.clone());
                    }
                    params.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            Message::ToolResult(_) => {
                // Consecutive results merge into one user message. Anthropic rejects
                // non-text error results, so their images follow all results of the turn.
                let mut results = Vec::new();
                let mut error_images = Vec::new();
                while let Some(Message::ToolResult(result)) = messages.get(i) {
                    let split = split_error_tool_result_images(result);
                    let content = split
                        .as_ref()
                        .map_or(&result.content, |split| &split.content);
                    results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": result.tool_call_id,
                        "content": convert_content_blocks(content),
                        "is_error": result.is_error,
                    }));
                    if let Some(split) = split {
                        error_images.push(json!({ "type": "text", "text": split.caption }));
                        error_images.extend(split.images.iter().map(user_block));
                    }
                    i += 1;
                }
                results.append(&mut error_images);
                params.push(json!({ "role": "user", "content": results }));
                continue;
            }
        }
        i += 1;
    }
    params.append(&mut pending_system);

    // Cache the conversation history up to the last user or system message.
    if let Some(last) = params.last_mut()
        && matches!(last["role"].as_str(), Some("user" | "system"))
        && let Some(block) = last["content"]
            .as_array_mut()
            .and_then(|blocks| blocks.last_mut())
        && matches!(
            block["type"].as_str(),
            Some("text" | "image" | "tool_result" | "tool_addition" | "tool_removal")
        )
    {
        block["cache_control"] = cache_control.clone();
    }

    (params, assistant_levels)
}

/// Put an effort marker before each replayed assistant message produced at a known
/// effort, and the active effort at the end.
fn insert_thinking_level_messages(
    messages: Vec<Value>,
    assistant_levels: &HashMap<usize, String>,
    active_effort: &str,
) -> Vec<Value> {
    let marker = |effort: &str| json!({ "role": "system", "content": [], "output_config": { "effort": effort } });
    let mut result = Vec::with_capacity(messages.len() + assistant_levels.len() + 1);
    for (index, message) in messages.into_iter().enumerate() {
        if let Some(effort) = assistant_levels.get(&index) {
            result.push(marker(effort));
        }
        result.push(message);
    }
    result.push(marker(active_effort));
    result
}
