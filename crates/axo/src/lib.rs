//! axo's coding-agent core: the built-in tools, Nix dev-shell exec, prompt and context
//! assembly, config, and sessions on top of `axo-ai` and `axo-agent`.

pub mod axo;
mod commands;
mod compaction;
pub mod config;
pub mod context;
pub mod error;
pub mod exec;
mod input;
pub mod jobs;
pub mod recovery;
pub mod schemes;
pub mod session;
pub mod store;
pub mod tools;

pub use axo::{Axo, AxoBuilder};
pub use commands::{Command, CommandInfo, CommandKind, CommandPolicy};
pub use config::{
    AskSettings, AxoDir, BashSettings, CompactionSettings, DevShellMode, ProjectAccess,
    ProjectFiles, ProjectSettings, Settings, SummaryRequest,
};
pub use context::Diagnostic;
pub use error::Error;
pub use exec::devshell::DevShellEnv;
pub use exec::host::{OutputError, ProcessSupervisor, Supervision};
pub use exec::{EnvMap, ExecEnv, HostEnv, Resolved, StaticEnv};
pub use input::{PromptInput, PromptPart};
pub use schemes::{Scheme, SchemeContext, SchemeUrl, Schemes, Target};
pub use session::{
    Attach, Fork, LiveSnapshot, MaintenanceKind, NavigateOptions, PromptRequest, PromptResult,
    RecoveryPolicy, Session, SessionEvent, SessionOptions, ShakeMode,
};
pub use tools::ask::{Answer, AskOutcome, PendingQuestions, Question, QuestionOption};
pub use tools::{SessionTools, ToolFactory};

#[cfg(doctest)]
mod markdown_doctests {
    #[doc = include_str!("../../../README.md")]
    mod readme {}

    #[doc = include_str!("../docs/README.md")]
    mod docs_readme {}

    #[doc = include_str!("../docs/configuration.md")]
    mod configuration {}

    #[doc = include_str!("../docs/prompts-and-context.md")]
    mod prompts_and_context {}

    #[doc = include_str!("../docs/sessions-and-compaction.md")]
    mod sessions_and_compaction {}

    #[doc = include_str!("../docs/extending.md")]
    mod extending {}
}
