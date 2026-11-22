//! axo's coding-agent core: the built-in tools, Nix dev-shell exec, prompt and context
//! assembly, config, and sessions on top of `axo-ai` and `axo-agent`.

mod axo;
mod commands;
mod compaction;
mod config;
pub mod context;
mod coordination;
mod error;
pub mod exec;
pub mod ext;
mod host;
mod input;
mod jobs;
mod models;
mod recovery;
pub mod schemes;
mod session;
pub mod store;
pub mod tools;

pub use axo::{Axo, AxoBuilder, Catalog};
pub use commands::{
    Command, CommandHandler, CommandInfo, CommandKind, CommandPolicy, RegisteredCommand,
    valid_command_name,
};
pub use config::{
    AskSettings, BashSettings, CompactionSettings, ConfigLayer, ConfigSource, DevShellMode,
    EffectiveConfig, EnabledExtension, ExtensionListError, ModelChoice, PROMPT_NAMES,
    ProjectAccess, ProjectFiles, ProjectSettings, PromptOverride, RetrySettings, Settings,
    SubagentSettings, SummaryRequest, UserConfig,
};
pub use context::agents::{AgentSpec, RunAs};
pub use context::keywords::{KeywordMatch, KeywordSettings, KeywordSpec};
pub use context::modes::{BashAccess, ModeResult, ModeSpec, ToolLimits};
pub use context::prompt::valid_section_name;
pub use context::{Diagnostic, DiagnosticOrigin};
pub use coordination::{
    KillReceipt, SendIdentity, SendKind, SendReceipt, SpawnReceipt, SubmissionProvenance,
    WaitReason, WaitReceipt,
};
pub use error::{Error, ErrorCode};
pub use exec::devshell::DevShellEnv;
pub use exec::host::{
    OutputChunk, OutputError, OutputStream, Pipe, PipeChunk, ProcessSupervisor, Sandbox,
    Supervision,
};
pub use exec::{EnvMap, ExecEnv, Resolved, StaticEnv};
pub use host::SessionPage;
pub use input::{PromptInput, PromptPart};
pub use jobs::{
    ChildProgress, CommandExit, CommandOutput, JobInfo, JobKind, JobMessageInfo, JobStatus, Killer,
    MessageDelivery, OutputProgress, Promotion, jobs_from_entries,
};
pub use models::CompletionRequest;
pub use schemes::{Scheme, SchemeContext, SchemeUrl, Schemes, Target, valid_scheme_name};
pub use session::wire::{EntryPage, OutputPage};
pub use session::{
    AcceptResultOptions, Activity, Attach, ChildHandle, ContextFilesInspection, ContextUsage, Fork,
    HostDelivery, HostMessage, InlineSpec, LiveSnapshot, MaintenanceKind, NavigateOptions,
    PendingResult, PromptInspection, PromptRequest, PromptResult, QueuedInput, RecoveryPolicy,
    Registrations, ResultResolution, Session, SessionEvent, SessionEventKind, SessionOptions,
    SessionToolInfo, ShakeMode, SpawnContext, SpawnDecision, SpawnHook, SpawnOrigin, SpawnRequest,
    SpawnTeam, TeamHandle, TeamMemberRequest, TeamRequest, ToolPolicy, ToolScope,
};
pub use store::{
    CompactionReason, ContextReplacement, Entry, EntryBase, EntryId, FORMAT_VERSION, Header,
    JsonlStore, ModeRestore, ProjectedSource, Projection, RosterEntry, RunOutcome, SeqEntry,
    SessionInfo, SessionStore, SessionWriter, SpawnLink, SubmissionKind, SubmittedCustom,
    TIMESTAMP_FORMAT, TeamLink, TeamRef, project,
};
pub use tools::{
    Answer, ArtifactSlot, BashRequest, CommandRequest, Exposure, FileAccess, FileChange, FileRead,
    FileRecord, FileWrite, HostTool, LedgerRecord, LineDiff, MAX_SNAPSHOT_BYTES, Omitted, Question,
    QuestionOption, SessionTools, SnapshotBlob, SnapshotStore, SquashedChange, ToolAccess,
    ToolFactory, WriteRoot, WriteScope, Written, file_changes, file_details, file_records, squash,
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

    #[doc = include_str!("../docs/extension-api.md")]
    mod extension_api {}
}
