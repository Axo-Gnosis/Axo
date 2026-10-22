//! axo's coding-agent core: the built-in tools, Nix dev-shell exec, prompt and context
//! assembly, config, and sessions on top of `axo-ai` and `axo-agent`.

pub mod axo;
pub mod config;
pub mod context;
pub mod error;
pub mod exec;
pub mod session;
pub mod store;
pub mod tools;

pub use axo::{Axo, AxoBuilder};
pub use config::{
    AskSettings, AxoDir, BashSettings, DevShellMode, ProjectAccess, ProjectFiles, ProjectSettings,
    Settings,
};
pub use context::Diagnostic;
pub use error::Error;
pub use exec::devshell::DevShellEnv;
pub use exec::{EnvMap, ExecEnv, HostEnv, Resolved, StaticEnv};
pub use session::{
    Attach, Fork, LiveSnapshot, PromptRequest, PromptResult, RecoveryPolicy, Session, SessionEvent,
    SessionOptions,
};
pub use tools::SessionTools;
pub use tools::ask::{Answer, AskOutcome, PendingQuestions, Question, QuestionOption};
