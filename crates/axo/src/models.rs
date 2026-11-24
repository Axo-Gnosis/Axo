use std::path::Path;
use std::sync::Arc;

use axo_ai::estimate::{estimate_blocks_tokens, estimate_message_tokens, estimate_text_tokens};
use axo_ai::{
    AssistantEvent, AssistantMessage, CredentialSource, Message, Model, Models, ProviderInfo,
    ServiceTier, SimpleStreamOptions, Streamer, ThinkingLevel, ToolChoice, Transcript, UserBlock,
};
use axo_macros::expose;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::config::ConfigSource;
use crate::ext::Caller;
use crate::{Axo, CompletionDetails, EffectiveConfig, Error, ProjectAccess};

/// A single model request, independent of any session.
#[derive(Debug, Clone, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletionRequest {
    /// A model selector as `/model` takes it: `provider/id` or `@role`, optionally with
    /// `:thinking` and `:tier` suffixes in either order.
    pub model: String,
    pub messages: Vec<Message>,
    /// Overrides the selector's thinking suffix; `off` when neither names a level.
    #[serde(default)]
    pub thinking: Option<ThinkingLevel>,
    /// Overrides the selector's tier suffix; standard when neither names one.
    #[serde(default)]
    pub service_tier: Option<ServiceTier>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
}

#[expose(block = AXO_MODELS, object = "host")]
impl Axo {
    /// Streams one request through the host's configured streamer. Cancel the token to stop it.
    #[ext(skip = "live stream; served as models.stream")]
    pub fn stream(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, AssistantEvent>, Error> {
        self.stream_with(self.models(), request, cancel)
    }

    /// Streams one request resolved against `models` (for example a scoped `Models` view).
    /// Uses `AxoBuilder::streamer` if set, otherwise streams through `models`.
    #[ext(skip = "live stream; served as models.stream")]
    pub fn stream_with(
        &self,
        models: &Arc<Models>,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, AssistantEvent>, Error> {
        stream_request(
            &*self.config,
            models,
            self.custom_streamer.as_ref(),
            request,
            cancel,
        )
    }

    /// Collects one request's events into the final assistant message.
    #[ext(skip = "served as models.complete")]
    pub async fn complete(&self, request: CompletionRequest) -> Result<AssistantMessage, Error> {
        self.complete_with(self.models(), request).await
    }

    /// Collects one request's events resolved against `models` into the final assistant message.
    #[ext(skip = "served as models.complete")]
    pub async fn complete_with(
        &self,
        models: &Arc<Models>,
        request: CompletionRequest,
    ) -> Result<AssistantMessage, Error> {
        complete(
            &*self.config,
            models,
            self.custom_streamer.as_ref(),
            request,
        )
        .await
    }
}

pub(crate) fn streamer_for(
    models: &Arc<Models>,
    custom_streamer: Option<&Arc<dyn Streamer>>,
) -> Arc<dyn Streamer> {
    custom_streamer.cloned().unwrap_or_else(|| models.clone())
}

fn stream_request(
    config: &dyn ConfigSource,
    models: &Arc<Models>,
    custom_streamer: Option<&Arc<dyn Streamer>>,
    request: CompletionRequest,
    cancel: CancellationToken,
) -> Result<BoxStream<'static, AssistantEvent>, Error> {
    let streamer = streamer_for(models, custom_streamer);
    // `modelRoles` is user-only, so the project layer (and with it the cwd) never matters.
    let settings =
        EffectiveConfig::resolve(config, Path::new(""), Some(ProjectAccess::Ignore))?.settings;
    let choice = settings.resolve_model(models, &request.model)?;
    let model = choice.model.ok_or_else(|| Error::InvalidModelSelector {
        selector: request.model.clone(),
        message: "a completion needs a model, not only a thinking level".into(),
    })?;
    Ok(streamer.stream(
        &model,
        Transcript::new(request.messages),
        SimpleStreamOptions {
            thinking: request.thinking.or(choice.thinking).unwrap_or_default(),
            service_tier: request.service_tier.or(choice.tier).unwrap_or_default(),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            tool_choice: request.tool_choice,
            ..Default::default()
        },
        cancel,
    ))
}

async fn complete(
    config: &dyn ConfigSource,
    models: &Arc<Models>,
    custom_streamer: Option<&Arc<dyn Streamer>>,
    request: CompletionRequest,
) -> Result<AssistantMessage, Error> {
    let cancel = CancellationToken::new();
    let stream = stream_request(config, models, custom_streamer, request, cancel.clone())?;
    collect_completion(stream, cancel).await
}

async fn collect_completion(
    mut stream: BoxStream<'static, AssistantEvent>,
    cancel: CancellationToken,
) -> Result<AssistantMessage, Error> {
    let _guard = cancel.drop_guard();
    while let Some(event) = stream.next().await {
        match event {
            AssistantEvent::Done { message, .. } => return Ok(message),
            AssistantEvent::Error { message, .. } => {
                return Err(Error::Completion {
                    message: message
                        .error_message
                        .unwrap_or_else(|| "completion failed".into()),
                    details: Some(Box::new(CompletionDetails {
                        failure: message.failure,
                        usage: message.usage,
                        stop_reason: message.stop_reason,
                    })),
                });
            }
            _ => {}
        }
    }
    Err(Error::Completion {
        message: "the stream ended without a final message".into(),
        details: None,
    })
}

pub(crate) struct ModelsTarget {
    pub(crate) models: Arc<Models>,
    pub(crate) config: Arc<dyn ConfigSource>,
    pub(crate) custom_streamer: Option<Arc<dyn Streamer>>,
}

struct ModelsWire<'a> {
    target: &'a ModelsTarget,
}

impl<'a> ModelsWire<'a> {
    fn wire(target: &'a ModelsTarget, _: &'a Caller) -> Self {
        Self { target }
    }
}

#[expose(block = MODELS_WIRE, object = "models", target = ModelsTarget)]
impl ModelsWire<'_> {
    /// Looks up one model by its provider and id.
    #[ext]
    pub fn find(&self, selector: String) -> Option<Model> {
        self.target.models.find(&selector)
    }

    /// Lists every model in the host registry.
    #[ext]
    pub fn all(&self) -> Vec<Arc<Model>> {
        // Handle clones only; the dispatch macro types `Arc<T>` as `T`, which rejects `Arc<[_]>`.
        self.target.models.all().to_vec()
    }

    /// Lists provider metadata without credential values.
    #[ext]
    pub fn providers(&self) -> Vec<ProviderInfo> {
        self.target.models.providers()
    }

    /// Identifies the source of a provider's credential without resolving it.
    #[ext]
    pub async fn credential_source(&self, provider: String) -> Result<CredentialSource, Error> {
        Ok(self.target.models.credential_source(&provider).await?)
    }

    /// Estimates the token count of plain text.
    #[ext]
    pub fn estimate_text(&self, text: String) -> u64 {
        estimate_text_tokens(&text)
    }

    /// Estimates the token count of user content blocks.
    #[ext]
    pub fn estimate_blocks(&self, content: Vec<UserBlock>) -> u64 {
        estimate_blocks_tokens(&content)
    }

    /// Estimates the token count of a message.
    #[ext]
    pub fn estimate_message(&self, message: Message) -> u64 {
        estimate_message_tokens(&message)
    }

    /// Sends a one-off request and returns the final assistant message.
    #[ext]
    pub async fn complete(&self, request: CompletionRequest) -> Result<AssistantMessage, Error> {
        complete(
            &*self.target.config,
            &self.target.models,
            self.target.custom_streamer.as_ref(),
            request,
        )
        .await
    }
}
