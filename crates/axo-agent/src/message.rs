//! Messages the agent keeps: LLM messages plus host-defined custom messages
//! (pi `coding-agent/src/core/messages.ts:30-78`, the `custom` role).

use axo_ai::{Message, UserBlock};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A transcript entry. `Custom` messages never reach a provider directly:
/// [`LoopHooks::convert_to_llm`](crate::LoopHooks::convert_to_llm) lowers or drops them.
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
