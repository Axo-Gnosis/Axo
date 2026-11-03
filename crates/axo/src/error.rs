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
    /// A host scheme has an invalid or reserved name.
    #[error("{0}")]
    Scheme(String),
    /// Neither `AXO_DIR` nor `HOME` is set.
    #[error("Cannot locate the axo directory: set AXO_DIR or HOME")]
    NoAxoDir,
    #[error("No model configured")]
    NoModel,
    #[error("Model not found: {0}")]
    ModelNotFound(String),
    /// A built-in command's arguments are missing or invalid, or its name is unknown.
    #[error("/{command}: {message}")]
    InvalidCommand { command: String, message: String },
    /// A prompt part names a missing file, or content has an invalid name.
    #[error("{0}")]
    InvalidInput(String),
    /// A run or maintenance operation owns the session's idle reservation.
    #[error(
        "Agent is already processing. Use steer() or follow_up() to queue messages, or wait for completion."
    )]
    Busy,
    /// Invalid compaction limits, or a compaction that cannot be produced or was rejected.
    #[error("{0}")]
    Compaction(String),
    #[error("Maintenance aborted")]
    MaintenanceAborted,
    #[error("Session context changed during maintenance")]
    ContextChanged,
    #[error("Invalid session history: {0}")]
    InvalidHistory(String),
    /// `answer`/`dismiss` named a tool call with no pending question.
    #[error("No pending question for tool call: {0}")]
    NoPendingQuestion(String),
    /// Another writer holds the session's lock.
    #[error("Session is open in another process: {}", path.display())]
    SessionBusy { path: PathBuf },
    /// `navigate`/`fork` named an entry id the session does not have.
    #[error("Entry not found: {0}")]
    EntryNotFound(String),
}
