//! OpenAI Responses mapping shared by Responses transports. Port of pi
//! `api/openai-responses-shared.ts`: `convertResponsesMessages`, `convertResponsesTools`
//! and `processResponsesStream`.
//!
//! Unconstrained custom tools are supported. Not ported: grammar formats and strict
//! JSON-schema tools, which need pi's `constrainedSampling` metadata; `configuration_update`
//! effort items; tool-call namespaces. Tool-call ids are already normalized by
//! `transform_messages`.

use std::collections::{HashMap, HashSet};

use indexmap::IndexSet;
use serde_json::{Value, json};

use crate::api::{Reply, served_tier};
use crate::cost::calculate_cost;
use crate::event::AssistantEvent;
use crate::json::parse_tool_arguments;
use crate::model::{InputKind, Model};
use crate::transcript::{
    TranscriptToolSupport, render_system_message_update, resolve_transcript_tools,
    system_message_text,
};
use crate::transform::short_hash;
use crate::types::{
    AssistantBlock, Message, ReasoningReplay, StopReason, ToolCall, ToolSpec, Usage, UserBlock,
};

/// Options of [`convert_messages`] (pi `ConvertResponsesMessagesOptions`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConvertOptions {
    /// Later system messages are sent in place; otherwise the caller collapsed them.
    pub supports_mid_convo_system_messages: bool,
    pub supports_strict_mode: bool,
    pub supports_custom_tools: bool,
}

/// Collect raw input declarations before collapsing system messages, so calls to tools
/// since removed can still be replayed as custom calls.
pub(crate) fn raw_tool_inputs(messages: &[Message]) -> HashMap<&str, &str> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::System(system) => Some(&system.tools_added),
            _ => None,
        })
        .flatten()
        .filter_map(|tool| Some((tool.name.as_str(), tool.raw_input.as_deref()?)))
        .collect()
}

/// Convert a resolved transcript (see `Transcript::resolve`) into Responses `input` items.
pub(crate) fn convert_messages(
    model: &Model,
    messages: &[Message],
    options: &ConvertOptions,
    raw_inputs: &HashMap<&str, &str>,
) -> Vec<Value> {
    let support = model.tool_support();
    let transcript_tools = resolve_transcript_tools(messages, support);
    let instruction_role = if model.reasoning && model.compat.supports_developer_role != Some(false)
    {
        "developer"
    } else {
        "system"
    };
    let mut input = Vec::new();
    let mut declared_additional_tools = false;
    // Tools announced as removed and not declared again since.
    let mut unavailable: IndexSet<String> = IndexSet::new();
    let mut custom_calls = HashSet::new();

    let mut msg_index = 0usize;
    let mut system_index = 0usize;
    for (source_index, message) in messages.iter().enumerate() {
        let leading = source_index == 0 && matches!(message, Message::System(_));
        match message {
            Message::System(system) => {
                // Rows that take mid-conversation system items get the prompt as the leading
                // instruction item, so later prompt changes sit in the same item stream.
                if leading && options.supports_mid_convo_system_messages {
                    let text = system_message_text(system);
                    if !text.is_empty() {
                        input.push(json!({ "role": instruction_role, "content": text }));
                    }
                }
                let anchors_tool_changes = system_index >= transcript_tools.folded_system_messages;
                system_index += 1;
                if anchors_tool_changes {
                    for tool in &system.tools_added {
                        unavailable.shift_remove(&tool.name);
                    }
                    if !system.tools_added.is_empty() {
                        match support {
                            TranscriptToolSupport::Changes => {
                                if declared_additional_tools {
                                    // Without it, models read the latest item as the complete
                                    // tool set once a removal was announced.
                                    let notice = "Tool list update: tools declared here replace earlier definitions; other tools stay available unless listed as unavailable.";
                                    let content = if unavailable.is_empty() {
                                        notice.to_owned()
                                    } else {
                                        let names: Vec<&str> =
                                            unavailable.iter().map(String::as_str).collect();
                                        format!(
                                            "{notice}\nStill unavailable: {}.",
                                            names.join(", ")
                                        )
                                    };
                                    input.push(
                                        json!({ "role": instruction_role, "content": content }),
                                    );
                                }
                                declared_additional_tools = true;
                                input.push(json!({
                                    "type": "additional_tools",
                                    "role": "developer",
                                    "tools": convert_tools(&system.tools_added, options.supports_strict_mode, options.supports_custom_tools, false),
                                }));
                            }
                            TranscriptToolSupport::Additions => {
                                let names: Vec<&str> = system
                                    .tools_added
                                    .iter()
                                    .map(|tool| tool.name.as_str())
                                    .collect();
                                let call_id = format!(
                                    "tool_load_{}",
                                    short_hash(&format!("system:{msg_index}:{}", names.join(",")))
                                );
                                input.push(json!({
                                    "type": "tool_search_call",
                                    "call_id": call_id,
                                    "execution": "client",
                                    "status": "completed",
                                    "arguments": { "query": names.join(" "), "limit": names.len() },
                                }));
                                input.push(json!({
                                    "type": "tool_search_output",
                                    "call_id": call_id,
                                    "execution": "client",
                                    "status": "completed",
                                    "tools": convert_tools(&system.tools_added, options.supports_strict_mode, options.supports_custom_tools, true),
                                }));
                            }
                            TranscriptToolSupport::None => {}
                        }
                    }
                }
                if !leading {
                    let mut parts = vec![render_system_message_update(system)];
                    if anchors_tool_changes {
                        // A redefinition is a removal plus an addition of the same name; only
                        // true removals are announced.
                        let removed: Vec<&String> = system
                            .tools_removed
                            .iter()
                            .filter(|name| {
                                !system.tools_added.iter().any(|tool| &tool.name == *name)
                            })
                            .collect();
                        if !removed.is_empty() {
                            let lines: Vec<String> =
                                removed.iter().map(|name| format!("- {name}")).collect();
                            parts.push(format!(
                                "The following tools are no longer available. Do not call them:\n{}",
                                lines.join("\n")
                            ));
                        }
                        unavailable.extend(removed.into_iter().cloned());
                    }
                    let text = parts
                        .into_iter()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    if !text.is_empty() {
                        input.push(json!({ "role": instruction_role, "content": text }));
                    }
                }
            }
            Message::User(user) => {
                let content: Vec<Value> = user
                    .content
                    .iter()
                    .map(|block| match block {
                        UserBlock::Text { text } => json!({ "type": "input_text", "text": text }),
                        UserBlock::Image { data, mime_type } => json!({
                            "type": "input_image",
                            "detail": "auto",
                            "image_url": format!("data:{mime_type};base64,{data}"),
                        }),
                    })
                    .collect();
                if content.is_empty() {
                    continue;
                }
                input.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(assistant) => {
                let same_provider_and_api =
                    assistant.provider == model.provider && assistant.api == model.api;
                let different_model = same_provider_and_api && assistant.model != model.id;
                let mut output = Vec::new();
                let mut text_block_index = 0usize;
                for block in &assistant.content {
                    match block {
                        AssistantBlock::Thinking {
                            replay: Some(ReasoningReplay::OpenAiReasoningItem(item)),
                            ..
                        } => output.push(item.clone()),
                        AssistantBlock::Thinking { .. } => {}
                        AssistantBlock::Text {
                            text,
                            text_signature,
                        } => {
                            let signature = parse_text_signature(text_signature.as_deref());
                            let fallback_id = if text_block_index == 0 {
                                format!("msg_{msg_index}")
                            } else {
                                format!("msg_{msg_index}_{text_block_index}")
                            };
                            text_block_index += 1;
                            // OpenAI requires ids of at most 64 characters.
                            let id = match signature.as_ref().map(|(id, _)| id.as_str()) {
                                None | Some("") => fallback_id,
                                Some(id) if id.chars().count() > 64 => {
                                    format!("msg_{}", short_hash(id))
                                }
                                Some(id) => id.to_owned(),
                            };
                            let mut item = json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                                "status": "completed",
                                "id": id,
                            });
                            if let Some((_, Some(phase))) = signature {
                                item["phase"] = json!(phase);
                            }
                            output.push(item);
                        }
                        AssistantBlock::ToolCall(call) => {
                            let mut parts = call.id.split('|');
                            let call_id = parts.next().unwrap_or_default();
                            let item_id = parts.next();
                            let raw_input = raw_inputs
                                .get(call.name.as_str())
                                .filter(|_| call.arguments.len() == 1)
                                .and_then(|&property| call.arguments.get(property)?.as_str());
                            // A different model's item ids were paired with its reasoning
                            // items; omitting them avoids that validation.
                            let (mut item, item_id) = if let Some(text) = raw_input {
                                custom_calls.insert(call_id);
                                (
                                    json!({"type": "custom_tool_call", "input": text}),
                                    item_id.filter(|id| {
                                        same_provider_and_api
                                            && !different_model
                                            && id.starts_with("ctc_")
                                    }),
                                )
                            } else {
                                (
                                    json!({
                                        "type": "function_call",
                                        "arguments": serde_json::to_string(&call.arguments).unwrap_or_default(),
                                    }),
                                    item_id.filter(|id| !different_model && id.starts_with("fc_")),
                                )
                            };
                            if let Some(item_id) = item_id {
                                item["id"] = json!(item_id);
                            }
                            item["call_id"] = json!(call_id);
                            item["name"] = json!(call.name);
                            output.push(item);
                        }
                    }
                }
                if output.is_empty() {
                    continue;
                }
                input.extend(output);
            }
            Message::ToolResult(result) => {
                let call_id = result.tool_call_id.split('|').next().unwrap_or_default();
                let text = result
                    .content
                    .iter()
                    .filter_map(UserBlock::as_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                let images: Vec<_> = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::Image { data, mime_type } => Some((data, mime_type)),
                        UserBlock::Text { .. } => None,
                    })
                    .collect();
                let has_text = !text.trim().is_empty();
                let output = if images.is_empty() || !model.input.contains(&InputKind::Image) {
                    json!(if has_text {
                        text
                    } else if images.is_empty() {
                        "(no tool output)".to_owned()
                    } else {
                        "(see attached image)".to_owned()
                    })
                } else {
                    let mut parts = Vec::with_capacity(images.len() + 1);
                    if has_text {
                        parts.push(json!({ "type": "input_text", "text": text }));
                    }
                    parts.extend(images.into_iter().map(|(data, mime_type)| {
                        json!({
                            "type": "input_image",
                            "detail": "auto",
                            "image_url": format!("data:{mime_type};base64,{data}"),
                        })
                    }));
                    Value::Array(parts)
                };
                let kind = if custom_calls.remove(call_id) {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };
                input.push(json!({ "type": kind, "call_id": call_id, "output": output }));
            }
        }
        if !leading {
            msg_index += 1;
        }
    }
    input
}

/// Responses tool declarations. Function tools use `strict: null` where accepted;
/// strict schemas need constrained-sampling metadata axo tools do not have.
pub(crate) fn convert_tools(
    tools: &[ToolSpec],
    supports_strict_mode: bool,
    supports_custom_tools: bool,
    tool_search_result: bool,
) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let custom = supports_custom_tools && tool.raw_input.is_some();
            let mut declaration = json!({
                "type": if custom { "custom" } else { "function" },
                "name": tool.name,
                "description": tool.description,
            });
            if !custom {
                declaration["parameters"] = tool.parameters.clone();
                if supports_strict_mode {
                    declaration["strict"] = Value::Null;
                }
            }
            if tool_search_result {
                declaration["defer_loading"] = json!(true);
            }
            declaration
        })
        .collect()
}

/// pi `parseTextSignature`: a `TextSignatureV1` JSON string `{"v":1,"id":…,"phase"?:…}`,
/// or a legacy plain message id. Returns the id and the phase.
fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<String>)> {
    let signature = signature.filter(|signature| !signature.is_empty())?;
    if signature.starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Value>(signature)
        && parsed.get("v").and_then(Value::as_u64) == Some(1)
        && let Some(id) = parsed.get("id").and_then(Value::as_str)
    {
        let phase = parsed
            .get("phase")
            .and_then(Value::as_str)
            .filter(|phase| matches!(*phase, "commentary" | "final_answer"));
        return Some((id.to_owned(), phase.map(str::to_owned)));
    }
    Some((signature.to_owned(), None))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Thinking,
    Text,
    ToolCall,
    CustomToolCall,
}

/// An output item still streaming: its kind and the content block it fills.
#[derive(Debug, Clone, Copy)]
struct Slot {
    kind: SlotKind,
    content_index: usize,
}

/// Applies Responses stream events to an assistant message (pi `processResponsesStream`).
/// Feed every decoded event to [`ResponsesStream::handle`], then call
/// [`ResponsesStream::finish`] once the stream ends.
pub(crate) struct ResponsesStream<'a> {
    model: &'a Model,
    raw_inputs: HashMap<String, String>,
    slots: HashMap<u64, Slot>,
    /// Content index of each finished reasoning item, by item id.
    reasoning_by_id: HashMap<String, usize>,
    saw_terminal: bool,
}

impl<'a> ResponsesStream<'a> {
    pub fn new(model: &'a Model, raw_inputs: HashMap<String, String>) -> Self {
        Self {
            model,
            raw_inputs,
            slots: HashMap::new(),
            reasoning_by_id: HashMap::new(),
            saw_terminal: false,
        }
    }

    /// Apply one event to the reply. An `Err` is the error message the response fails with.
    pub fn handle(&mut self, event: &Value, reply: &mut Reply) -> Result<(), String> {
        let output_index = event.get("output_index").and_then(Value::as_u64);
        let delta = event
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "response.created" => {
                reply.message.response_id = event
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            "response.output_item.added" => {
                if let (Some(output_index), Some(item)) = (output_index, event.get("item")) {
                    self.create_slot(output_index, item, reply);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.push_delta(output_index, SlotKind::Thinking, delta, reply);
            }
            "response.reasoning_summary_part.done" => {
                self.push_delta(output_index, SlotKind::Thinking, "\n\n", reply);
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                self.push_delta(output_index, SlotKind::Text, delta, reply);
            }
            "response.function_call_arguments.delta" => {
                let Some(index) = self.slot(output_index, SlotKind::ToolCall) else {
                    return Ok(());
                };
                reply.push_tool_args(index, delta);
            }
            "response.function_call_arguments.done" => {
                let Some(index) = self.slot(output_index, SlotKind::ToolCall) else {
                    return Ok(());
                };
                let text = event
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let previous = reply.take_tool_args(index);
                reply.set_tool_args(index, text.to_owned());
                if let Some(rest) = text.strip_prefix(previous.as_str())
                    && !rest.is_empty()
                {
                    reply.emit(AssistantEvent::ToolCallDelta {
                        content_index: index,
                        delta: rest.to_owned(),
                    });
                }
            }
            "response.custom_tool_call_input.delta" => {
                if let Some(index) = self.slot(output_index, SlotKind::CustomToolCall) {
                    let quoted = serde_json::to_string(delta).expect("string is JSON serializable");
                    reply.push_tool_args(index, &quoted[1..quoted.len() - 1]);
                }
            }
            // The custom input's `.done` event is redundant with the finished item.
            "response.output_item.done" => {
                if let (Some(output_index), Some(item)) = (output_index, event.get("item")) {
                    self.finish_item(output_index, item, reply);
                }
            }
            "response.completed" => {
                self.finalize(event.get("response").unwrap_or(&Value::Null), reply)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Check the stream ended properly: a terminal response event arrived, and no tool
    /// call the agent would run is unfinished (its arguments may be cut off).
    pub fn finish(&self, reply: &Reply) -> Result<(), String> {
        if !self.saw_terminal {
            return Err(
                "OpenAI Responses stream ended before a terminal response event".to_owned(),
            );
        }
        if reply.message.stop_reason == StopReason::ToolUse {
            for slot in self.slots.values() {
                if let AssistantBlock::ToolCall(call) = &reply.message.content[slot.content_index] {
                    return Err(format!(
                        "OpenAI Responses stream completed with an unfinished tool call: {} ({})",
                        call.name, call.id
                    ));
                }
            }
        }
        Ok(())
    }

    fn slot(&self, output_index: Option<u64>, kind: SlotKind) -> Option<usize> {
        self.slots
            .get(&output_index?)
            .filter(|slot| slot.kind == kind)
            .map(|slot| slot.content_index)
    }

    fn push_delta(
        &mut self,
        output_index: Option<u64>,
        kind: SlotKind,
        delta: &str,
        reply: &mut Reply,
    ) {
        let Some(index) = self.slot(output_index, kind) else {
            return;
        };
        match reply.message.content.get_mut(index) {
            Some(AssistantBlock::Thinking { thinking, .. }) => {
                thinking.push_str(delta);
                reply.emit(AssistantEvent::ThinkingDelta {
                    content_index: index,
                    delta: delta.to_owned(),
                });
            }
            Some(AssistantBlock::Text { text, .. }) => {
                text.push_str(delta);
                reply.emit(AssistantEvent::TextDelta {
                    content_index: index,
                    delta: delta.to_owned(),
                });
            }
            _ => {}
        }
    }

    fn raw_property(&self, name: &str) -> &str {
        self.raw_inputs.get(name).map_or("input", String::as_str)
    }

    fn create_slot(&mut self, output_index: u64, item: &Value, reply: &mut Reply) -> Option<Slot> {
        let content_index = reply.message.content.len();
        let item_type = item.get("type").and_then(Value::as_str)?;
        let (kind, block, event) = match item_type {
            "reasoning" => (
                SlotKind::Thinking,
                AssistantBlock::Thinking {
                    thinking: String::new(),
                    replay: None,
                    redacted: false,
                },
                AssistantEvent::ThinkingStart { content_index },
            ),
            "message" => (
                SlotKind::Text,
                AssistantBlock::Text {
                    text: String::new(),
                    text_signature: None,
                },
                AssistantEvent::TextStart { content_index },
            ),
            "function_call" | "custom_tool_call" => {
                let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
                let id = format!("{}|{}", text("call_id"), text("id"));
                let name = text("name").to_owned();
                let kind = if item_type == "custom_tool_call" {
                    SlotKind::CustomToolCall
                } else {
                    reply.set_tool_args(content_index, text("arguments").to_owned());
                    SlotKind::ToolCall
                };
                (
                    kind,
                    AssistantBlock::ToolCall(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: Default::default(),
                        arguments_error: None,
                        thought_signature: None,
                    }),
                    AssistantEvent::ToolCallStart {
                        content_index,
                        id,
                        name,
                    },
                )
            }
            _ => return None,
        };
        reply.message.content.push(block);
        let slot = Slot {
            kind,
            content_index,
        };
        self.slots.insert(output_index, slot);
        reply.emit(event);
        if kind == SlotKind::CustomToolCall
            && let AssistantBlock::ToolCall(call) = &reply.message.content[content_index]
        {
            let opening = format!("{{{}:\"", json!(self.raw_property(&call.name)));
            reply.push_tool_args(content_index, &opening);
        }
        Some(slot)
    }

    fn finish_item(&mut self, output_index: u64, item: &Value, reply: &mut Reply) {
        let slot = match self.slots.get(&output_index) {
            Some(slot) => *slot,
            None => match self.create_slot(output_index, item, reply) {
                Some(slot) => slot,
                None => return,
            },
        };
        let index = slot.content_index;
        let item_type = item.get("type").and_then(Value::as_str);
        let partial = match (item_type, slot.kind) {
            (Some("function_call"), SlotKind::ToolCall)
            | (Some("custom_tool_call"), SlotKind::CustomToolCall) => reply.take_tool_args(index),
            _ => String::new(),
        };
        match (item_type, slot.kind, reply.message.content.get_mut(index)) {
            (
                Some("reasoning"),
                SlotKind::Thinking,
                Some(AssistantBlock::Thinking {
                    thinking, replay, ..
                }),
            ) => {
                let join = |key: &str| {
                    item.get(key)
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .map(|part| {
                                    part.get("text").and_then(Value::as_str).unwrap_or_default()
                                })
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default()
                };
                let summary = join("summary");
                let content = join("content");
                if !summary.is_empty() {
                    *thinking = summary;
                } else if !content.is_empty() {
                    *thinking = content;
                }
                *replay = Some(ReasoningReplay::OpenAiReasoningItem(item.clone()));
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    self.reasoning_by_id.insert(id.to_owned(), index);
                }
            }
            (
                Some("message"),
                SlotKind::Text,
                Some(AssistantBlock::Text {
                    text,
                    text_signature,
                }),
            ) => {
                *text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .map(|part| {
                                let key = if part.get("type").and_then(Value::as_str)
                                    == Some("output_text")
                                {
                                    "text"
                                } else {
                                    "refusal"
                                };
                                part.get(key).and_then(Value::as_str).unwrap_or_default()
                            })
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                // TextSignatureV1.
                let mut signature = json!({
                    "v": 1,
                    "id": item.get("id").and_then(Value::as_str).unwrap_or_default(),
                });
                if let Some(phase) = item
                    .get("phase")
                    .and_then(Value::as_str)
                    .filter(|phase| !phase.is_empty())
                {
                    signature["phase"] = json!(phase);
                }
                *text_signature = Some(signature.to_string());
            }
            (Some("function_call"), SlotKind::ToolCall, Some(AssistantBlock::ToolCall(call))) => {
                let text = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .unwrap_or(partial.as_str());
                (call.arguments, call.arguments_error) = parse_tool_arguments(text);
            }
            (
                Some("custom_tool_call"),
                SlotKind::CustomToolCall,
                Some(AssistantBlock::ToolCall(call)),
            ) => {
                let property = self.raw_property(&call.name);
                let text = item
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                call.arguments = [(property.to_owned(), json!(text))].into_iter().collect();
                call.arguments_error = None;
                // A finished item can also arrive without any or all of its deltas.
                const CLOSE: &str = "\"}";
                let complete = serde_json::to_string(&call.arguments).unwrap_or_default();
                if let Some(rest) = complete
                    .strip_suffix(CLOSE)
                    .and_then(|s| s.strip_prefix(&partial))
                    && !rest.is_empty()
                {
                    reply.emit(AssistantEvent::ToolCallDelta {
                        content_index: index,
                        delta: rest.to_owned(),
                    });
                }
                reply.emit(AssistantEvent::ToolCallDelta {
                    content_index: index,
                    delta: CLOSE.to_owned(),
                });
            }
            _ => return,
        };
        self.slots.remove(&output_index);
        reply.end_block(index);
    }

    fn finalize(&mut self, response: &Value, reply: &mut Reply) -> Result<(), String> {
        self.saw_terminal = true;

        // Some servers omit `encrypted_content` from `output_item.done` and send it only in
        // the terminal response; backfill it so `store: false` replay stays stateless.
        for item in response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let encrypted = item
                .get("encrypted_content")
                .filter(|value| value.as_str().is_some_and(|text| !text.is_empty()));
            let (Some("reasoning"), Some(encrypted), Some(id)) = (
                item.get("type").and_then(Value::as_str),
                encrypted,
                item.get("id").and_then(Value::as_str),
            ) else {
                continue;
            };
            let Some(&index) = self.reasoning_by_id.get(id) else {
                continue;
            };
            if let Some(AssistantBlock::Thinking {
                replay: Some(ReasoningReplay::OpenAiReasoningItem(Value::Object(stored))),
                ..
            }) = reply.message.content.get_mut(index)
                && !stored
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
            {
                stored.insert("encrypted_content".to_owned(), encrypted.clone());
            }
        }

        if let Some(id) = response.get("id").and_then(Value::as_str) {
            reply.message.response_id = Some(id.to_owned());
        }
        if let Some(usage) = response.get("usage").filter(|usage| usage.is_object()) {
            let count = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
            let cached = count("/input_tokens_details/cached_tokens");
            let cache_write = count("/input_tokens_details/cache_write_tokens");
            reply.message.usage = Usage {
                // OpenAI counts cached and cache-write tokens in input_tokens.
                input: count("/input_tokens").saturating_sub(cached + cache_write),
                output: count("/output_tokens"),
                cache_read: cached,
                cache_write,
                cache_write_1h: None,
                reasoning: Some(count("/output_tokens_details/reasoning_tokens")),
                total_tokens: count("/total_tokens"),
                cost: Default::default(),
            };
        }
        if let Some(echo) = response.get("service_tier").and_then(Value::as_str) {
            reply.message.service_tier = served_tier(echo);
        }
        calculate_cost(self.model, &mut reply.message);

        // Incomplete responses keep the provider's reason so max-output truncation and
        // content filtering stay distinct.
        let status = response.get("status").and_then(Value::as_str);
        let reason = response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty());
        reply.message.raw_stop_reason = match reason {
            Some(reason) => Some(format!("{}.{reason}", status.unwrap_or("undefined"))),
            None => status.map(str::to_owned),
        };
        let (stop_reason, error_message) = match status {
            None | Some("completed" | "in_progress" | "queued") => (StopReason::Stop, None),
            Some("incomplete") if reason == Some("max_output_tokens") => (StopReason::Length, None),
            Some("incomplete") => (
                StopReason::Error,
                Some(reason.map_or_else(
                    || "Response incomplete without a provider reason".to_owned(),
                    |reason| format!("Response incomplete: {reason}"),
                )),
            ),
            Some("failed" | "cancelled") => (StopReason::Error, None),
            Some(other) => return Err(format!("Unhandled stop reason: {other}")),
        };
        reply.message.stop_reason = stop_reason;
        // Error metadata can also arrive on a normalized Codex terminal event.
        if stop_reason == StopReason::Error {
            super::http::record_failure(&mut reply.message, response);
        }
        reply.message.error_message = error_message.or_else(|| {
            (stop_reason == StopReason::Error)
                .then(|| {
                    response
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .flatten()
        });
        if stop_reason == StopReason::Stop && reply.message.tool_calls().next().is_some() {
            reply.message.stop_reason = StopReason::ToolUse;
        }
        Ok(())
    }
}
