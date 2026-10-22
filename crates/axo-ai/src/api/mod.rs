//! Provider APIs: the [`Api`] trait adapters implement and the [`Streamer`] trait the
//! agent loop calls.

pub mod anthropic;
pub mod codex;
pub mod completions;
pub mod http;
pub mod responses;
pub mod sse;

use std::collections::BTreeMap;

use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::auth::ModelAuth;
use crate::event::AssistantEvent;
use crate::model::Model;
use crate::transcript::Transcript;
use crate::types::ThinkingLevel;

/// The error message of a request the caller cancelled.
pub(crate) const ABORTED: &str = "Request was aborted";

/// One wire API. Receives a transcript already passed through `transform_messages`
/// and resolved auth. Never fails by returning `Err`: request, model and runtime failures
/// end the stream with an `AssistantEvent::Error`.
pub trait Api: Send + Sync {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: StreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent>;
}

/// What the agent loop calls: resolves auth, transforms the transcript and dispatches to
/// the model's [`Api`]. Auth failures end the stream with a single `Error` event and no
/// `Start`.
pub trait Streamer: Send + Sync {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: SimpleStreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
}

impl ToolChoice {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
        }
    }
}

/// Request options shared by every API.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SimpleStreamOptions {
    pub thinking: ThinkingLevel,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f32>,
    pub tool_choice: Option<ToolChoice>,
    /// Stable id for prompt-cache keys and session-affinity headers.
    pub session_id: Option<String>,
    /// Overrides every other key source.
    pub api_key: Option<String>,
    /// Extra request headers; `None` deletes a default header.
    pub headers: BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamOptions {
    pub simple: SimpleStreamOptions,
    pub auth: ModelAuth,
}
