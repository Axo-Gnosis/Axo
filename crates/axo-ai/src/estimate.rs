//! Context-token estimates. Port of pi `utils/estimate.ts`.

use crate::transcript::system_message_text;
use crate::types::{AssistantBlock, Message, StopReason, Usage, UserBlock};

const ESTIMATED_IMAGE_CHARS: usize = 4800;
/// Tokens kept free below the context window when clamping a response ceiling.
pub(crate) const CONTEXT_SAFETY_TOKENS: i64 = 4096;

fn units_to_tokens(units: usize) -> u64 {
    (units as u64 * 2).div_ceil(7)
}

/// Estimate tokens from UTF-16 code units, at 3.5 units per token.
pub fn estimate_text_tokens(text: &str) -> u64 {
    units_to_tokens(text.encode_utf16().count())
}

/// Estimate user content tokens, counting each image as 4800 UTF-16 code units.
pub fn estimate_blocks_tokens(content: &[UserBlock]) -> u64 {
    units_to_tokens(
        content
            .iter()
            .map(|block| match block {
                UserBlock::Text { text } => text.encode_utf16().count(),
                UserBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
    )
}

/// pi `estimateContextTokens`: the usage of the last assistant response that still
/// describes the prefix, plus estimates for the messages after it.
pub fn estimate_context_tokens(messages: &[Message]) -> u64 {
    let context_tokens = |usage: &Usage| match usage.total_tokens {
        0 => usage.input + usage.output + usage.cache_read + usage.cache_write,
        total => total,
    };
    let mut latest_prefix_timestamp = i64::MIN;
    let mut last_usage: Option<(usize, u64)> = None;
    for (index, message) in messages.iter().enumerate() {
        let timestamp = match message {
            Message::System(system) => system.timestamp,
            Message::User(user) => user.timestamp,
            Message::ToolResult(result) => result.timestamp,
            Message::Assistant(assistant) => {
                // A newer prefix message (e.g. a compaction summary) invalidates older usage.
                let tokens = context_tokens(&assistant.usage);
                if assistant.timestamp >= latest_prefix_timestamp
                    && !matches!(
                        assistant.stop_reason,
                        StopReason::Aborted | StopReason::Error
                    )
                    && tokens > 0
                {
                    last_usage = Some((index, tokens));
                }
                assistant.timestamp
            }
        };
        latest_prefix_timestamp = latest_prefix_timestamp.max(timestamp);
    }
    let (start, usage_tokens) = last_usage.map_or((0, 0), |(index, tokens)| (index + 1, tokens));
    usage_tokens
        + messages[start..]
            .iter()
            .map(estimate_message_tokens)
            .sum::<u64>()
}

/// pi `estimateMessageTokens`: 3.5 UTF-16 code units per token, images as 4800 units.
pub fn estimate_message_tokens(message: &Message) -> u64 {
    let utf16 = |text: &str| text.encode_utf16().count();
    let tokens = units_to_tokens;
    match message {
        Message::System(system) => {
            let tools = |json: Option<String>| json.map_or(0, |json| estimate_text_tokens(&json));
            estimate_text_tokens(&system_message_text(system))
                + tools(
                    (!system.tools_added.is_empty())
                        .then(|| serde_json::to_string(&system.tools_added).unwrap_or_default()),
                )
                + tools(
                    (!system.tools_removed.is_empty())
                        .then(|| serde_json::to_string(&system.tools_removed).unwrap_or_default()),
                )
        }
        Message::User(user) => estimate_blocks_tokens(&user.content),
        Message::ToolResult(result) => estimate_blocks_tokens(&result.content),
        Message::Assistant(assistant) => tokens(
            assistant
                .content
                .iter()
                .map(|block| match block {
                    AssistantBlock::Text { text, .. } => utf16(text),
                    AssistantBlock::Thinking { thinking, .. } => utf16(thinking),
                    AssistantBlock::ToolCall(call) => {
                        utf16(&call.name)
                            + utf16(&serde_json::to_string(&call.arguments).unwrap_or_default())
                    }
                })
                .sum(),
        ),
    }
}
