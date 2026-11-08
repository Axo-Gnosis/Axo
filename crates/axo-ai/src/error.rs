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
    #[error("No API key for provider: {provider}")]
    NoApiKey { provider: String },
    /// Login, token exchange, refresh or credential lookup failed.
    #[error("{0}")]
    Auth(String),
}
