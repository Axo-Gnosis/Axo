//! Cross-model replay. Port of pi `api/transform-messages.ts`, with each adapter's
//! tool-call id normalizer selected by the target model's API.

use std::collections::{HashMap, HashSet};

use crate::model::{ApiKind, InputKind, Model};
use crate::transcript::Transcript;
use crate::types::{
    AssistantBlock, AssistantMessage, Message, StopReason, ToolCall, ToolResultMessage, UserBlock,
    now_ms,
};

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";
const ERROR_TOOL_IMAGE_PLACEHOLDER: &str = "(image attached after the tool results)";

/// Providers whose Responses `call_id|item_id` tool-call ids the Codex adapter replays
/// as-is (pi `CODEX_TOOL_CALL_PROVIDERS`).
const CODEX_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];

/// An error tool result split into a text-only result and the images to send after the
/// tool results.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitErrorToolResult {
    /// The result content with each run of images replaced by a note.
    pub content: Vec<UserBlock>,
    /// Text naming the tool call, to send before `images`.
    pub caption: String,
    /// The image blocks.
    pub images: Vec<UserBlock>,
}

/// Split the images out of an error tool result, for APIs that reject images in error
/// results (Anthropic). Returns `None` for results that need no split.
pub fn split_error_tool_result_images(message: &ToolResultMessage) -> Option<SplitErrorToolResult> {
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
        content: replace_images(&message.content, ERROR_TOOL_IMAGE_PLACEHOLDER),
        caption: format!(
            "Images from the error result of tool call {} ({}):",
            message.tool_call_id, message.tool_name
        ),
        images,
    })
}

/// Replace each run of images with one placeholder text block.
fn replace_images(content: &[UserBlock], placeholder: &str) -> Vec<UserBlock> {
    let mut result = Vec::with_capacity(content.len());
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            UserBlock::Image { .. } => {
                if !previous_was_placeholder {
                    result.push(UserBlock::Text {
                        text: placeholder.to_owned(),
                    });
                }
                previous_was_placeholder = true;
            }
            UserBlock::Text { text } => {
                previous_was_placeholder = text == placeholder;
                result.push(block.clone());
            }
        }
    }
    result
}

/// Make a transcript produced by any model replayable to `model`:
/// - images become placeholders when the model is text-only;
/// - thinking from another model is dropped (signed/redacted thinking is kept for the
///   same model, empty unsigned thinking is dropped);
/// - text signatures from another model are dropped;
/// - tool-call ids from another model are normalized for the target API, and the
///   matching tool results follow;
/// - errored and aborted assistant turns are left out;
/// - a tool call without a result gets a `"No result provided"` error result before the
///   next assistant or user message (system messages between a call and its results are
///   held until after the results).
pub fn transform_messages(transcript: &Transcript, model: &Model) -> Transcript {
    let vision = model.input.contains(&InputKind::Image);
    let mut id_map: HashMap<String, String> = HashMap::new();

    // First pass: image downgrade, thinking/text filtering, tool-call id normalization.
    let transformed = transcript.iter().map(|message| match message {
        Message::User(user) if !vision => {
            let mut user = user.clone();
            user.content = replace_images(&user.content, NON_VISION_USER_IMAGE_PLACEHOLDER);
            Message::User(user)
        }
        Message::ToolResult(result) => {
            let mut result = result.clone();
            if !vision {
                result.content = replace_images(&result.content, NON_VISION_TOOL_IMAGE_PLACEHOLDER);
            }
            if let Some(id) = id_map.get(&result.tool_call_id) {
                result.tool_call_id = id.clone();
            }
            Message::ToolResult(result)
        }
        Message::Assistant(assistant) => {
            Message::Assistant(transform_assistant(assistant, model, &mut id_map))
        }
        Message::System(_) | Message::User(_) => message.clone(),
    });

    // Second pass: drop failed turns, synthesize results for orphaned tool calls.
    let mut result: Vec<Message> = Vec::with_capacity(transcript.len());
    let mut pending: Vec<ToolCall> = Vec::new();
    let mut answered: HashSet<String> = HashSet::new();
    let mut held: Vec<Message> = Vec::new();
    let close_pending = |result: &mut Vec<Message>,
                         pending: &mut Vec<ToolCall>,
                         answered: &mut HashSet<String>,
                         held: &mut Vec<Message>| {
        for call in pending.drain(..) {
            if !answered.contains(&call.id) {
                result.push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: call.id,
                    tool_name: call.name,
                    content: vec![UserBlock::Text {
                        text: "No result provided".to_owned(),
                    }],
                    details: None,
                    is_error: true,
                    timestamp: now_ms(),
                    duration_ms: None,
                }));
            }
        }
        answered.clear();
        result.append(held);
    };

    for message in transformed {
        match message {
            Message::Assistant(assistant) => {
                close_pending(&mut result, &mut pending, &mut answered, &mut held);
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }
                let calls: Vec<ToolCall> = assistant.tool_calls().cloned().collect();
                if !calls.is_empty() {
                    pending = calls;
                    answered.clear();
                }
                result.push(Message::Assistant(assistant));
            }
            Message::ToolResult(tool_result) => {
                answered.insert(tool_result.tool_call_id.clone());
                result.push(Message::ToolResult(tool_result));
            }
            Message::System(_) if !pending.is_empty() => held.push(message),
            Message::System(_) => result.push(message),
            Message::User(_) => {
                close_pending(&mut result, &mut pending, &mut answered, &mut held);
                result.push(message);
            }
        }
    }
    close_pending(&mut result, &mut pending, &mut answered, &mut held);
    Transcript(result)
}

fn transform_assistant(
    assistant: &AssistantMessage,
    model: &Model,
    id_map: &mut HashMap<String, String>,
) -> AssistantMessage {
    let same_model = assistant.provider == model.provider
        && assistant.api == model.api
        && assistant.model == model.id;
    let content = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantBlock::Thinking {
                thinking,
                replay,
                redacted,
            } => {
                // Signatures and encrypted reasoning are only valid for the model that
                // produced them, and readable thinking sent as text would read as reply.
                if !same_model {
                    return None;
                }
                // Signed or redacted blocks are kept even when empty: they are needed for replay.
                if *redacted || replay.is_some() || !thinking.trim().is_empty() {
                    Some(block.clone())
                } else {
                    None
                }
            }
            AssistantBlock::Text { text, .. } if !same_model => Some(AssistantBlock::Text {
                text: text.clone(),
                text_signature: None,
            }),
            AssistantBlock::ToolCall(call) if !same_model => {
                let id = normalize_tool_call_id(&call.id, model, assistant);
                if id != call.id {
                    id_map.insert(call.id.clone(), id.clone());
                }
                Some(AssistantBlock::ToolCall(ToolCall { id, ..call.clone() }))
            }
            AssistantBlock::Text { .. } | AssistantBlock::ToolCall(_) => Some(block.clone()),
        })
        .collect();
    AssistantMessage {
        content,
        ..assistant.clone()
    }
}

/// Each adapter's tool-call id normalizer (pi `anthropic-messages.ts`
/// `normalizeToolCallId`, `openai-completions.ts` and `openai-responses-shared.ts`).
fn normalize_tool_call_id(id: &str, model: &Model, source: &AssistantMessage) -> String {
    let sanitize = |part: &str| -> String {
        part.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    match model.api {
        // Anthropic requires ^[a-zA-Z0-9_-]+$, at most 64 chars.
        ApiKind::AnthropicMessages => sanitize(id).chars().take(64).collect(),
        // Chat Completions: keep item-level uniqueness of `call_id|item_id` ids within
        // OpenAI's 40-char limit.
        ApiKind::OpenAiCompletions => match id.split_once('|') {
            Some((call_id, item_id)) => {
                let call_id = sanitize(call_id);
                let item_id = sanitize(item_id);
                let combined = if item_id.is_empty() {
                    call_id.clone()
                } else {
                    format!("{call_id}_{item_id}")
                };
                if combined.len() <= 40 {
                    return combined;
                }
                let hash: String = short_hash(id).chars().take(8).collect();
                let prefix: String = call_id.chars().take(1.max(40 - hash.len() - 1)).collect();
                format!("{prefix}_{hash}")
            }
            None if model.provider == "openai" => id.chars().take(40).collect(),
            None => id.to_owned(),
        },
        ApiKind::OpenAiCodexResponses => {
            let normalize_part = |part: &str| -> String {
                let sanitized: String = sanitize(part).chars().take(64).collect();
                sanitized.trim_end_matches('_').to_owned()
            };
            if !CODEX_TOOL_CALL_PROVIDERS.contains(&model.provider.as_str()) {
                return normalize_part(id);
            }
            let Some((call_id, rest)) = id.split_once('|') else {
                return normalize_part(id);
            };
            let item_id = rest.split('|').next().unwrap_or(rest);
            let foreign = source.provider != model.provider || source.api != model.api;
            let mut item: String = if foreign {
                format!("fc_{}", short_hash(item_id))
                    .chars()
                    .take(64)
                    .collect()
            } else {
                normalize_part(item_id)
            };
            // The Responses API requires item ids to start with "fc".
            if !item.starts_with("fc_") {
                item = normalize_part(&format!("fc_{item}"));
            }
            format!("{}|{item}", normalize_part(call_id))
        }
    }
}

/// pi `utils/hash.ts` `shortHash`: a fast deterministic hash over UTF-16 code units,
/// rendered as two base-36 numbers.
pub(crate) fn short_hash(text: &str) -> String {
    let mut h1: u32 = 0xdeadbeef;
    let mut h2: u32 = 0x41c6ce57;
    for unit in text.encode_utf16() {
        let unit = u32::from(unit);
        h1 = (h1 ^ unit).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ unit).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    let base36 = |mut value: u32| {
        let mut digits = Vec::new();
        loop {
            digits.push(char::from_digit(value % 36, 36).unwrap_or('0'));
            value /= 36;
            if value == 0 {
                break;
            }
        }
        digits.into_iter().rev().collect::<String>()
    };
    format!("{}{}", base36(h2), base36(h1))
}
