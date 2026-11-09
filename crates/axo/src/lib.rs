//! axo's coding-agent core: the built-in tools, Nix dev-shell exec, prompt and context
//! assembly, config, and sessions on top of `axo-ai` and `axo-agent`.

mod axo;
mod commands;
mod compaction;
mod config;
pub mod context;
mod error;
pub mod exec;
mod input;
mod jobs;
mod recovery;
pub mod schemes;
mod session;
pub mod store;
pub mod tools;

pub use axo::{Axo, AxoBuilder};
pub use commands::{Command, CommandInfo, CommandKind, CommandPolicy};
pub use config::{
    AskSettings, BashSettings, CompactionSettings, ConfigLayer, ConfigSource, DevShellMode,
    EffectiveConfig, ModelChoice, PROMPT_NAMES, ProjectAccess, ProjectFiles, ProjectSettings,
    PromptOverride, RetrySettings, Settings, SubagentSettings, SummaryRequest, UserConfig,
};
pub use context::agents::AgentSpec;
pub use context::{Diagnostic, DiagnosticOrigin};
pub use error::Error;
pub use exec::devshell::DevShellEnv;
pub use exec::host::{OutputError, ProcessSupervisor, Supervision};
pub use exec::{EnvMap, ExecEnv, Resolved, StaticEnv};
pub use input::{PromptInput, PromptPart};
pub use jobs::{JobInfo, JobKind, JobStatus};
pub use schemes::{Scheme, SchemeContext, SchemeUrl, Schemes, Target};
pub use session::{
    Attach, ChildHandle, Fork, InlineSpec, LiveSnapshot, MaintenanceKind, NavigateOptions,
    PromptRequest, PromptResult, RecoveryPolicy, Session, SessionEvent, SessionOptions, ShakeMode,
    SpawnContext, SpawnDecision, SpawnHook, SpawnRequest,
};
pub use store::{
    CompactionReason, ContextReplacement, Entry, EntryBase, EntryId, FORMAT_VERSION, Header,
    JsonlStore, ProjectedSource, Projection, RunOutcome, SeqEntry, SessionInfo, SessionStore,
    SessionWriter, SpawnLink, SubmissionKind, TIMESTAMP_FORMAT, project,
};
pub use tools::{Answer, CommandRequest, Question, QuestionOption, SessionTools, ToolFactory};

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
