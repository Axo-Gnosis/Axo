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

pub use axo::{Axo, AxoBuilder, Catalog};
pub use commands::{Command, CommandInfo, CommandKind, CommandPolicy};
pub use config::{
    AskSettings, BashSettings, CompactionSettings, ConfigLayer, ConfigSource, DevShellMode,
    EffectiveConfig, ModelChoice, PROMPT_NAMES, ProjectAccess, ProjectFiles, ProjectSettings,
    PromptOverride, RetrySettings, Settings, SubagentSettings, SummaryRequest, UserConfig,
};
pub use context::agents::{AgentSpec, RunAs};
pub use context::modes::{ModeResult, ModeSpec, ToolLimits};
pub use context::{Diagnostic, DiagnosticOrigin};
pub use error::Error;
pub use exec::devshell::DevShellEnv;
pub use exec::host::{OutputError, ProcessSupervisor, Supervision};
pub use exec::{EnvMap, ExecEnv, Resolved, StaticEnv};
pub use input::{PromptInput, PromptPart};
pub use jobs::{
    ChildProgress, CommandExit, CommandOutput, JobInfo, JobKind, JobMessageInfo, JobStatus, Killer,
    MessageDelivery, OutputProgress, Promotion, jobs_from_entries,
};
pub use schemes::{Scheme, SchemeContext, SchemeUrl, Schemes, Target};
pub use session::{
    Attach, ChildHandle, ContextUsage, Fork, HostDelivery, HostMessage, InlineSpec, LiveSnapshot,
    MaintenanceKind, NavigateOptions, PendingResult, PromptRequest, PromptResult, RecoveryPolicy,
    Session, SessionEvent, SessionOptions, ShakeMode, SpawnContext, SpawnDecision, SpawnHook,
    SpawnOrigin, SpawnRequest, SpawnTeam, TeamHandle, TeamMemberRequest, TeamRequest, ToolPolicy,
    ToolScope,
};
pub use store::{
    CompactionReason, ContextReplacement, Entry, EntryBase, EntryId, FORMAT_VERSION, Header,
    JsonlStore, ModeRestore, ProjectedSource, Projection, RosterEntry, RunOutcome, SeqEntry,
    SessionInfo, SessionStore, SessionWriter, SpawnLink, SubmissionKind, SubmittedCustom,
    TIMESTAMP_FORMAT, TeamLink, TeamRef, project,
};
pub use tools::{
    Answer, CommandRequest, Exposure, HostTool, Question, QuestionOption, SessionTools, ToolFactory,
};

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
