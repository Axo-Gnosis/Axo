use std::path::PathBuf;

/// Every error `axo` returns.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Ai(#[from] axo_ai::Error),
    #[error(transparent)]
    Agent(#[from] axo_agent::Error),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A config file (`settings.jsonc`, `models.jsonc`) could not be parsed.
    #[error("{}: {message}", path.display())]
    Config { path: PathBuf, message: String },
    /// Neither `AXO_DIR` nor `HOME` is set.
    #[error("Cannot locate the axo directory: set AXO_DIR or HOME")]
    NoAxoDir,
    #[error("No model configured")]
    NoModel,
    #[error("Model not found: {0}")]
    ModelNotFound(String),
    /// `prompt` was called while a run is active.
    #[error(
        "Agent is already processing. Use steer() or follow_up() to queue messages, or wait for completion."
    )]
    Busy,
    /// `answer`/`dismiss` named a tool call with no pending question.
    #[error("No pending question for tool call: {0}")]
    NoPendingQuestion(String),
}
