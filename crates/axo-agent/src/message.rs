//! Messages the agent keeps: LLM messages plus host-defined custom messages
//! (pi `coding-agent/src/core/messages.ts:30-78`, the `custom` role).

use axo_ai::{AssistantMessage, Message, StopReason, ToolCall, ToolResultMessage, UserBlock};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A transcript entry. `Custom` messages never reach a provider directly:
/// [`LoopHooks::convert_to_llm`](crate::LoopHooks::convert_to_llm) lowers or drops them.
#[allow(clippy::large_enum_variant)] // Keep transcript messages inline; boxing adds an allocation per LLM message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMessage {
    Llm(Message),
    Custom(CustomMessage),
}

/// A host message (`role: "custom"`), e.g. an extension notice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename = "custom", rename_all = "camelCase")]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: Vec<UserBlock>,
    /// Whether a UI shows it.
    pub display: bool,
    /// Host-only data; never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub timestamp: i64,
}

/// A resumable assistant message, its unanswered calls and its trailing results.
pub struct Unanswered<'a> {
    pub message: &'a AssistantMessage,
    pub calls: Vec<&'a ToolCall>,
    /// Trailing results answering `message`, in transcript order.
    pub results: Vec<&'a ToolResultMessage>,
}

/// The last assistant message's calls that no later tool result answers, when only tool
/// results (or system messages) follow it. Failed and aborted responses are never
/// resumed: continuing after them requests a new turn.
pub fn unanswered_calls<'a>(
    messages: impl DoubleEndedIterator<Item = &'a AgentMessage>,
) -> Option<Unanswered<'a>> {
    let mut results = Vec::new();
    for message in messages.rev() {
        match message {
            AgentMessage::Llm(Message::ToolResult(result)) => results.push(result),
            AgentMessage::Llm(Message::System(_)) => {}
            AgentMessage::Llm(Message::Assistant(assistant))
                if !matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) =>
            {
                let calls: Vec<&ToolCall> = assistant
                    .tool_calls()
                    .filter(|call| !results.iter().any(|result| result.tool_call_id == call.id))
                    .collect();
                results.reverse();
                return (!calls.is_empty()).then_some(Unanswered {
                    message: assistant,
                    calls,
                    results,
                });
            }
            _ => return None,
        }
    }
    None
}
