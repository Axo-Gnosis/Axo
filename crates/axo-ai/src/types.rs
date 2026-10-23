//! The message model, ported from pi `ai/src/types.ts`.
//!
//! Deliberate differences from pi:
//! - thinking blocks carry a typed [`ReasoningReplay`] instead of `thinkingSignature`;
//! - message content is always a `Vec`, and still deserializes from a plain string;
//! - [`StopReason`] has no `pending`/`deferred`;
//! - vendor-only fields (Google thought signatures, Responses namespaces, diagnostics,
//!   deferred handles, nested tool calls) are not ported.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::model::{ApiKind, Model};

/// A JSON object, in source order.
pub type JsonMap = Map<String, Value>;

/// Milliseconds since the Unix epoch, pi's timestamp convention.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

#[allow(clippy::large_enum_variant)] // Keep public variants inline; boxing adds an allocation per assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

/// System instructions and tool declarations at one point in the transcript.
///
/// The leading system message is the system prompt. Later system messages change it:
/// `content` adds instructions from that point on, `sections` replace or remove named
/// prompt sections (`None` removes one), and `tools_added`/`tools_removed` change the
/// tool set. Replaying every system message in order yields the current prompt and tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    #[serde(deserialize_with = "content_vec")]
    pub content: Vec<TextBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<IndexMap<String, Option<String>>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_added: Vec<ToolSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_removed: Vec<String>,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    #[serde(deserialize_with = "content_vec")]
    pub content: Vec<UserBlock>,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderFailure {
    pub status: Option<u16>,
    pub code: Option<String>,
    pub retry_after_ms: Option<u64>,
    pub should_retry: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    #[serde(deserialize_with = "content_vec")]
    pub content: Vec<AssistantBlock>,
    pub api: ApiKind,
    pub provider: String,
    pub model: String,
    /// Concrete model reported by the provider when different from the requested `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    /// Provider-specific response/message identifier, when the upstream API exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Exact provider-native effort level used for this response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_thinking_level: Option<String>,
    /// Thinking level the agent loop requested for this response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ProviderFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    /// Provider indication of whether the model explicitly ended its turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    /// Milliseconds since the epoch when the request started.
    pub timestamp: i64,
    /// Milliseconds from `timestamp` until the response ended (monotonic clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl AssistantMessage {
    /// The empty partial message an adapter starts a response with: no content, zero
    /// usage, `StopReason::Stop`, timestamped now.
    pub fn empty(model: &Model) -> Self {
        Self {
            content: Vec::new(),
            api: model.api,
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            failure: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_ms(),
            duration_ms: None,
        }
    }

    /// The tool calls of this message, in content order.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|block| match block {
            AssistantBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(deserialize_with = "content_vec")]
    pub content: Vec<UserBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub is_error: bool,
    pub timestamp: i64,
    /// Milliseconds the tool's execution took (monotonic clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// A text block of a system message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "text")]
pub struct TextBlock {
    pub text: String,
}

/// Content of user messages and tool results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UserBlock {
    Text {
        text: String,
    },
    Image {
        /// Base64-encoded image data, shared by every copy of the transcript.
        data: Arc<str>,
        /// e.g. `image/png`.
        mime_type: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AssistantBlock {
    Text {
        text: String,
        /// Provider message metadata, e.g. an OpenAI Responses `TextSignatureV1` JSON string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    Thinking {
        thinking: String,
        /// Opaque data the producing API needs back on the next request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay: Option<ReasoningReplay>,
        /// The thinking was redacted by safety filters; the encrypted payload is in `replay`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: JsonMap,
    /// Set when the model's complete arguments text was not a valid JSON object; `arguments`
    /// is then `{}`. Tool argument validation fails with this text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_error: Option<String>,
}

/// Opaque provider data needed to replay reasoning, tagged with its kind so data from
/// one API can never be mistaken for another's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum ReasoningReplay {
    /// Anthropic `signature` of a thinking block.
    AnthropicSignature(String),
    /// Anthropic `redacted_thinking` data.
    AnthropicRedacted(String),
    /// The whole OpenAI Responses reasoning item.
    OpenAiReasoningItem(Value),
    /// The delta field name an OpenAI-compatible server used for reasoning
    /// (`reasoning_content`, `reasoning` or `reasoning_text`).
    ReasoningField(String),
    /// OpenRouter-style `reasoning_details`.
    ReasoningDetails(Value),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Subset of `cache_write` written with 1h retention. Only Anthropic reports this split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u64>,
    /// Reasoning tokens, a subset of `output`, when the provider reports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: UsageCost,
}

/// Cost of one response in USD.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingLevel {
    /// Every level, lowest first.
    pub const ALL: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Max,
    ];

    /// The serde name (`off`, `minimal`, …, `xhigh`, `max`).
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::XHigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }
}

impl std::str::FromStr for ThinkingLevel {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        ThinkingLevel::ALL
            .into_iter()
            .find(|level| level.as_str() == text)
            .ok_or_else(|| format!("unknown thinking level: {text}"))
    }
}

/// A tool declaration as the model sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
}

/// Block types that a plain-string `content` deserializes into.
trait FromText {
    fn from_text(text: String) -> Self;
}

impl FromText for TextBlock {
    fn from_text(text: String) -> Self {
        TextBlock { text }
    }
}

impl FromText for UserBlock {
    fn from_text(text: String) -> Self {
        UserBlock::Text { text }
    }
}

impl FromText for AssistantBlock {
    fn from_text(text: String) -> Self {
        AssistantBlock::Text {
            text,
            text_signature: None,
        }
    }
}

/// Accept `content` as a block array, a plain string (one text block, none when empty)
/// or null (no blocks).
fn content_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + FromText,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Content<T> {
        Text(String),
        Blocks(Vec<T>),
    }
    Ok(match Option::<Content<T>>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(Content::Text(text)) if text.is_empty() => Vec::new(),
        Some(Content::Text(text)) => vec![T::from_text(text)],
        Some(Content::Blocks(blocks)) => blocks,
    })
}
