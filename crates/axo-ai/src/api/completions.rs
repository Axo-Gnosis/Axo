//! OpenAI Chat Completions adapter (`openai-completions`). Port of pi
//! `api/openai-completions.ts` for OpenAI-compatible servers (llama.cpp, vLLM,
//! OpenRouter, DeepSeek, …).
//!
//! Differences from pi:
//! - with no API key the request carries no `Authorization` header (pi throws), so
//!   keyless local servers work;
//! - not ported: the GitHub Copilot and opencode-go branches, grammar (custom) tools,
//!   per-tool strict JSON-schema sampling (axo tools never request it), sampling
//!   presets, long cache retention and the `PI_CACHE_RETENTION` switch (retention is
//!   always pi's default `short`), and the `onPayload`/`onResponse` hooks.

use std::collections::HashMap;

use futures::channel::mpsc::{self, UnboundedSender};
use futures::stream::{self, BoxStream};
use futures::{StreamExt, future};
use indexmap::IndexMap;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::http::{RetryPolicy, error_body, send_with_retry};
use crate::api::sse::{SseDecoder, SseEvent};
use crate::api::{ABORTED, Api, SimpleStreamOptions, StreamOptions};
use crate::cost::calculate_cost;
use crate::error::Error;
use crate::estimate::{CONTEXT_SAFETY_TOKENS, estimate_context_tokens};
use crate::event::AssistantEvent;
use crate::json::{parse_streaming, parse_tool_arguments};
use crate::model::{
    CacheControlFormat, ChatTemplateValue, InputKind, MaxTokensField, Model, SessionAffinityFormat,
    ThinkingFormat, ThinkingTokenBudgetField, ThinkingVar,
};
use crate::thinking::{clamp_level, provider_value};
use crate::transcript::{
    Transcript, TranscriptToolSupport, render_system_message_update, resolve_transcript_tools,
    system_message_text,
};
use crate::types::{
    AssistantBlock, AssistantMessage, JsonMap, Message, ReasoningReplay, StopReason, ThinkingLevel,
    ToolCall, ToolSpec, Usage, UserBlock,
};

/// Delta fields that carry reasoning text, in the order they are read: llama.cpp uses
/// `reasoning_content`, other servers `reasoning`; some send both with the same text, so
/// the first non-empty one wins.
const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

const USER_AGENT: &str = concat!("axo-ai/", env!("CARGO_PKG_VERSION"));

/// Provider error bodies longer than this are truncated in error messages.
const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4000;

/// Tokens always left for the answer when a thinking budget shares the response ceiling.
const MIN_ANSWER_TOKENS: u64 = 1024;

/// The OpenAI Chat Completions API.
#[derive(Debug, Clone)]
pub struct CompletionsApi {
    http: reqwest::Client,
}

impl CompletionsApi {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

impl Api for CompletionsApi {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: StreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent> {
        let (tx, rx) = mpsc::unbounded();
        let response = Response::new(model.clone(), tx);
        let http = self.http.clone();
        // The request is driven by the returned stream itself: dropping the stream drops
        // the request.
        let producer =
            stream::once(async move { response.run(&http, &transcript, &opts, &cancel).await })
                .filter_map(|()| future::ready(None));
        stream::select(rx, producer).boxed()
    }
}

/// Compat flags after auto-detection from provider id and base URL, overridden field by
/// field by the explicit `model.compat` (pi `detectCompat` + `getCompat`).
struct ResolvedCompat {
    supports_store: bool,
    supports_developer_role: bool,
    supports_reasoning_effort: bool,
    supports_usage_in_streaming: bool,
    supports_finish_reason: bool,
    max_tokens_field: MaxTokensField,
    requires_tool_result_name: bool,
    requires_assistant_after_tool_result: bool,
    requires_thinking_as_text: bool,
    requires_reasoning_content_on_assistant_messages: bool,
    thinking_format: ThinkingFormat,
    zai_tool_stream: bool,
    supports_thinking_token_budget: bool,
    thinking_token_budget_field: Option<ThinkingTokenBudgetField>,
    supports_strict_mode: bool,
    supports_mid_convo_system_messages: bool,
    supports_mid_convo_tool_additions: bool,
    cache_control_format: Option<CacheControlFormat>,
    send_session_affinity_headers: bool,
    session_affinity_format: SessionAffinityFormat,
}

fn resolve_compat(model: &Model, base_url: &str) -> ResolvedCompat {
    let provider = model.provider.as_str();
    let is_zai = provider == "zai"
        || provider == "zai-coding-cn"
        || base_url.contains("api.z.ai")
        || base_url.contains("open.bigmodel.cn");
    let is_together = provider == "together"
        || base_url.contains("api.together.ai")
        || base_url.contains("api.together.xyz");
    let is_moonshot = provider == "moonshotai"
        || provider == "moonshotai-cn"
        || base_url.contains("api.moonshot.");
    let is_open_router = provider == "openrouter" || base_url.contains("openrouter.ai");
    let is_cloudflare_workers_ai =
        provider == "cloudflare-workers-ai" || base_url.contains("api.cloudflare.com");
    let is_cloudflare_ai_gateway =
        provider == "cloudflare-ai-gateway" || base_url.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");
    let is_cerebras = provider == "cerebras" || base_url.contains("cerebras.ai");
    let is_deepseek = provider == "deepseek" || base_url.to_lowercase().contains("deepseek.com");
    let is_grok = provider == "xai" || base_url.contains("api.x.ai");
    let is_chutes = base_url.contains("chutes.ai");

    let non_standard = is_nvidia
        || is_cerebras
        || is_grok
        || is_together
        || is_chutes
        || is_deepseek
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base_url.contains("opencode.ai")
        || is_cloudflare_workers_ai
        || is_cloudflare_ai_gateway
        || is_ant_ling;
    let use_max_tokens = is_chutes
        || is_deepseek
        || is_moonshot
        || is_cloudflare_ai_gateway
        || is_together
        || is_nvidia
        || is_ant_ling
        || is_zai;
    let open_router_developer_role =
        is_open_router && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
    let detected_thinking_format = if is_deepseek {
        ThinkingFormat::Deepseek
    } else if is_zai {
        ThinkingFormat::Zai
    } else if is_together {
        ThinkingFormat::Together
    } else if is_ant_ling {
        ThinkingFormat::AntLing
    } else if is_open_router {
        ThinkingFormat::Openrouter
    } else {
        ThinkingFormat::Openai
    };

    let compat = &model.compat;
    ResolvedCompat {
        supports_store: compat.supports_store.unwrap_or(!non_standard),
        supports_developer_role: compat
            .supports_developer_role
            .unwrap_or(open_router_developer_role || (!non_standard && !is_open_router)),
        supports_reasoning_effort: compat.supports_reasoning_effort.unwrap_or(
            !is_grok
                && !is_zai
                && !is_moonshot
                && !is_together
                && !is_cloudflare_ai_gateway
                && !is_nvidia
                && !is_ant_ling,
        ),
        supports_usage_in_streaming: compat.supports_usage_in_streaming.unwrap_or(true),
        supports_finish_reason: compat.supports_finish_reason.unwrap_or(true),
        max_tokens_field: compat.max_tokens_field.unwrap_or(if use_max_tokens {
            MaxTokensField::MaxTokens
        } else {
            MaxTokensField::MaxCompletionTokens
        }),
        requires_tool_result_name: compat.requires_tool_result_name.unwrap_or(false),
        requires_assistant_after_tool_result: compat
            .requires_assistant_after_tool_result
            .unwrap_or(false),
        requires_thinking_as_text: compat.requires_thinking_as_text.unwrap_or(false),
        requires_reasoning_content_on_assistant_messages: compat
            .requires_reasoning_content_on_assistant_messages
            .unwrap_or(is_deepseek),
        thinking_format: compat.thinking_format.unwrap_or(detected_thinking_format),
        zai_tool_stream: compat.zai_tool_stream.unwrap_or(false),
        supports_thinking_token_budget: compat.supports_thinking_token_budget.unwrap_or(false),
        thinking_token_budget_field: compat.thinking_token_budget_field,
        // OpenAI compatibility alone does not imply strict JSON-schema tool support.
        supports_strict_mode: compat.supports_strict_mode.unwrap_or(false),
        supports_mid_convo_system_messages: compat
            .supports_mid_convo_system_messages
            .unwrap_or(false),
        supports_mid_convo_tool_additions: compat
            .supports_mid_convo_tool_additions
            .unwrap_or(false),
        cache_control_format: compat.cache_control_format.or((provider == "openrouter"
            && model.id.starts_with("anthropic/"))
        .then_some(CacheControlFormat::Anthropic)),
        send_session_affinity_headers: compat
            .send_session_affinity_headers
            .unwrap_or(is_open_router),
        session_affinity_format: compat.session_affinity_format.unwrap_or(if is_open_router {
            SessionAffinityFormat::Openrouter
        } else {
            SessionAffinityFormat::Openai
        }),
    }
}

/// Request headers, later sources winning: identity → `model.headers` → session
/// affinity → resolved auth headers → request headers (`None` deletes). The bearer key
/// goes first so a configured `authorization` header replaces it.
fn build_headers(
    model: &Model,
    compat: &ResolvedCompat,
    opts: &StreamOptions,
) -> Result<HeaderMap, String> {
    let insert = |headers: &mut HeaderMap, name: &str, value: &str| -> Result<(), String> {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("Invalid header name: {name}"))?;
        let value =
            HeaderValue::from_str(value).map_err(|_| format!("Invalid value for header {name}"))?;
        headers.insert(name, value);
        Ok(())
    };
    let mut headers = HeaderMap::new();
    let api_key = opts
        .simple
        .api_key
        .as_deref()
        .or(opts.auth.api_key.as_deref())
        .filter(|key| !key.is_empty());
    if let Some(key) = api_key {
        insert(
            &mut headers,
            AUTHORIZATION.as_str(),
            &format!("Bearer {key}"),
        )?;
    }
    insert(&mut headers, "user-agent", USER_AGENT)?;
    for (name, value) in &model.headers {
        insert(&mut headers, name, value)?;
    }
    if let Some(session_id) = &opts.simple.session_id
        && compat.send_session_affinity_headers
    {
        if compat.session_affinity_format == SessionAffinityFormat::Openrouter {
            insert(&mut headers, "x-session-id", session_id)?;
        } else {
            if compat.session_affinity_format == SessionAffinityFormat::Openai {
                insert(&mut headers, "session_id", session_id)?;
            }
            insert(&mut headers, "x-client-request-id", session_id)?;
            insert(&mut headers, "x-session-affinity", session_id)?;
        }
    }
    for (name, value) in &opts.auth.headers {
        insert(&mut headers, name, value)?;
    }
    for (name, value) in &opts.simple.headers {
        match value {
            Some(value) => insert(&mut headers, name, value)?,
            None => {
                headers.remove(name.as_str());
            }
        }
    }
    Ok(headers)
}

/// The request body (pi `streamSimple` + `buildParams`).
fn build_params(
    model: &Model,
    transcript: &Transcript,
    simple: &SimpleStreamOptions,
    compat: &ResolvedCompat,
    base_url: &str,
) -> Value {
    let effort = Some(clamp_level(model, simple.thinking)).filter(|l| *l != ThinkingLevel::Off);
    let resolved = transcript.resolve(compat.supports_mid_convo_system_messages);
    let messages = resolved.messages();
    let support =
        if compat.supports_mid_convo_system_messages && compat.supports_mid_convo_tool_additions {
            TranscriptToolSupport::Additions
        } else {
            TranscriptToolSupport::None
        };
    let transcript_tools = resolve_transcript_tools(messages, support);
    let mut converted = convert_messages(
        model,
        messages,
        compat,
        transcript_tools.folded_system_messages,
    );
    let has_tool_history = messages.iter().any(|message| match message {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant.tool_calls().next().is_some(),
        _ => false,
    });
    let mut tools = if !transcript_tools.request_tools.is_empty() {
        Some(convert_tools(&transcript_tools.request_tools, compat))
    } else if has_tool_history {
        // Anthropic behind LiteLLM-style proxies requires `tools` when the history has
        // tool calls or results.
        Some(Vec::new())
    } else {
        None
    };
    if compat.cache_control_format == Some(CacheControlFormat::Anthropic) {
        apply_cache_control(
            &mut converted,
            tools.as_mut(),
            &json!({ "type": "ephemeral" }),
        );
    }

    // pi `clampMaxTokensToContext`: leave room for the estimated prompt.
    let requested_max_tokens = simple.max_tokens.unwrap_or(model.max_tokens);
    let max_tokens = if model.context_window == 0 {
        requested_max_tokens.max(1)
    } else {
        let available = model.context_window as i64
            - estimate_context_tokens(transcript.messages()) as i64
            - CONTEXT_SAFETY_TOKENS;
        requested_max_tokens.min(available.max(1) as u64)
    };

    let mut params = Map::new();
    params.insert("model".into(), model.id.clone().into());
    params.insert("messages".into(), Value::Array(converted));
    params.insert("stream".into(), true.into());
    if let Some(choice) = simple.tool_choice {
        params.insert("tool_choice".into(), choice.as_str().into());
    }
    if base_url.contains("api.openai.com")
        && let Some(session_id) = &simple.session_id
    {
        params.insert(
            "prompt_cache_key".into(),
            session_id.chars().take(64).collect::<String>().into(),
        );
    }
    if compat.supports_usage_in_streaming {
        params.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if compat.supports_store {
        params.insert("store".into(), false.into());
    }
    let max_tokens_field = match compat.max_tokens_field {
        MaxTokensField::MaxTokens => "max_tokens",
        MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
    };
    params.insert(max_tokens_field.into(), max_tokens.into());
    if let Some(temperature) = simple.temperature {
        // Through the decimal text so 0.7f32 is sent as 0.7, not 0.699999988.
        let temperature = temperature
            .to_string()
            .parse::<f64>()
            .unwrap_or(f64::from(temperature));
        params.insert("temperature".into(), temperature.into());
    }
    if let Some(tools) = tools {
        let has_tools = !tools.is_empty();
        params.insert("tools".into(), Value::Array(tools));
        if has_tools && compat.zai_tool_stream {
            params.insert("tool_stream".into(), true.into());
        }
    }
    if let Some(priority) = model.compat.vllm_priority {
        params.insert("priority".into(), priority.into());
    }

    let budget_field = compat.thinking_token_budget_field.or(compat
        .supports_thinking_token_budget
        .then_some(ThinkingTokenBudgetField::ThinkingTokenBudget));
    let thinking_budget = effort
        .filter(|_| model.reasoning)
        .map(|level| {
            let budget: u64 = match level {
                ThinkingLevel::Minimal => 1024,
                ThinkingLevel::Low => 2048,
                ThinkingLevel::Medium => 8192,
                _ => 16384,
            };
            budget.min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS))
        })
        .filter(|budget| *budget > 0);

    // `None`: no map entry (provider default); `Some(None)`: unsupported (`null`).
    let off = model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get(&ThinkingLevel::Off))
        .cloned();
    let off_supported = !matches!(off, Some(None));
    // pi `thinkingLevelMap[level] ?? level`.
    let effort_value = |level: ThinkingLevel| {
        provider_value(model, level).unwrap_or_else(|| level.as_str().to_owned())
    };
    // The mapped value of the requested level, or of `off` when thinking is off.
    let requested_value = match effort {
        Some(level) => provider_value(model, level),
        None => off.clone().flatten(),
    };
    let reasoning = model.reasoning;
    match compat.thinking_format {
        ThinkingFormat::Zai if reasoning => {
            params.insert(
                "thinking".into(),
                match effort {
                    Some(_) => json!({ "type": "enabled", "clear_thinking": false }),
                    None => json!({ "type": "disabled" }),
                },
            );
            if let Some(level) = effort
                && compat.supports_reasoning_effort
                && let Some(value) = provider_value(model, level)
            {
                params.insert("reasoning_effort".into(), value.into());
            }
        }
        ThinkingFormat::Qwen if reasoning => {
            params.insert("enable_thinking".into(), effort.is_some().into());
            if let Some(level) = effort
                && compat.supports_reasoning_effort
            {
                params.insert("reasoning_effort".into(), effort_value(level).into());
            }
        }
        ThinkingFormat::QwenChatTemplate if reasoning => {
            params.insert(
                "chat_template_kwargs".into(),
                json!({ "enable_thinking": effort.is_some(), "preserve_thinking": true }),
            );
        }
        ThinkingFormat::ChatTemplate if reasoning => {
            if let Some(values) = chat_template_values(
                model.compat.chat_template_kwargs.as_ref(),
                effort,
                requested_value.as_deref(),
                thinking_budget,
            ) {
                params.insert("chat_template_kwargs".into(), values.into());
            }
        }
        ThinkingFormat::Baseten if reasoning => {
            if let Some(values) = chat_template_values(
                model.compat.chat_template_args.as_ref(),
                effort,
                requested_value.as_deref(),
                thinking_budget,
            ) {
                params.insert("chat_template_args".into(), values.into());
            }
            if compat.supports_reasoning_effort
                && let Some(value) = requested_value
            {
                params.insert("reasoning_effort".into(), value.into());
            }
        }
        ThinkingFormat::Deepseek if reasoning => {
            if effort.is_some() {
                params.insert("thinking".into(), json!({ "type": "enabled" }));
            } else if off_supported {
                params.insert("thinking".into(), json!({ "type": "disabled" }));
            }
            if let Some(level) = effort
                && compat.supports_reasoning_effort
            {
                params.insert("reasoning_effort".into(), effort_value(level).into());
            }
        }
        ThinkingFormat::Openrouter if reasoning => match effort {
            Some(level) => {
                params.insert("reasoning".into(), json!({ "effort": effort_value(level) }));
            }
            None if off_supported => {
                let value = off.flatten().unwrap_or_else(|| "none".to_owned());
                params.insert("reasoning".into(), json!({ "effort": value }));
            }
            None => {}
        },
        ThinkingFormat::AntLing if reasoning && effort.is_some() => {
            let mapped = effort.and_then(|level| {
                model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get(&level))
                    .cloned()
                    .flatten()
            });
            if let Some(value) = mapped {
                params.insert("reasoning".into(), json!({ "effort": value }));
            }
        }
        ThinkingFormat::Together if reasoning => {
            params.insert("reasoning".into(), json!({ "enabled": effort.is_some() }));
            if let Some(level) = effort
                && compat.supports_reasoning_effort
            {
                params.insert("reasoning_effort".into(), effort_value(level).into());
            }
        }
        ThinkingFormat::StringThinking if reasoning => match effort {
            Some(level) => {
                params.insert("thinking".into(), effort_value(level).into());
            }
            None if off_supported => {
                let value = off.flatten().unwrap_or_else(|| "none".to_owned());
                params.insert("thinking".into(), value.into());
            }
            None => {}
        },
        // OpenAI-style `reasoning_effort`, also the fallback of every other format.
        _ if reasoning && compat.supports_reasoning_effort => match effort {
            Some(level) => {
                params.insert("reasoning_effort".into(), effort_value(level).into());
            }
            None => {
                if let Some(Some(value)) = off {
                    params.insert("reasoning_effort".into(), value.into());
                }
            }
        },
        _ => {}
    }

    // Reasoning and answer share max_tokens, so an uncapped reasoning phase could
    // consume the whole response; the budget field caps it whatever the format.
    if let Some(field) = budget_field
        && let Some(budget) = thinking_budget
    {
        let field = match field {
            ThinkingTokenBudgetField::ThinkingTokenBudget => "thinking_token_budget",
            ThinkingTokenBudgetField::ThinkingBudget => "thinking_budget",
            ThinkingTokenBudgetField::ThinkingBudgetTokens => "thinking_budget_tokens",
        };
        params.insert(field.into(), budget.into());
    }
    if let Some(routing) = &model.compat.open_router_routing
        && truthy(Some(routing))
    {
        params.insert("provider".into(), routing.clone());
    }
    if let Some(routing) = &model.compat.vercel_gateway_routing {
        let mut gateway = Map::new();
        for key in ["only", "order"] {
            if let Some(value) = routing.get(key).filter(|value| truthy(Some(value))) {
                gateway.insert(key.into(), value.clone());
            }
        }
        if !gateway.is_empty() {
            params.insert("providerOptions".into(), json!({ "gateway": gateway }));
        }
    }
    Value::Object(params)
}

/// Resolve `chat_template_kwargs`/`chat_template_args` values; `None` when nothing is
/// left. `requested_value` is the mapped value of the requested level (or of `off`).
fn chat_template_values(
    values: Option<&IndexMap<String, ChatTemplateValue>>,
    effort: Option<ThinkingLevel>,
    requested_value: Option<&str>,
    thinking_budget: Option<u64>,
) -> Option<Map<String, Value>> {
    let mut resolved = Map::new();
    for (key, value) in values.into_iter().flatten() {
        let value = match value {
            ChatTemplateValue::Literal(value) => Some(value.clone()),
            ChatTemplateValue::Var { omit_when_off, .. }
                if effort.is_none() && *omit_when_off == Some(true) =>
            {
                None
            }
            ChatTemplateValue::Var { var, .. } => match var {
                ThinkingVar::Enabled => Some(effort.is_some().into()),
                ThinkingVar::Budget => thinking_budget.map(Value::from),
                ThinkingVar::Effort => requested_value.map(Value::from),
            },
        };
        if let Some(value) = value {
            resolved.insert(key.clone(), value);
        }
    }
    (!resolved.is_empty()).then_some(resolved)
}

/// Function tools; `strict: false` is sent only when the server accepts the field.
fn convert_tools(tools: &[ToolSpec], compat: &ResolvedCompat) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut function = Map::new();
            function.insert("name".into(), tool.name.clone().into());
            function.insert("description".into(), tool.description.clone().into());
            function.insert("parameters".into(), tool.parameters.clone());
            if compat.supports_strict_mode {
                function.insert("strict".into(), false.into());
            }
            json!({ "type": "function", "function": function })
        })
        .collect()
}

/// Convert the resolved transcript to Chat Completions messages (pi `convertMessages`).
/// `folded_system_messages` leading system messages already have their tools in the
/// request's `tools`; later ones declare added tools in place.
fn convert_messages(
    model: &Model,
    messages: &[Message],
    compat: &ResolvedCompat,
    folded_system_messages: usize,
) -> Vec<Value> {
    let instruction_role = if model.reasoning && compat.supports_developer_role {
        "developer"
    } else {
        "system"
    };
    let bridge = json!({ "role": "assistant", "content": "I have processed the tool results." });
    let mut params = Vec::new();
    let mut last_was_tool_result = false;
    let mut system_index = 0;
    let mut i = 0;
    while i < messages.len() {
        let message = &messages[i];
        // Some providers reject a user message right after tool results.
        if compat.requires_assistant_after_tool_result
            && last_was_tool_result
            && matches!(message, Message::User(_))
        {
            params.push(bridge.clone());
        }
        match message {
            Message::System(system) => {
                if system_index >= folded_system_messages && !system.tools_added.is_empty() {
                    params.push(json!({
                        "role": "system",
                        "tools": convert_tools(&system.tools_added, compat),
                    }));
                }
                system_index += 1;
                let text = if i == 0 {
                    system_message_text(system)
                } else {
                    render_system_message_update(system)
                };
                if !text.is_empty() {
                    params.push(json!({ "role": instruction_role, "content": text }));
                }
            }
            Message::User(user) => {
                let content: Vec<Value> = user
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::Text { text } if text.is_empty() => None,
                        UserBlock::Text { text } => Some(json!({ "type": "text", "text": text })),
                        UserBlock::Image { data, mime_type } => Some(json!({
                            "type": "image_url",
                            "image_url": { "url": format!("data:{mime_type};base64,{data}") },
                        })),
                    })
                    .collect();
                if content.is_empty() {
                    i += 1;
                    continue;
                }
                params.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(assistant) => {
                let Some(converted) = convert_assistant(model, assistant, compat) else {
                    i += 1;
                    continue;
                };
                params.push(converted);
            }
            Message::ToolResult(_) => {
                let mut images = Vec::new();
                while let Some(Message::ToolResult(result)) = messages.get(i) {
                    let text = result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            UserBlock::Text { text } => Some(text.as_str()),
                            UserBlock::Image { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let has_images = result
                        .content
                        .iter()
                        .any(|block| matches!(block, UserBlock::Image { .. }));
                    let text = if !text.trim().is_empty() {
                        text
                    } else if has_images {
                        "(see attached image)".to_owned()
                    } else {
                        "(no tool output)".to_owned()
                    };
                    let mut tool_message = json!({
                        "role": "tool",
                        "content": text,
                        "tool_call_id": result.tool_call_id,
                    });
                    if compat.requires_tool_result_name && !result.tool_name.is_empty() {
                        tool_message["name"] = result.tool_name.clone().into();
                    }
                    params.push(tool_message);
                    if has_images && model.input.contains(&InputKind::Image) {
                        images.extend(result.content.iter().filter_map(|block| match block {
                            UserBlock::Image { data, mime_type } => Some(json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{mime_type};base64,{data}") },
                            })),
                            UserBlock::Text { .. } => None,
                        }));
                    }
                    i += 1;
                }
                // Tool messages cannot hold images: they follow in a user message.
                if images.is_empty() {
                    last_was_tool_result = true;
                } else {
                    if compat.requires_assistant_after_tool_result {
                        params.push(bridge.clone());
                    }
                    let mut content = vec![json!({
                        "type": "text",
                        "text": "Attached image(s) from tool result:",
                    })];
                    content.extend(images);
                    params.push(json!({ "role": "user", "content": content }));
                    last_was_tool_result = false;
                }
                continue;
            }
        }
        last_was_tool_result = false;
        i += 1;
    }
    params
}

/// One assistant message, or `None` when it has neither content nor tool calls (some
/// providers reject empty assistant messages, e.g. from an aborted response).
fn convert_assistant(
    model: &Model,
    assistant: &AssistantMessage,
    compat: &ResolvedCompat,
) -> Option<Value> {
    let mut message = Map::new();
    message.insert("role".into(), "assistant".into());
    // Some providers reject null content.
    message.insert(
        "content".into(),
        if compat.requires_assistant_after_tool_result {
            "".into()
        } else {
            Value::Null
        },
    );
    let texts: Vec<&str> = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantBlock::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let assistant_text = texts.concat();
    let thinking: Vec<(&str, Option<&ReasoningReplay>)> = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantBlock::Thinking {
                thinking, replay, ..
            } => Some((thinking.as_str(), replay.as_ref())),
            _ => None,
        })
        .collect();
    let preserved_details = thinking.iter().find_map(|(_, replay)| match replay {
        Some(ReasoningReplay::ReasoningDetails(Value::Array(details)))
            if !details.is_empty() && details.iter().all(is_reasoning_detail) =>
        {
            Some(Value::Array(details.clone()))
        }
        _ => None,
    });
    let non_empty_thinking: Vec<&(&str, Option<&ReasoningReplay>)> = thinking
        .iter()
        .filter(|(text, _)| !text.trim().is_empty())
        .collect();

    if let Some((_, first_replay)) = non_empty_thinking.first() {
        if compat.requires_thinking_as_text {
            // Plain text without tags, so the model does not learn to mimic them.
            let thinking_text = non_empty_thinking
                .iter()
                .map(|(text, _)| *text)
                .collect::<Vec<_>>()
                .join("\n\n");
            let mut parts = vec![json!({ "type": "text", "text": thinking_text })];
            parts.extend(
                texts
                    .iter()
                    .map(|text| json!({ "type": "text", "text": text })),
            );
            message.insert("content".into(), Value::Array(parts));
        } else {
            // Content is always a plain string: some models mirror content parts literally.
            if !assistant_text.is_empty() {
                message.insert("content".into(), assistant_text.into());
            }
            // Replay reasoning under the field the server streamed it in, unless the
            // structured `reasoning_details` carry it.
            if preserved_details.is_none()
                && let Some(ReasoningReplay::ReasoningField(field)) = first_replay
                && REASONING_FIELDS.contains(&field.as_str())
            {
                let reasoning = non_empty_thinking
                    .iter()
                    .map(|(text, _)| *text)
                    .collect::<Vec<_>>()
                    .join("\n");
                message.insert(field.clone(), reasoning.into());
            }
        }
    } else if !assistant_text.is_empty() {
        message.insert("content".into(), assistant_text.into());
    }

    let tool_calls: Vec<Value> = assistant
        .tool_calls()
        .map(|call| {
            json!({
                "id": call.id,
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": Value::Object(call.arguments.clone()).to_string(),
                },
            })
        })
        .collect();
    let has_tool_calls = !tool_calls.is_empty();
    if has_tool_calls {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    if let Some(details) = preserved_details {
        message.insert("reasoning_details".into(), details);
    }
    if compat.requires_reasoning_content_on_assistant_messages
        && model.reasoning
        && !message.contains_key("reasoning_content")
    {
        message.insert("reasoning_content".into(), "".into());
    }
    let has_content = match message.get("content") {
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(parts)) => !parts.is_empty(),
        _ => false,
    };
    (has_content || has_tool_calls).then_some(Value::Object(message))
}

/// Anthropic-style cache markers for OpenAI-compatible proxies: on the first
/// system/developer message, the last tool, and the last text part of the last
/// user/assistant/tool message.
fn apply_cache_control(messages: &mut [Value], tools: Option<&mut Vec<Value>>, marker: &Value) {
    let add = |message: &mut Value| -> bool {
        match message.get_mut("content") {
            Some(Value::String(text)) if !text.is_empty() => {
                let text = std::mem::take(text);
                message["content"] =
                    json!([{ "type": "text", "text": text, "cache_control": marker }]);
                true
            }
            Some(Value::Array(parts)) => match parts
                .iter_mut()
                .rev()
                .find(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            {
                Some(part) => {
                    part["cache_control"] = marker.clone();
                    true
                }
                None => false,
            },
            _ => false,
        }
    };
    let role = |message: &Value| {
        message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    if let Some(system) = messages
        .iter_mut()
        .find(|message| matches!(role(message).as_str(), "system" | "developer"))
    {
        add(system);
    }
    if let Some(last) = tools.and_then(|tools| tools.last_mut()) {
        last["cache_control"] = marker.clone();
    }
    for message in messages.iter_mut().rev() {
        if matches!(role(message).as_str(), "user" | "assistant" | "tool") && add(message) {
            return;
        }
    }
}

/// JavaScript truthiness of an optional JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(_) => true,
    }
}

/// An OpenRouter `reasoning_details` entry pi knows how to replay.
fn is_reasoning_detail(detail: &Value) -> bool {
    let Some(fields) = detail.as_object() else {
        return false;
    };
    let common = fields
        .get("id")
        .is_none_or(|id| id.is_null() || id.is_string())
        && fields.get("format").is_none_or(Value::is_string)
        && fields.get("index").is_none_or(Value::is_number);
    common
        && match fields.get("type").and_then(Value::as_str) {
            Some("reasoning.summary") => fields.get("summary").is_some_and(Value::is_string),
            Some("reasoning.encrypted") => fields.get("data").is_some_and(Value::is_string),
            Some("reasoning.text") => {
                fields.get("text").is_some_and(Value::is_string)
                    && fields
                        .get("signature")
                        .is_none_or(|signature| signature.is_null() || signature.is_string())
            }
            _ => false,
        }
}

/// Append a streamed `reasoning_details` delta: consecutive text or summary deltas merge
/// into one logical entry; encrypted entries stay discrete.
fn append_reasoning_detail(details: &mut Vec<Value>, detail: &Value) {
    let kind = detail.get("type").and_then(Value::as_str);
    let merged_field = match kind {
        Some("reasoning.text") => Some("text"),
        Some("reasoning.summary") => Some("summary"),
        _ => None,
    };
    if let Some(field) = merged_field
        && let Some(source) = detail.as_object()
        && let Some(Value::Object(last)) = details.last_mut()
        && last.get("type").and_then(Value::as_str) == kind
    {
        let merged = format!(
            "{}{}",
            last.get(field).and_then(Value::as_str).unwrap_or_default(),
            source
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        last.insert(field.into(), merged.into());
        if field == "text"
            && !truthy(last.get("signature"))
            && let Some(signature) = source.get("signature")
        {
            last.insert("signature".into(), signature.clone());
        }
        for key in ["id", "index"] {
            if last.get(key).is_none_or(Value::is_null)
                && let Some(value) = source.get(key)
            {
                last.insert(key.into(), value.clone());
            }
        }
        if !truthy(last.get("format"))
            && let Some(format) = source.get("format")
        {
            last.insert("format".into(), format.clone());
        }
        return;
    }
    details.push(detail.clone());
}

/// The OpenAI SDK's `APIError` message: the error's `message`, else the error as JSON,
/// else the raw body text, prefixed by the status.
fn api_error_message(status: Option<u16>, error: Option<&Value>, body: Option<&str>) -> String {
    let message = match error.filter(|error| truthy(Some(error))) {
        Some(error) => Some(match error.get("message") {
            Some(Value::String(text)) if !text.is_empty() => text.clone(),
            Some(message) if truthy(Some(message)) => message.to_string(),
            _ => error.to_string(),
        }),
        None => body.filter(|body| !body.is_empty()).map(str::to_owned),
    };
    match (status, message) {
        (Some(status), Some(message)) => format!("{status} {message}"),
        (Some(status), None) => format!("{status} status code (no body)"),
        (None, Some(message)) => message,
        (None, None) => "(no status code or body)".to_owned(),
    }
}

/// Append OpenRouter's `error.metadata.raw` detail unless the message already has it.
fn with_raw_metadata(mut message: String, error: Option<&Value>) -> String {
    if let Some(raw) = error
        .and_then(|error| error.pointer("/metadata/raw"))
        .filter(|raw| truthy(Some(raw)))
    {
        let raw = raw.as_str().map_or_else(|| raw.to_string(), str::to_owned);
        if !message.contains(&raw) {
            message.push('\n');
            message.push_str(&raw);
        }
    }
    message
}

/// Error message for a non-success HTTP response (pi `normalizeProviderError` +
/// `formatProviderError` over the SDK's status error): `"<status>: <error JSON>"` when
/// the body has an `error` object, else the SDK message.
fn http_error_message(status: u16, text: &str) -> String {
    let parsed = serde_json::from_str::<Value>(text)
        .ok()
        .filter(|value| truthy(Some(value)));
    let error = parsed.as_ref().and_then(|body| body.get("error"));
    let message = api_error_message(
        Some(status),
        error,
        if parsed.is_some() { None } else { Some(text) },
    );
    let message = match error {
        Some(Value::Object(fields)) if !fields.is_empty() => {
            let body = Value::Object(fields.clone()).to_string();
            let length = body.chars().count();
            let body = if length > MAX_PROVIDER_ERROR_BODY_CHARS {
                format!(
                    "{}... [truncated {} chars]",
                    body.chars()
                        .take(MAX_PROVIDER_ERROR_BODY_CHARS)
                        .collect::<String>(),
                    length - MAX_PROVIDER_ERROR_BODY_CHARS
                )
            } else {
                body
            };
            if message.contains(&body) {
                message
            } else {
                format!("{status}: {body}")
            }
        }
        _ => message,
    };
    with_raw_metadata(message, error)
}

/// Usage from a chunk's `usage` (pi `parseChunkUsage`). `cached_tokens` are cache reads;
/// `completion_tokens` already include reasoning tokens.
fn parse_usage(raw: &Value, model: &Model) -> Usage {
    let count = |value: Option<&Value>| {
        value
            .and_then(Value::as_f64)
            .map(|count| count.max(0.0) as u64)
    };
    let details = raw.get("prompt_tokens_details");
    let prompt = count(raw.get("prompt_tokens")).unwrap_or(0);
    let cache_read = count(details.and_then(|details| details.get("cached_tokens")))
        .or_else(|| count(raw.get("prompt_cache_hit_tokens")))
        .or_else(|| count(raw.get("cached_tokens")))
        .unwrap_or(0);
    let cache_write =
        count(details.and_then(|details| details.get("cache_write_tokens"))).unwrap_or(0);
    let input = prompt
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    let output = count(raw.get("completion_tokens")).unwrap_or(0);
    let reasoning = count(
        raw.get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens")),
    )
    .unwrap_or(0);
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: Some(reasoning),
        total_tokens: input + output + cache_read + cache_write,
        cost: Default::default(),
    };
    calculate_cost(model, &mut usage);
    usage
}

/// Map a `finish_reason` to a stop reason and, for failures, an error message.
fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

/// A tool call still streaming.
struct ToolState {
    /// Position in `output.content`.
    index: usize,
    /// `delta.tool_calls[].index`, once known.
    stream_index: Option<u64>,
    /// Raw arguments text received so far.
    args: String,
}

/// One streamed response: the message being built and the block bookkeeping.
struct Response {
    model: Model,
    tx: UnboundedSender<AssistantEvent>,
    output: AssistantMessage,
    /// The single text block of the response.
    text: Option<usize>,
    /// The single thinking block of the response.
    thinking: Option<usize>,
    /// Merged `reasoning_details`, written to the thinking block when it finishes.
    reasoning_details: Option<Vec<Value>>,
    tools: Vec<ToolState>,
    tools_by_id: HashMap<String, usize>,
    stop_reason: Option<StopReason>,
    has_finish_reason: bool,
}

impl Response {
    fn new(model: Model, tx: UnboundedSender<AssistantEvent>) -> Self {
        let output = AssistantMessage::empty(&model);
        Self {
            model,
            tx,
            output,
            text: None,
            thinking: None,
            reasoning_details: None,
            tools: Vec::new(),
            tools_by_id: HashMap::new(),
            stop_reason: None,
            has_finish_reason: false,
        }
    }

    /// Run the request and send its terminal event.
    async fn run(
        mut self,
        http: &reqwest::Client,
        transcript: &Transcript,
        opts: &StreamOptions,
        cancel: &CancellationToken,
    ) {
        let event = match self.request(http, transcript, opts, cancel).await {
            Ok(reason) => {
                self.output.stop_reason = reason;
                AssistantEvent::Done {
                    reason,
                    message: self.output,
                }
            }
            Err(message) => {
                self.apply_reasoning_details();
                let reason = if cancel.is_cancelled() {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                self.output.stop_reason = reason;
                self.output.error_message = Some(message);
                AssistantEvent::Error {
                    reason,
                    message: self.output,
                }
            }
        };
        let _ = self.tx.unbounded_send(event);
    }

    /// Send the request and stream the reply; the error is the message's `error_message`.
    async fn request(
        &mut self,
        http: &reqwest::Client,
        transcript: &Transcript,
        opts: &StreamOptions,
        cancel: &CancellationToken,
    ) -> Result<StopReason, String> {
        let base_url = opts
            .auth
            .base_url
            .as_deref()
            .unwrap_or(&self.model.base_url)
            .trim_end_matches('/')
            .to_owned();
        let compat = resolve_compat(&self.model, &base_url);
        let headers = build_headers(&self.model, &compat, opts)?;
        let params = build_params(&self.model, transcript, &opts.simple, &compat, &base_url);
        let url = format!("{base_url}/chat/completions");
        let response = send_with_retry(
            || http.post(&url).headers(headers.clone()).json(&params),
            &RetryPolicy::default(),
            cancel,
        )
        .await
        .map_err(|error| match error {
            Error::Aborted => ABORTED.to_owned(),
            error => error.to_string(),
        })?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            self.output.failure = Some(super::http::response_failure(&response));
            let Some(text) = error_body(response, cancel).await else {
                return Err(ABORTED.to_owned());
            };
            if let Ok(body) = serde_json::from_str::<Value>(&text) {
                super::http::record_failure(&mut self.output, &body);
            }
            return Err(http_error_message(status, &text));
        }
        let _ = self.tx.unbounded_send(AssistantEvent::Start {
            partial: self.output.clone(),
        });

        let mut body = std::pin::pin!(response.bytes_stream());
        let mut decoder = SseDecoder::new();
        let mut done = false;
        while !done {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ABORTED.to_owned()),
                chunk = body.next() => chunk,
            };
            let events = match chunk {
                Some(Ok(bytes)) => decoder.push(&bytes),
                Some(Err(error)) => return Err(error.to_string()),
                None => {
                    done = true;
                    std::mem::take(&mut decoder).finish().into_iter().collect()
                }
            };
            for event in &events {
                if self.handle_event(event)? {
                    done = true;
                    break;
                }
            }
        }

        self.finish_blocks();
        if cancel.is_cancelled() {
            return Err(ABORTED.to_owned());
        }
        if !self.has_finish_reason && !compat.supports_finish_reason {
            self.stop_reason = Some(if self.output.tool_calls().next().is_some() {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            });
        }
        if self.stop_reason == Some(StopReason::Error) {
            return Err(self
                .output
                .error_message
                .clone()
                .unwrap_or_else(|| "Provider returned an error stop reason".to_owned()));
        }
        match self.stop_reason {
            Some(reason) if self.has_finish_reason || !compat.supports_finish_reason => Ok(reason),
            _ => Err("Stream ended without finish_reason".to_owned()),
        }
    }

    /// Handle one SSE event; `true` at the `[DONE]` sentinel.
    fn handle_event(&mut self, event: &SseEvent) -> Result<bool, String> {
        let data = event.data.as_str();
        if data.starts_with("[DONE]") {
            return Ok(true);
        }
        if data.is_empty()
            || event
                .event
                .as_deref()
                .is_some_and(|name| name.starts_with("thread."))
        {
            return Ok(false);
        }
        let chunk: Value = serde_json::from_str(data).map_err(|error| error.to_string())?;
        if let Some(error) = chunk.get("error").filter(|error| truthy(Some(error))) {
            super::http::record_failure(&mut self.output, &chunk);
            return Err(with_raw_metadata(
                api_error_message(None, Some(error), None),
                Some(error),
            ));
        }
        self.apply_chunk(&chunk);
        Ok(false)
    }

    fn apply_chunk(&mut self, chunk: &Value) {
        if !chunk.is_object() {
            return;
        }
        if self.output.response_id.is_none()
            && let Some(id) = chunk
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        {
            self.output.response_id = Some(id.to_owned());
        }
        if self.output.response_model.is_none()
            && let Some(name) = chunk.get("model").and_then(Value::as_str)
            && !name.is_empty()
            && name != self.model.id
        {
            self.output.response_model = Some(name.to_owned());
        }
        let usage = chunk.get("usage").filter(|usage| truthy(Some(usage)));
        if let Some(usage) = usage {
            self.output.usage = parse_usage(usage, &self.model);
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .filter(|choice| truthy(Some(choice)))
        else {
            return;
        };
        // Some providers (Moonshot) report usage on the choice.
        if usage.is_none()
            && let Some(usage) = choice.get("usage").filter(|usage| truthy(Some(usage)))
        {
            self.output.usage = parse_usage(usage, &self.model);
        }
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
        {
            self.output.raw_stop_reason = Some(reason.to_owned());
            let (stop_reason, error_message) = map_stop_reason(reason);
            self.stop_reason = Some(stop_reason);
            if error_message.is_some() {
                self.output.error_message = error_message;
            }
            self.has_finish_reason = true;
        }

        let Some(delta) = choice.get("delta").filter(|delta| delta.is_object()) else {
            return;
        };
        // Reasoning before content: proxies may put both in one delta, and the thinking
        // block must come first.
        if let Some((field, text)) = REASONING_FIELDS.iter().find_map(|field| {
            delta
                .get(*field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| (*field, text))
        }) {
            let index = self.thinking_block(Some(field));
            if let Some(AssistantBlock::Thinking { thinking, .. }) =
                self.output.content.get_mut(index)
            {
                thinking.push_str(text);
            }
            let _ = self.tx.unbounded_send(AssistantEvent::ThinkingDelta {
                content_index: index,
                delta: text.to_owned(),
            });
        }
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in details.iter().filter(|detail| is_reasoning_detail(detail)) {
                self.thinking_block(None);
                append_reasoning_detail(self.reasoning_details.get_or_insert_default(), detail);
            }
        }
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            let index = match self.text {
                Some(index) => index,
                None => {
                    let index = self.output.content.len();
                    self.output.content.push(AssistantBlock::Text {
                        text: String::new(),
                        text_signature: None,
                    });
                    self.text = Some(index);
                    let _ = self.tx.unbounded_send(AssistantEvent::TextStart {
                        content_index: index,
                    });
                    index
                }
            };
            if let Some(AssistantBlock::Text { text: block, .. }) =
                self.output.content.get_mut(index)
            {
                block.push_str(text);
            }
            let _ = self.tx.unbounded_send(AssistantEvent::TextDelta {
                content_index: index,
                delta: text.to_owned(),
            });
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.tool_call_delta(call);
            }
        }
    }

    /// The response's thinking block, created on first use. `field` is the delta field
    /// the reasoning text arrived in, kept for replay.
    fn thinking_block(&mut self, field: Option<&str>) -> usize {
        if let Some(index) = self.thinking {
            return index;
        }
        let index = self.output.content.len();
        self.output.content.push(AssistantBlock::Thinking {
            thinking: String::new(),
            replay: field.map(|field| ReasoningReplay::ReasoningField(field.to_owned())),
            redacted: false,
        });
        self.thinking = Some(index);
        let _ = self.tx.unbounded_send(AssistantEvent::ThinkingStart {
            content_index: index,
        });
        index
    }

    /// Apply one `delta.tool_calls[]` entry. Calls are keyed by their stream `index`,
    /// falling back to `id`; parallel calls stream interleaved.
    fn tool_call_delta(&mut self, call: &Value) {
        let stream_index = call.get("index").and_then(Value::as_u64);
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let function = call.get("function");
        let name = function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let arguments_delta = function
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        let found = stream_index
            .and_then(|stream_index| {
                self.tools
                    .iter()
                    .position(|tool| tool.stream_index == Some(stream_index))
            })
            .or_else(|| id.and_then(|id| self.tools_by_id.get(id).copied()));
        let position = match found {
            Some(position) => position,
            None => {
                let index = self.output.content.len();
                self.output.content.push(AssistantBlock::ToolCall(ToolCall {
                    id: id.unwrap_or_default().to_owned(),
                    name: name.to_owned(),
                    arguments: JsonMap::new(),
                    arguments_error: None,
                }));
                self.tools.push(ToolState {
                    index,
                    stream_index,
                    args: String::new(),
                });
                let _ = self.tx.unbounded_send(AssistantEvent::ToolCallStart {
                    content_index: index,
                    id: id.unwrap_or_default().to_owned(),
                    name: name.to_owned(),
                });
                self.tools.len() - 1
            }
        };
        if let Some(id) = id {
            self.tools_by_id.insert(id.to_owned(), position);
        }
        let tool = &mut self.tools[position];
        if tool.stream_index.is_none() {
            tool.stream_index = stream_index;
        }
        let arguments = (!arguments_delta.is_empty()).then(|| {
            tool.args.push_str(arguments_delta);
            parse_streaming(&tool.args)
        });
        let index = tool.index;
        if let Some(AssistantBlock::ToolCall(block)) = self.output.content.get_mut(index) {
            if block.id.is_empty()
                && let Some(id) = id
            {
                block.id = id.to_owned();
            }
            if block.name.is_empty() && !name.is_empty() {
                block.name = name.to_owned();
            }
            if let Some(arguments) = arguments {
                block.arguments = arguments;
            }
        }
        let _ = self.tx.unbounded_send(AssistantEvent::ToolCallDelta {
            content_index: index,
            delta: arguments_delta.to_owned(),
        });
    }

    /// Store merged `reasoning_details` as the thinking block's replay data.
    fn apply_reasoning_details(&mut self) {
        let Some(details) = &self.reasoning_details else {
            return;
        };
        for block in &mut self.output.content {
            if let AssistantBlock::Thinking { replay, .. } = block {
                *replay = Some(ReasoningReplay::ReasoningDetails(Value::Array(
                    details.clone(),
                )));
            }
        }
    }

    /// Finish every block in content order once the stream has ended: tool-call
    /// arguments get their final parse, and each `*End` carries the final block.
    fn finish_blocks(&mut self) {
        self.apply_reasoning_details();
        for index in 0..self.output.content.len() {
            if let AssistantBlock::ToolCall(call) = &mut self.output.content[index]
                && let Some(tool) = self.tools.iter().find(|tool| tool.index == index)
            {
                (call.arguments, call.arguments_error) = parse_tool_arguments(&tool.args);
            }
            let event = match self.output.content[index].clone() {
                block @ AssistantBlock::Text { .. } => AssistantEvent::TextEnd {
                    content_index: index,
                    block,
                },
                block @ AssistantBlock::Thinking { .. } => AssistantEvent::ThinkingEnd {
                    content_index: index,
                    block,
                },
                block @ AssistantBlock::ToolCall(_) => AssistantEvent::ToolCallEnd {
                    content_index: index,
                    block,
                },
            };
            let _ = self.tx.unbounded_send(event);
        }
    }
}
