use std::path::Path;

use axo_ai::estimate::{estimate_blocks_tokens, estimate_message_tokens, estimate_text_tokens};
use axo_ai::{
    AssistantEvent, AssistantMessage, CredentialSource, Message, Model, ProviderInfo, ServiceTier,
    SimpleStreamOptions, ThinkingLevel, ToolChoice, Transcript, UserBlock,
};
use axo_macros::expose;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::ext::Caller;
use crate::{Axo, EffectiveConfig, Error, ProjectAccess};

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
        // `modelRoles` is user-only, so the project layer (and with it the cwd) never matters.
        let settings =
            EffectiveConfig::resolve(&*self.config, Path::new(""), Some(ProjectAccess::Ignore))?
                .settings;
        let choice = settings.resolve_model(self.models(), &request.model)?;
        let model = choice.model.ok_or_else(|| Error::InvalidModelSelector {
            selector: request.model.clone(),
            message: "a completion needs a model, not only a thinking level".into(),
        })?;
        Ok(self.streamer.stream(
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

    /// Collects one request's events into the final assistant message.
    #[ext(skip = "served as models.complete")]
    pub async fn complete(&self, request: CompletionRequest) -> Result<AssistantMessage, Error> {
        let cancel = CancellationToken::new();
        let mut stream = self.stream(request, cancel.clone())?;
        let _guard = cancel.drop_guard();
        while let Some(event) = stream.next().await {
            match event {
                AssistantEvent::Done { message, .. } => return Ok(message),
                AssistantEvent::Error { message, .. } => {
                    return Err(Error::Completion(
                        message
                            .error_message
                            .unwrap_or_else(|| "completion failed".into()),
                    ));
                }
                _ => {}
            }
        }
        Err(Error::Completion(
            "the stream ended without a final message".into(),
        ))
    }
}

struct ModelsWire<'a> {
    axo: &'a Axo,
}

impl<'a> ModelsWire<'a> {
    fn wire(axo: &'a Axo, _: &'a Caller) -> Self {
        Self { axo }
    }
}

#[expose(block = MODELS_WIRE, object = "models", target = Axo)]
impl ModelsWire<'_> {
    /// Looks up one model by its provider and id.
    #[ext]
    pub fn find(&self, selector: String) -> Option<Model> {
        self.axo.models().find(&selector)
    }

    /// Lists every model in the host registry.
    #[ext]
    pub fn all(&self) -> Vec<Model> {
        self.axo.models().all().to_vec()
    }

    /// Lists provider metadata without credential values.
    #[ext]
    pub fn providers(&self) -> Vec<ProviderInfo> {
        self.axo.models().providers()
    }

    /// Identifies the source of a provider's credential without resolving it.
    #[ext]
    pub async fn credential_source(&self, provider: String) -> Result<CredentialSource, Error> {
        Ok(self.axo.models().credential_source(&provider).await?)
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
        self.axo.complete(request).await
    }
}
