use crate::types::ProviderFailure;

/// Every error `axo-ai` returns.
///
/// Provider streams never return errors: failures end the stream with an
/// `AssistantEvent::Error`. This enum covers setup paths (config, auth, catalog,
/// credential storage) and the HTTP helpers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// A config file (`models.jsonc`, a config value) could not be parsed or resolved.
    #[error("{0}")]
    Config(String),
    /// The model is not in the registry (its provider was unregistered or replaced, or a reload dropped it).
    #[error("Unknown model: {provider}/{model}")]
    UnknownModel { provider: String, model: String },
    #[error("No API key for provider: {provider}")]
    NoApiKey { provider: String },
    /// The provider requires a stored OAuth login rather than an API key.
    #[error("Log in to {provider}")]
    LoginRequired { provider: String },
    /// Login, token exchange, refresh or credential lookup failed.
    #[error("{0}")]
    Auth(String),
    /// The provider is reserved but cannot run now (`ProviderLayer::set_unavailable`, or its
    /// login, refresh or rewrite callback was interrupted). Never retried automatically.
    #[error("provider {provider}: {reason}")]
    ProviderUnavailable { provider: String, reason: String },
}

impl Error {
    /// The structured failure a terminal message carries for this error: only
    /// `ProviderUnavailable` has one, coded `provider_unavailable` and not retryable, so its
    /// text never reaches the transient-error patterns.
    pub fn provider_failure(&self) -> Option<ProviderFailure> {
        matches!(self, Error::ProviderUnavailable { .. }).then(|| ProviderFailure {
            code: Some("provider_unavailable".into()),
            should_retry: Some(false),
            ..ProviderFailure::default()
        })
    }
}
