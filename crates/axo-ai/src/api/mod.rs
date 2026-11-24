//! Provider APIs: the [`Api`] trait adapters implement and the [`Streamer`] trait the
//! agent loop calls.

pub mod anthropic;
pub mod codex;
pub mod completions;
pub mod http;
pub(crate) mod responses;
pub(crate) mod sse;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use futures::channel::mpsc::{self, UnboundedSender};
use futures::stream::{self, BoxStream};
use futures::{StreamExt, future};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tokio_util::sync::CancellationToken;

use crate::auth::ModelAuth;
use crate::error::Error;
use crate::event::{AssistantEvent, parse_tool_args};
use crate::model::Model;
use crate::transcript::Transcript;
use crate::types::{AssistantMessage, ServiceTier, StopReason, ThinkingLevel};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
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
    /// The chosen tier; adapters send `Model::effective_tier` of it.
    pub service_tier: ServiceTier,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub tool_choice: Option<ToolChoice>,
    /// Stable id for prompt-cache keys and session-affinity headers.
    pub session_id: Option<String>,
    /// Extra request headers; `None` deletes a default header.
    pub headers: BTreeMap<String, Option<String>>,
}

/// The tier a provider's echoed tier or speed word means.
pub(crate) fn served_tier(echo: &str) -> ServiceTier {
    match echo {
        "priority" | "fast" => ServiceTier::Fast,
        "flex" => ServiceTier::Flex,
        "ultrafast" => ServiceTier::Ultrafast,
        _ => ServiceTier::Standard,
    }
}

#[derive(Clone, Default)]
pub struct StreamOptions {
    pub simple: SimpleStreamOptions,
    pub auth: ModelAuth,
    /// The provider's level-2 request rewrite, run once per request before sending.
    pub rewrite: Option<Arc<dyn RequestRewrite>>,
    /// The provider's transport, sending each attempt instead of axo's HTTP client.
    pub transport: Option<Arc<dyn ProviderTransport>>,
}

impl std::fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamOptions")
            .field("simple", &self.simple)
            .field("auth", &self.auth)
            .field("rewrite", &self.rewrite.is_some())
            .field("transport", &self.transport.is_some())
            .finish()
    }
}

impl StreamOptions {
    pub(crate) fn configured_headers(&self) -> BTreeMap<String, Option<String>> {
        let mut headers: BTreeMap<_, _> = self
            .auth
            .headers
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), Some(value.clone())))
            .collect();
        headers.extend(
            self.simple
                .headers
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value.clone())),
        );
        headers
    }

    pub(crate) fn base_url<'a>(&'a self, model: &'a Model) -> &'a str {
        self.auth
            .base_url
            .as_deref()
            .unwrap_or(&model.base_url)
            .trim_end_matches('/')
    }
}

/// A provider request as the standard adapter built it, before it is sent.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub url: String,
    pub headers: HeaderMap,
    pub body: serde_json::Value,
}

/// The level-2 hook: rewrite the URL, headers and body of a provider request. axo still
/// sends it and parses the response.
#[async_trait::async_trait]
pub trait RequestRewrite: Send + Sync {
    async fn rewrite(
        &self,
        model: &Model,
        request: ProviderRequest,
    ) -> Result<ProviderRequest, Error>;
}

/// What axo knows about a request beyond the request itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestContext {
    /// `SimpleStreamOptions::session_id`.
    pub session_id: Option<String>,
    /// 0 for the first attempt; axo's transport retry increments it.
    pub attempt: u32,
}

/// One HTTP response as a transport returns it. axo treats it exactly as its own client's:
/// a retryable status is retried, another error status becomes the failure, a success
/// body is decoded as the model's wire.
pub struct ProviderResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: BoxStream<'static, Result<bytes::Bytes, String>>,
}

impl From<reqwest::Response> for ProviderResponse {
    fn from(response: reqwest::Response) -> Self {
        Self {
            status: response.status().as_u16(),
            headers: response.headers().clone(),
            body: response
                .bytes_stream()
                .map(|chunk| chunk.map_err(|error| error.to_string()))
                .boxed(),
        }
    }
}

impl std::fmt::Debug for ProviderResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

/// Owns the HTTP exchange for a provider's requests: axo builds the request in the model's
/// wire and decodes the response; the transport sends one attempt and returns the response.
/// It may change the request, call other endpoints first, or synthesize the response.
/// Dropping the returned future or body cancels the attempt.
#[async_trait::async_trait]
pub trait ProviderTransport: Send + Sync {
    async fn send(
        &self,
        model: &Model,
        request: &ProviderRequest,
        context: &RequestContext,
    ) -> Result<ProviderResponse, Error>;
}

pub(crate) fn header_map(
    layers: impl IntoIterator<Item = impl IntoIterator<Item = (String, Option<String>)>>,
) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for layer in layers {
        for (name, value) in layer {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|error| error.to_string())?;
            if let Some(value) = value {
                let value = HeaderValue::from_str(&value).map_err(|error| error.to_string())?;
                headers.insert(name, value);
            } else {
                headers.remove(name);
            }
        }
    }
    Ok(headers)
}

pub(crate) struct Reply {
    tx: UnboundedSender<AssistantEvent>,
    pub message: AssistantMessage,
    tool_args: HashMap<usize, String>,
}

impl Reply {
    pub(crate) fn emit(&self, event: AssistantEvent) {
        let _ = self.tx.unbounded_send(event);
    }

    pub(crate) fn start(&self) {
        self.emit(AssistantEvent::Start {
            partial: self.message.clone(),
        });
    }

    pub(crate) fn end_block(&self, index: usize) {
        self.emit(AssistantEvent::end_block(
            index,
            self.message.content[index].clone(),
        ));
    }

    pub(crate) fn push_tool_args(&mut self, index: usize, delta: &str) {
        self.tool_args.entry(index).or_default().push_str(delta);
        self.emit(AssistantEvent::ToolCallDelta {
            content_index: index,
            delta: delta.to_owned(),
        });
    }

    pub(crate) fn take_tool_args(&mut self, index: usize) -> String {
        self.tool_args.remove(&index).unwrap_or_default()
    }

    pub(crate) fn set_tool_args(&mut self, index: usize, arguments: String) {
        self.tool_args.insert(index, arguments);
    }
}

/// The returned stream owns the request future, so dropping it cancels the request.
pub(crate) fn respond<F>(
    model: &Model,
    tier: ServiceTier,
    cancel: CancellationToken,
    run: impl FnOnce(Reply, CancellationToken) -> F + Send + 'static,
) -> BoxStream<'static, AssistantEvent>
where
    F: Future<Output = (Reply, Result<StopReason, String>)> + Send + 'static,
{
    let (tx, rx) = mpsc::unbounded();
    let reply = Reply {
        tx,
        message: AssistantMessage {
            service_tier: tier,
            ..AssistantMessage::empty(model)
        },
        tool_args: HashMap::new(),
    };
    let producer = stream::once(async move {
        let (mut reply, result) = run(reply, cancel.clone()).await;
        let event = match result {
            Ok(reason)
                if !cancel.is_cancelled()
                    && !matches!(reason, StopReason::Error | StopReason::Aborted) =>
            {
                reply.message.stop_reason = reason;
                AssistantEvent::Done {
                    reason,
                    message: reply.message,
                }
            }
            result => {
                let reason = if cancel.is_cancelled() {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                parse_tool_args(&mut reply.message, &reply.tool_args);
                reply.message.stop_reason = reason;
                reply.message.error_message = Some(
                    result
                        .err()
                        .filter(|message| !message.is_empty())
                        .or(reply
                            .message
                            .error_message
                            .filter(|message| !message.is_empty()))
                        .unwrap_or_else(|| "An unknown error occurred".to_owned()),
                );
                AssistantEvent::Error {
                    reason,
                    message: reply.message,
                }
            }
        };
        let _ = reply.tx.unbounded_send(event);
    })
    .filter_map(|()| future::ready(None));
    stream::select(rx, producer).boxed()
}
