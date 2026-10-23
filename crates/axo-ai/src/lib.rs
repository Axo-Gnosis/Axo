//! axo's LLM layer: the message model, streaming provider adapters, the model catalog
//! and auth. A port of pi's `@earendil-works/pi-ai` (v1.3.0).

pub mod api;
pub mod auth;
pub mod cost;
pub mod error;
pub mod estimate;
pub mod event;
pub mod json;
pub mod model;
pub mod models;
pub mod overflow;
pub mod retry;
pub mod thinking;
pub mod transcript;
pub mod transform;
pub mod types;

#[cfg(feature = "testing")]
pub mod testing;

pub use api::anthropic::AnthropicApi;
pub use api::codex::CodexApi;
pub use api::completions::CompletionsApi;
pub use api::{Api, SimpleStreamOptions, StreamOptions, Streamer, ToolChoice};
pub use auth::resolve::resolve_config_value;
pub use auth::store::FileCredentialStore;
pub use auth::{Credential, CredentialStore, ModelAuth};
pub use cost::calculate_cost;
pub use error::Error;
pub use event::{Accumulator, AssistantEvent};
pub use model::{ApiKind, Compat, Cost, CostTier, InputKind, Model, ThinkingLevelMap};
pub use models::config::ModelsConfig;
pub use models::{Models, ModelsOptions};
pub use overflow::{is_context_overflow, is_recoverable_length};
pub use retry::{is_fatal_error, is_retryable_error, retry_delay};
#[cfg(feature = "testing")]
pub use testing::{RecordedRequest, ScriptedApi, ScriptedTurn};
pub use transcript::{Context, Transcript};
pub use transform::{drop_failed_turns, transform_messages};
pub use types::{
    AssistantBlock, AssistantMessage, JsonMap, Message, ProviderFailure, ReasoningReplay,
    StopReason, SystemMessage, TextBlock, ThinkingLevel, ToolCall, ToolResultMessage, ToolSpec,
    Usage, UsageCost, UserBlock, UserMessage, now_ms,
};
