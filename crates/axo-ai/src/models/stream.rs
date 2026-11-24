use std::sync::Arc;

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tokio_util::sync::CancellationToken;

use crate::api::{ABORTED, Api, SimpleStreamOptions, StreamOptions, Streamer};
use crate::auth::resolve::resolve_auth;
use crate::error::Error;
use crate::event::{AssistantEvent, timed};
use crate::model::{ApiKind, Model};
use crate::transcript::Transcript;
use crate::transform::transform_messages;
use crate::types::{AssistantMessage, ProviderFailure, StopReason};

use super::Models;

impl Streamer for Models {
    fn stream(
        &self,
        model: &Model,
        transcript: Transcript,
        opts: SimpleStreamOptions,
        cancel: CancellationToken,
    ) -> BoxStream<'static, AssistantEvent> {
        let request = match self.request(model) {
            Ok(request) => request,
            Err(error) => return setup_failed(model, &error),
        };
        let transcript = transform_messages(transcript, &request.model);
        let api: Arc<dyn Api> = match request.model.api {
            ApiKind::AnthropicMessages if request.anthropic_subscription => {
                self.anthropic_subscription.clone()
            }
            ApiKind::AnthropicMessages => self.anthropic.clone(),
            ApiKind::OpenAiCompletions => self.completions.clone(),
            ApiKind::OpenAiCodexResponses => self.codex.clone(),
        };
        let auth = resolve_auth(
            request.auth,
            self.view.shared.credentials.clone(),
            request.headers,
            request.config_base_url,
        );
        stream::once(async move {
            // A refresh in progress keeps running in its own spawned task.
            let auth = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                auth = auth => Some(auth),
            };
            match auth {
                Some(Ok(auth)) => timed(api.stream(
                    &request.model,
                    transcript,
                    StreamOptions {
                        simple: opts,
                        auth,
                        rewrite: request.rewrite,
                    },
                    cancel,
                )),
                Some(Err(error)) => setup_failed(&request.model, &error),
                None => failed(
                    &request.model,
                    StopReason::Aborted,
                    ABORTED.to_owned(),
                    None,
                ),
            }
        })
        .flatten()
        .boxed()
    }
}

/// A setup error as the terminal event, with `Error::provider_failure` so an unavailable
/// provider is never retried.
fn setup_failed(model: &Model, error: &Error) -> BoxStream<'static, AssistantEvent> {
    failed(
        model,
        StopReason::Error,
        error.to_string(),
        error.provider_failure(),
    )
}

fn failed(
    model: &Model,
    reason: StopReason,
    error: String,
    failure: Option<ProviderFailure>,
) -> BoxStream<'static, AssistantEvent> {
    let mut message = AssistantMessage::empty(model);
    message.stop_reason = reason;
    message.error_message = Some(error);
    message.failure = failure;
    stream::iter([AssistantEvent::Error { reason, message }]).boxed()
}
