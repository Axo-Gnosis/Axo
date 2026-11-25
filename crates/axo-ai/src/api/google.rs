//! Google Gemini `generateContent` over SSE (`POST {base}/models/{id}:streamGenerateContent`).
//! Port of pi `api/google-shared.ts` and `google-generative-ai.ts`, with oh-my-pi's
//! `google-shared.ts` schema and signature handling. Thought signatures ride on the parts
//! that carry them and are replayed to the same model.

use std::collections::HashSet;
use std::ops::ControlFlow;

use futures::stream::BoxStream;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::http::{dispatch, read_sse, record_failure};
use crate::api::{Api, ProviderRequest, Reply, StreamOptions, ToolChoice, header_map, respond};
use crate::cost::calculate_cost;
use crate::event::AssistantEvent;
use crate::model::Model;
use crate::thinking::{clamp_level, default_thinking_budget, provider_value};
use crate::transcript::{Transcript, current_tools, system_message_text};
use crate::types::{
    AssistantBlock, Message, ReasoningReplay, StopReason, ThinkingLevel, ToolCall, UserBlock,
    now_ms,
};

/// Sent on a function call without a signature of its own (another model's, or a replay
/// that lost it); Gemini accepts it in place of a real one.
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// Streams Gemini replies.
pub struct GoogleApi {
    http: reqwest::Client,
}

impl GoogleApi {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

impl Api for GoogleApi {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: StreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent> {
        let tier = model.effective_tier(opts.simple.service_tier);
        let http = self.http.clone();
        let owned_model = model.clone();
        respond(model, tier, cancel, move |mut reply, cancel| async move {
            let result = async {
                let (request, level) = build_request(&owned_model, &transcript, &opts)?;
                reply.message.provider_thinking_level = level;
                let response = dispatch(
                    &http,
                    &opts,
                    &owned_model,
                    request,
                    &cancel,
                    &mut reply,
                    describe,
                )
                .await?;
                let mut state = Decoder::new(&owned_model, &mut reply);
                read_sse(response, &cancel, |event| state.handle(&event.data)).await?;
                state.finish()
            }
            .await;
            (reply, result)
        })
    }
}

/// The request, and the thinking level sent (for `provider_thinking_level`).
fn build_request(
    model: &Model,
    transcript: &Transcript,
    opts: &StreamOptions,
) -> Result<(ProviderRequest, Option<String>), String> {
    let transcript = transcript.collapse_system_messages();
    let mut body = json!({ "contents": contents(&transcript) });
    if let Some(text) = transcript
        .initial_system_message()
        .map(system_message_text)
        .filter(|text| !text.is_empty())
    {
        body["systemInstruction"] = json!({ "parts": [{ "text": text }] });
    }
    let tools = current_tools(&transcript);
    if !tools.is_empty() {
        let declarations: Vec<Value> = tools
            .iter()
            .map(|tool| {
                let mut schema = tool.parameters.clone();
                const_to_enum(&mut schema);
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parametersJsonSchema": schema,
                })
            })
            .collect();
        body["tools"] = json!([{ "functionDeclarations": declarations }]);
    }
    if opts.simple.tool_choice == Some(ToolChoice::None) {
        body["toolConfig"] = json!({ "functionCallingConfig": { "mode": "NONE" } });
    }
    let mut generation = Map::new();
    if let Some(temperature) = opts.simple.temperature {
        generation.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = opts.simple.max_tokens {
        generation.insert("maxOutputTokens".into(), json!(max_tokens));
    }
    let mut sent_level = None;
    if model.reasoning {
        let level = clamp_level(model, opts.simple.thinking);
        let value = provider_value(model, level);
        let config = match (level, &value) {
            (ThinkingLevel::Off, Some(value)) => {
                json!({ "thinkingLevel": value.to_ascii_uppercase() })
            }
            (ThinkingLevel::Off, None) => json!({ "thinkingBudget": 0 }),
            // An effort the catalog lists is a level; a budget model maps no level.
            (_, Some(value))
                if model
                    .thinking_level_map
                    .as_ref()
                    .is_some_and(|map| map.get(&level).is_some_and(Option::is_some)) =>
            {
                json!({ "includeThoughts": true, "thinkingLevel": value.to_ascii_uppercase() })
            }
            _ => json!({
                "includeThoughts": true,
                "thinkingBudget": default_thinking_budget(level),
            }),
        };
        generation.insert("thinkingConfig".into(), config);
        sent_level = Some(value.unwrap_or_else(|| level.as_str().to_owned()));
    }
    if !generation.is_empty() {
        body["generationConfig"] = Value::Object(generation);
    }

    let key = opts.auth.api_key.as_deref().filter(|key| !key.is_empty());
    let headers = header_map([
        vec![
            ("accept".to_owned(), Some("text/event-stream".to_owned())),
            (
                "content-type".to_owned(),
                Some("application/json".to_owned()),
            ),
        ],
        key.map(|key| ("x-goog-api-key".to_owned(), Some(key.to_owned())))
            .into_iter()
            .collect(),
        opts.configured_headers().into_iter().collect(),
    ])?;
    let url = format!(
        "{}/models/{}:streamGenerateContent?alt=sse",
        opts.base_url(model),
        model.id
    );
    Ok((ProviderRequest { url, headers, body }, sent_level))
}

/// A replayable signature: Gemini rejects anything that is not base64.
fn valid_signature(signature: Option<&str>) -> Option<&str> {
    signature.filter(|signature| {
        !signature.is_empty()
            && signature.len() % 4 == 0
            && signature
                .trim_end_matches('=')
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
            && signature.len() - signature.trim_end_matches('=').len() <= 2
    })
}

fn user_parts(content: &[UserBlock]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            UserBlock::Text { text } if text.is_empty() => None,
            UserBlock::Text { text } => Some(json!({ "text": text })),
            UserBlock::Image { data, mime_type } => {
                Some(json!({ "inlineData": { "mimeType": mime_type, "data": &**data } }))
            }
        })
        .collect()
}

/// `contents`: user and model turns, consecutive tool results merged into one user turn,
/// their images in one more user turn after it.
fn contents(transcript: &Transcript) -> Vec<Value> {
    let mut contents = Vec::new();
    let mut responses: Vec<Value> = Vec::new();
    let mut images: Vec<Value> = Vec::new();
    // ponytail: tool-result images always go in a separate user turn; nest them in functionResponse.parts behind a compat flag if it matters
    let flush = |contents: &mut Vec<Value>, responses: &mut Vec<Value>, images: &mut Vec<Value>| {
        if !responses.is_empty() {
            contents.push(json!({ "role": "user", "parts": std::mem::take(responses) }));
        }
        if !images.is_empty() {
            contents.push(json!({ "role": "user", "parts": std::mem::take(images) }));
        }
    };
    for message in transcript.iter() {
        match message {
            Message::ToolResult(result) => {
                let text: Vec<&str> = result
                    .content
                    .iter()
                    .filter_map(UserBlock::as_text)
                    .collect();
                let image_parts: Vec<Value> = user_parts(&result.content)
                    .into_iter()
                    .filter(|part| part.get("inlineData").is_some())
                    .collect();
                let text = if text.is_empty() && !image_parts.is_empty() {
                    "(see attached image)".to_owned()
                } else {
                    text.join("\n")
                };
                let key = if result.is_error { "error" } else { "output" };
                responses.push(json!({ "functionResponse": {
                    "name": result.tool_name,
                    "id": result.tool_call_id,
                    "response": { key: text },
                }}));
                if !image_parts.is_empty() {
                    images.push(json!({ "text": "Tool result image:" }));
                    images.extend(image_parts);
                }
            }
            Message::User(user) => {
                flush(&mut contents, &mut responses, &mut images);
                let parts = user_parts(&user.content);
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
            Message::Assistant(assistant) => {
                flush(&mut contents, &mut responses, &mut images);
                let parts: Vec<Value> = assistant.content.iter().filter_map(model_part).collect();
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            Message::System(_) => {}
        }
    }
    flush(&mut contents, &mut responses, &mut images);
    contents
}

fn model_part(block: &AssistantBlock) -> Option<Value> {
    let signed = |mut part: Value, signature: Option<&str>| {
        if let Some(signature) = valid_signature(signature) {
            part["thoughtSignature"] = json!(signature);
        }
        part
    };
    match block {
        AssistantBlock::Text {
            text,
            text_signature,
        } => {
            let signature = valid_signature(text_signature.as_deref());
            (!text.trim().is_empty() || signature.is_some())
                .then(|| signed(json!({ "text": text }), signature))
        }
        AssistantBlock::Thinking {
            thinking, replay, ..
        } => {
            let signature = match replay {
                Some(ReasoningReplay::GoogleThoughtSignature(signature)) => {
                    valid_signature(Some(signature))
                }
                _ => None,
            };
            (!thinking.trim().is_empty() || signature.is_some())
                .then(|| signed(json!({ "thought": true, "text": thinking }), signature))
        }
        AssistantBlock::ToolCall(call) => {
            let signature =
                valid_signature(call.thought_signature.as_deref()).unwrap_or(SKIP_SIGNATURE);
            Some(json!({
                "functionCall": { "name": call.name, "args": call.arguments, "id": call.id },
                "thoughtSignature": signature,
            }))
        }
    }
}

/// Gemini's schema subset has no `const`: a string or number `const` becomes a one-value
/// `enum`, in every subschema.
fn const_to_enum(schema: &mut Value) {
    const MAPS: [&str; 6] = [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
        "dependencies",
    ];
    const SCHEMAS: [&str; 16] = [
        "additionalProperties",
        "unevaluatedProperties",
        "propertyNames",
        "items",
        "prefixItems",
        "additionalItems",
        "unevaluatedItems",
        "contains",
        "allOf",
        "anyOf",
        "oneOf",
        "not",
        "if",
        "then",
        "else",
        "contentSchema",
    ];
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if let Some(value) = object.get("const")
        && (value.is_string() || value.is_number())
    {
        let value = object.remove("const").expect("present");
        object.insert("enum".into(), json!([value]));
    }
    for key in MAPS {
        if let Some(Value::Object(map)) = object.get_mut(key) {
            map.values_mut().for_each(const_to_enum);
        }
    }
    for key in SCHEMAS {
        match object.get_mut(key) {
            Some(Value::Array(items)) => items.iter_mut().for_each(const_to_enum),
            Some(value) => const_to_enum(value),
            None => {}
        }
    }
}

/// Decodes the SSE frames into the reply.
struct Decoder<'a> {
    model: &'a Model,
    reply: &'a mut Reply,
    /// The open text or thinking block.
    open: Option<usize>,
    ids: HashSet<String>,
    finished: Option<StopReason>,
}

impl<'a> Decoder<'a> {
    fn new(model: &'a Model, reply: &'a mut Reply) -> Self {
        reply.start();
        Self {
            model,
            reply,
            open: None,
            ids: HashSet::new(),
            finished: None,
        }
    }

    fn close(&mut self) {
        if let Some(index) = self.open.take() {
            self.reply.end_block(index);
        }
    }

    fn handle(&mut self, data: &str) -> Result<ControlFlow<()>, String> {
        if data.trim().is_empty() {
            return Ok(ControlFlow::Continue(()));
        }
        let mut frame: Value = serde_json::from_str(data)
            .map_err(|error| format!("Invalid Google SSE JSON: {error}"))?;
        // The Cloud Code envelope wraps each frame in `response`.
        if frame.get("candidates").is_none()
            && let Some(inner) = frame.get_mut("response").filter(|inner| inner.is_object())
        {
            frame = inner.take();
        }
        if let Some(error) = frame.get("error") {
            record_failure(&mut self.reply.message, &frame);
            let message = error["message"].as_str().unwrap_or("unknown error");
            let status = error["status"].as_str().unwrap_or_default();
            return Err(format!("Google API stream error: {message} ({status})"));
        }
        let candidate = frame.pointer("/candidates/0");
        if candidate.is_none()
            && let Some(reason) = frame
                .pointer("/promptFeedback/blockReason")
                .and_then(Value::as_str)
        {
            return Err(format!("Request blocked by Google ({reason})"));
        }
        let message = &mut self.reply.message;
        if message.response_id.is_none()
            && let Some(id) = frame["responseId"].as_str().filter(|id| !id.is_empty())
        {
            message.response_id = Some(id.to_owned());
        }
        if let Some(version) = frame["modelVersion"]
            .as_str()
            .filter(|v| *v != self.model.id)
        {
            message.response_model = Some(version.to_owned());
        }
        if let Some(parts) = candidate
            .and_then(|candidate| candidate.pointer("/content/parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                self.part(part);
            }
        }
        if let Some(reason) = candidate.and_then(|candidate| candidate["finishReason"].as_str()) {
            self.reply.message.raw_stop_reason = Some(reason.to_owned());
            // ponytail: CONTINUATION ends the turn as Length; resume with the continuation token when a provider needs it
            self.finished = Some(match reason {
                "STOP" if self.reply.message.tool_calls().next().is_some() => StopReason::ToolUse,
                "STOP" => StopReason::Stop,
                "MAX_TOKENS" | "CONTINUATION" => StopReason::Length,
                other => return Err(format!("Provider stopped with: {other}")),
            });
        }
        if let Some(usage) = frame.get("usageMetadata") {
            self.usage(usage);
        }
        Ok(ControlFlow::Continue(()))
    }

    fn part(&mut self, part: &Value) {
        let signature = part["thoughtSignature"]
            .as_str()
            .filter(|signature| !signature.is_empty());
        if let Some(call) = part.get("functionCall") {
            self.close();
            let index = self.reply.message.content.len();
            let id = call["id"]
                .as_str()
                .filter(|id| !id.is_empty() && !self.ids.contains(*id))
                .map_or_else(|| format!("call_{}_{index}", now_ms()), str::to_owned);
            self.ids.insert(id.clone());
            let arguments = call["args"].as_object().cloned().unwrap_or_default();
            let name = call["name"].as_str().unwrap_or_default().to_owned();
            let delta = Value::Object(arguments.clone()).to_string();
            self.reply
                .message
                .content
                .push(AssistantBlock::ToolCall(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments,
                    arguments_error: None,
                    thought_signature: signature.map(str::to_owned),
                }));
            self.reply.emit(AssistantEvent::ToolCallStart {
                content_index: index,
                id,
                name,
            });
            self.reply.emit(AssistantEvent::ToolCallDelta {
                content_index: index,
                delta,
            });
            self.reply.end_block(index);
            return;
        }
        let Some(text) = part["text"].as_str() else {
            return;
        };
        let thought = part["thought"].as_bool() == Some(true);
        let same_kind = self.open.is_some_and(|index| {
            matches!(
                (&self.reply.message.content[index], thought),
                (AssistantBlock::Thinking { .. }, true) | (AssistantBlock::Text { .. }, false)
            )
        });
        if !same_kind {
            self.close();
            let index = self.reply.message.content.len();
            let (block, start) = if thought {
                (
                    AssistantBlock::Thinking {
                        thinking: String::new(),
                        replay: None,
                        redacted: false,
                    },
                    AssistantEvent::ThinkingStart {
                        content_index: index,
                    },
                )
            } else {
                (
                    AssistantBlock::Text {
                        text: String::new(),
                        text_signature: None,
                    },
                    AssistantEvent::TextStart {
                        content_index: index,
                    },
                )
            };
            self.reply.message.content.push(block);
            self.reply.emit(start);
            self.open = Some(index);
        }
        let index = self.open.expect("opened above");
        let delta = text.to_owned();
        let event = match &mut self.reply.message.content[index] {
            AssistantBlock::Thinking {
                thinking, replay, ..
            } => {
                thinking.push_str(text);
                if let Some(signature) = signature {
                    *replay = Some(ReasoningReplay::GoogleThoughtSignature(
                        signature.to_owned(),
                    ));
                }
                AssistantEvent::ThinkingDelta {
                    content_index: index,
                    delta,
                }
            }
            AssistantBlock::Text {
                text: body,
                text_signature,
            } => {
                body.push_str(text);
                if let Some(signature) = signature {
                    *text_signature = Some(signature.to_owned());
                }
                AssistantEvent::TextDelta {
                    content_index: index,
                    delta,
                }
            }
            AssistantBlock::ToolCall(_) => unreachable!("only text blocks stay open"),
        };
        if !text.is_empty() {
            self.reply.emit(event);
        }
    }

    fn usage(&mut self, metadata: &Value) {
        let count = |key: &str| metadata[key].as_u64();
        let candidates = count("candidatesTokenCount").unwrap_or(0);
        let thinking = count("thoughtsTokenCount").unwrap_or(0);
        let total = count("totalTokenCount").unwrap_or(0);
        let prompt = count("promptTokenCount")
            .unwrap_or_else(|| total.saturating_sub(candidates + thinking));
        let cache_read = count("cachedContentTokenCount").unwrap_or(0).min(prompt);
        let usage = &mut self.reply.message.usage;
        usage.input = prompt - cache_read;
        usage.output = candidates + thinking;
        usage.cache_read = cache_read;
        usage.reasoning = (thinking > 0).then_some(thinking);
        usage.total_tokens = total;
        calculate_cost(self.model, &mut self.reply.message);
    }

    fn finish(mut self) -> Result<StopReason, String> {
        self.close();
        self.finished
            .ok_or_else(|| "Google stream ended without a finish reason".to_owned())
    }
}

/// The HTTP failure message: `error.message`, else the raw body, after the status.
fn describe(status: u16, raw: &str, body: Option<&Value>) -> String {
    let message = body
        .and_then(|body| body.pointer("/error/message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or(raw);
    format!("Google API error ({status}): {message}")
}
