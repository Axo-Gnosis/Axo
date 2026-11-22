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
    /// A configuration value is invalid; `origin` names the layer or prompt override that holds it.
    #[error("{origin}: {message}")]
    Config { origin: String, message: String },
    /// A host integration such as a `ConfigSource` failed; carries the host's own error.
    #[error(transparent)]
    Host(Box<dyn std::error::Error + Send + Sync>),
    /// A session file's header line is missing or does not parse.
    #[error("{}: {message}", path.display())]
    InvalidSessionHeader { path: PathBuf, message: String },
    /// A host scheme has an invalid or reserved name.
    #[error("{0}")]
    Scheme(String),
    #[error("No model configured")]
    NoModel,
    #[error("Model not found: {0}")]
    ModelNotFound(String),
    #[error("Mode not found: {0}")]
    ModeNotFound(String),
    #[error("Skill not found: {0}")]
    SkillNotFound(String),
    /// `SessionOptions::agent` names no agent definition, or names a team.
    #[error("Agent not found: {0}")]
    AgentNotFound(String),
    #[error("Agent {0} cannot run as a root session (runAs: subagent)")]
    AgentNotRoot(String),
    #[error("Agent {0} cannot be spawned (runAs: root)")]
    AgentNotSubagent(String),
    /// A spawn hook blocked a spawn, the depth limit was reached, or the parent is shutting down.
    #[error("{0}")]
    SpawnBlocked(String),
    /// `Axo::session` knows no child session with this id.
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    /// `Axo::open_session` was given a child session's file.
    #[error("{}: a child session; open its root session{} and reach the child through Axo::session", path.display(), root.as_ref().map(|root| format!(" ({})", root.display())).unwrap_or_default())]
    ChildSession {
        path: PathBuf,
        root: Option<PathBuf>,
    },
    /// `@name` names no `modelRoles` entry.
    #[error("Unknown model role: @{0}")]
    UnknownModelRole(String),
    /// A model selector is malformed: a bad `:thinking` suffix, or a role whose value names another role.
    #[error("Invalid model selector \"{selector}\": {message}")]
    InvalidModelSelector { selector: String, message: String },
    /// A built-in command's arguments are missing or invalid, or its name is unknown.
    #[error("/{command}: {message}")]
    InvalidCommand { command: String, message: String },
    /// Invalid caller input: a prompt part names a missing file, content has an invalid name,
    /// or a spawn or team request is malformed.
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
    #[error("No pending mode result for tool call: {0}")]
    NoPendingResult(String),
    /// `withdraw` named a submission that was already delivered, withdrawn, or never queued
    /// as a steer or follow-up.
    #[error("Submission is not queued: {0}")]
    NotQueued(String),
    /// Another writer holds the session's lock.
    #[error("Session is open in another process: {}", path.display())]
    SessionBusy { path: PathBuf },
    /// `navigate`/`fork` named an entry id the session does not have.
    #[error("Entry not found: {0}")]
    EntryNotFound(String),
}

/// Machine-readable category of an [`Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Ai,
    Agent,
    Io,
    Config,
    Host,
    InvalidSessionHeader,
    Scheme,
    NoModel,
    ModelNotFound,
    ModeNotFound,
    SkillNotFound,
    AgentNotFound,
    AgentNotRoot,
    AgentNotSubagent,
    SpawnBlocked,
    SessionNotFound,
    ChildSession,
    UnknownModelRole,
    InvalidModelSelector,
    InvalidCommand,
    InvalidInput,
    Busy,
    Compaction,
    MaintenanceAborted,
    ContextChanged,
    InvalidHistory,
    NoPendingQuestion,
    NoPendingResult,
    NotQueued,
    SessionBusy,
    EntryNotFound,
}

impl Error {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Ai(_) => ErrorCode::Ai,
            Self::Agent(_) => ErrorCode::Agent,
            Self::Io { .. } => ErrorCode::Io,
            Self::Config { .. } => ErrorCode::Config,
            Self::Host(_) => ErrorCode::Host,
            Self::InvalidSessionHeader { .. } => ErrorCode::InvalidSessionHeader,
            Self::Scheme(_) => ErrorCode::Scheme,
            Self::NoModel => ErrorCode::NoModel,
            Self::ModelNotFound(_) => ErrorCode::ModelNotFound,
            Self::ModeNotFound(_) => ErrorCode::ModeNotFound,
            Self::SkillNotFound(_) => ErrorCode::SkillNotFound,
            Self::AgentNotFound(_) => ErrorCode::AgentNotFound,
            Self::AgentNotRoot(_) => ErrorCode::AgentNotRoot,
            Self::AgentNotSubagent(_) => ErrorCode::AgentNotSubagent,
            Self::SpawnBlocked(_) => ErrorCode::SpawnBlocked,
            Self::SessionNotFound(_) => ErrorCode::SessionNotFound,
            Self::ChildSession { .. } => ErrorCode::ChildSession,
            Self::UnknownModelRole(_) => ErrorCode::UnknownModelRole,
            Self::InvalidModelSelector { .. } => ErrorCode::InvalidModelSelector,
            Self::InvalidCommand { .. } => ErrorCode::InvalidCommand,
            Self::InvalidInput(_) => ErrorCode::InvalidInput,
            Self::Busy => ErrorCode::Busy,
            Self::Compaction(_) => ErrorCode::Compaction,
            Self::MaintenanceAborted => ErrorCode::MaintenanceAborted,
            Self::ContextChanged => ErrorCode::ContextChanged,
            Self::InvalidHistory(_) => ErrorCode::InvalidHistory,
            Self::NoPendingQuestion(_) => ErrorCode::NoPendingQuestion,
            Self::NoPendingResult(_) => ErrorCode::NoPendingResult,
            Self::NotQueued(_) => ErrorCode::NotQueued,
            Self::SessionBusy { .. } => ErrorCode::SessionBusy,
            Self::EntryNotFound(_) => ErrorCode::EntryNotFound,
        }
    }
}
