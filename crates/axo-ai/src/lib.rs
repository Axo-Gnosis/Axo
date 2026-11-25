//! axo's LLM layer: the message model, streaming provider adapters, the model catalog
//! and auth. A port of pi's `@earendil-works/pi-ai` (v1.3.0).

pub mod api;
pub mod auth;
mod cost;
mod error;
pub mod estimate;
mod event;
pub(crate) mod json;
pub mod model;
pub mod models;
mod overflow;
mod retry;
pub mod thinking;
pub mod transcript;
mod transform;
mod types;

#[cfg(feature = "testing")]
pub mod testing;

pub use api::anthropic::AnthropicApi;
pub use api::completions::CompletionsApi;
pub use api::google::GoogleApi;
pub use api::openai_responses::ResponsesApi;
pub use api::{
    Api, ProviderRequest, ProviderResponse, ProviderTransport, RequestContext, RequestRewrite,
    ResponseObserver, SimpleStreamOptions, StreamOptions, Streamer, ToolChoice,
};
pub use auth::provider::{
    AccountUsage, DeviceCode, LoginInteraction, LoginOption, LoginPrompt, LoginSelect,
    ModelsRequest, Provider, ProviderLogin, ProviderModels, ProviderUsage, UsageLimit, UsageSpend,
};
pub use auth::resolve::resolve_config_value;
pub use auth::store::FileCredentialStore;
pub use auth::{Credential, CredentialSource, CredentialStore, ModelAuth};
pub use error::Error;
pub use event::{Accumulator, AssistantEvent};
pub use model::{ApiKind, Compat, Cost, CostTier, InputKind, Model, ThinkingLevelMap};
pub use models::config::ModelsConfig;
pub use models::{Models, ModelsOptions, ProviderInfo, ProviderLayer, ProviderOrigin};
pub use overflow::{is_context_overflow, is_recoverable_length};
pub use retry::{is_fatal_error, is_retryable_error};
#[cfg(feature = "testing")]
pub use testing::{RecordedRequest, ScriptedApi, ScriptedTurn};
pub use transcript::{Transcript, TranscriptToolSupport};
pub use transform::transform_messages;
pub use types::{
    AssistantBlock, AssistantMessage, JsonMap, Message, ProviderFailure, ReasoningReplay,
    ServiceTier, StopReason, SystemMessage, TextBlock, ThinkingLevel, ToolCall, ToolResultMessage,
    ToolSpec, Usage, UsageCost, UserBlock, UserMessage, now_ms,
};
