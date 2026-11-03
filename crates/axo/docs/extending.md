# Embedding & Extending axo

`axo` can be customized declaratively through files in `~/.axo/`, `<cwd>/.axo/`, and `.agents/`, or extended in Rust by embedding `axo`, `axo-agent`, and `axo-ai`.

## 1. Declarative Customization

Without writing Rust code, users and repositories can customize an agent session through four file mechanisms:

- **Context files (`AGENTS.md`, `AGENTS.local.md`, `CLAUDE.md`, `CLAUDE.local.md`, `.agents/AGENTS.md`, `.axo/AGENTS.md`, plus any path enabled in `settings.contextFiles`):** Add persona, architecture notes, and coding rules at `~/.axo/AGENTS.md`, the repository root, or subdirectories (loaded on demand when `read`, `write`, or `edit` touches that subtree). See [`prompts-and-context.md`](prompts-and-context.md#context-files-cratesaxosrccontextfilesrs).
- **Prompt overrides (`~/.axo/prompts/<NAME>.md` and `<cwd>/.axo/prompts/<NAME>.md`):** Override `SYSTEM.md`, `WORK.md`, `PROSE.md`, or the summarizer prompts (`SUMMARY.md`, `COMPACTION.md`, `COMPACTION_UPDATE.md`, `COMPACTION_SPLIT_TURN.md`, `BRANCH_SUMMARY.md`). Writing an empty `SYSTEM.md`, `WORK.md`, or `PROSE.md` disables that prompt section.
- **Skills (`.agents/skills/<name>/SKILL.md`, `~/.axo/skills/<name>/SKILL.md`, and `~/.agents/skills/<name>/SKILL.md`):** Package task instructions and reference files that the model reads on demand from `<available_skills>` or the user invokes inline with `/<name> [args]`. Optional `argument-hint` frontmatter is the placeholder apps show after `/<name>`; a skill named like a built-in command (`compact`, `shake`, `model`, `thinking`, `navigate`) gets a warning diagnostic and is left out of the command catalog. See [`prompts-and-context.md`](prompts-and-context.md#skills-cratesaxosrccontextskillsrs).
- **Custom providers and model patches (`~/.axo/models.jsonc`):** Register OpenAI- or Anthropic-compatible endpoints, custom headers, `$ENV` or `!command` API key resolvers, and per-model overrides (`contextWindow`, `maxTokens`, `reasoning`, `thinkingLevelMap`, `cost`, `compat`). See [`configuration.md`](configuration.md#modelsjsonc-axo_aimodelsconfig--credentials).

## 2. Embedding `axo` in a Rust Host

Hosts link the `axo` crate to manage `Models`, `Settings`, `JsonlStore`, and `Session` instances in a single process.

### Building `Axo` and running a `Session`

```rust
use std::path::PathBuf;
use axo::{
    Answer, Axo, Command, CommandInfo, NavigateOptions, PromptPart, PromptRequest, PromptResult,
    RecoveryPolicy, SessionEvent, SessionOptions, ShakeMode,
};
use serde_json::json;

let axo = Axo::builder()
    .dir(PathBuf::from("/var/lib/my-host/axo"))
    .build()
    .await?;

let session = axo
    .create_session(SessionOptions {
        cwd: PathBuf::from("/repo/my-project"),
        model: Some("anthropic/claude-sonnet-4-5".into()),
        thinking: None,
        ask_enabled: true,
        project_access: None,
        exec_env: None,
        sections: vec![(
            "host_context".into(),
            Some("Running inside my-host.".into()),
        )],
        tools: Vec::new(),
        schemes: Vec::new(),
        recovery: RecoveryPolicy::Resume,
    })
    .await?;

let mut attach = session.attach(None).await;
tokio::spawn(async move {
    while let Ok(event) = attach.events.recv().await {
        if let SessionEvent::QuestionPending { call_id, questions } = event {
            // Answer or dismiss pending `ask` questions from the client UI.
            let _ = (call_id, questions);
        }
    }
});

let outcome = session
    .prompt("Inspect src/lib.rs and summarize the public API.")
    .await?;

// Structured parts with an idempotent client id.
match session
    .prompt_with(PromptRequest {
        input: vec![
            PromptPart::Text { text: "Review ".into() },
            PromptPart::File { path: "src/lib.rs".into() },
        ]
        .into(),
        request_id: Some("client-42".into()),
    })
    .await?
{
    PromptResult::Settled { outcome, .. } => { /* the run settled */ }
    PromptResult::Duplicate(existing) => { /* already submitted; nothing started */ }
    PromptResult::Command => { /* the text was a built-in command; it ran */ }
}

// Render the catalog, then run a picked command with JSON arguments or a typed value.
let catalog: Vec<CommandInfo> = session.commands();
session.run_command(Command::decode("shake", json!({ "mode": "images" }))?).await?;
session
    .run_command(Command::Model { model: "anthropic/claude-sonnet-4-5".into() })
    .await?;
```

### `Session` control methods

| Method | Signature | Behavior |
| :--- | :--- | :--- |
| `prompt` | `async fn prompt(&self, input: impl Into<PromptInput>) -> Result<RunOutcome, Error>` | Runs a built-in command typed as text (returning `RunOutcome::Done` once it has run), or saves the parts (handed-over content under `local://`), expands `/<skill> args`, persists a `Prompt` submission, runs the agent loop to completion, and returns `RunOutcome` (`Done`, `Aborted`, `Error`). Returns `Error::Busy` if a run or maintenance task is already active, `Error::InvalidInput` for a missing referenced file or invalid content name |
| `prompt_with` | `async fn prompt_with(&self, request: PromptRequest) -> Result<PromptResult, Error>` | `PromptRequest { input: PromptInput, request_id: Option<String> }`. Like `prompt`, but deduplicates by `request.request_id`: if a submission with the same `request_id` is already in the log, returns `PromptResult::Duplicate(existing)` without starting a second run. A built-in command returns `PromptResult::Command` and is never deduplicated |
| `steer` | `async fn steer(&self, input: impl Into<PromptInput>) -> Result<(), Error>` | Persists a `Steer` submission and queues it to be injected after the current batch of tool calls finishes, before the next model request. Skills expand here too; a built-in command runs under its own policy instead |
| `follow_up` | `async fn follow_up(&self, input: impl Into<PromptInput>) -> Result<(), Error>` | Persists a `FollowUp` submission and queues it to run when the active run would otherwise finish. Skills expand here too; a built-in command runs under its own policy instead |
| `abort` | `async fn abort(&self)` | Cancels active maintenance and stops the current run with `CancelReason::User` (persisting an `Aborted` outcome) |
| `shutdown` | `async fn shutdown(&self)` | Cancels with `CancelReason::Shutdown` and waits for the active run task to settle without recording it as user-aborted, so reopening the session can resume it |
| `set_model` | `async fn set_model(&self, model: Model) -> Result<(), Error>` | Validates compaction limits against `model`, appends `Entry::ModelChange` (and `ThinkingLevelChange` if clamped), and switches the model for the next request |
| `set_thinking` | `async fn set_thinking(&self, level: ThinkingLevel) -> Result<ThinkingLevel, Error>` | Clamps `level` to the current model, appends `Entry::ThinkingLevelChange` when changed, and returns the applied level |
| `answer` / `dismiss` | `fn answer(&self, call_id: &str, answers: Vec<Answer>) -> Result<(), Error>`<br>`fn dismiss(&self, call_id: &str) -> Result<(), Error>` | Resolves or aborts a pending `ask` tool call |
| `attach` | `async fn attach(&self, from: Option<u64>) -> Attach` | Returns `missed` entries after `from`, the current `LiveSnapshot`, and a live `broadcast::Receiver<SessionEvent>` |
| `compact` | `async fn compact(&self) -> Result<(), Error>` | Runs a standalone compaction summary checkpoint in a session-owned task |
| `shake` | `async fn shake(&self, mode: ShakeMode) -> Result<(), Error>` | Runs recoverable artifact elision (`Elide`, `Images`, or `Thinking`) without model calls |
| `navigate` | `async fn navigate(&self, target: Option<EntryId>, options: NavigateOptions) -> Result<(), Error>` | Moves the leaf to `target` (`None` = root), optionally summarizing the abandoned branch into an `Entry::BranchSummary` |
| `fork` | `async fn fork(&self, entry_id: &EntryId) -> Result<Fork, Error>` | Copies the root-to-`entry_id` path into a new session file; hard-links its `artifacts/` (copy fallback) and copies its `local/` files. Forking at a user message returns its parts as `Fork::editable: Option<Vec<PromptPart>>` (the submission's parts, or one `Text` part for messages without one) for the client to edit and resubmit; `Saved` parts resolve in the fork's copied `local/` |
| `reload_exec_env` | `async fn reload_exec_env(&self)` | Invalidates and re-resolves the `ExecEnv` (such as a Nix flake dev shell), queuing a `dev_shell` system-prompt diff if the note changed |
| `commands` | `fn commands(&self) -> Vec<CommandInfo>` | The command catalog: the five built-ins, then each skill whose name is not reserved. See [Commands](#commands) |
| `run_command` | `async fn run_command(&self, command: Command) -> Result<(), Error>` | Runs a built-in `Command` under its policy; `Command::Model` takes `provider/id` only (otherwise `Error::ModelNotFound`) |
| `id` / `path` / `skills` | `fn id(&self) -> &str`<br>`fn path(&self) -> &Path`<br>`fn skills(&self) -> &[Skill]` | The header UUIDv7 (known before the file exists); the session file, created lazily on the first submission; the skills loaded at start, which feed the prompt catalog and `/name` commands |

Prompt input is a `PromptInput` (`From<&str>`, `From<String>`, `From<Vec<PromptPart>>`); see [`prompts-and-context.md`](prompts-and-context.md) for each `PromptPart` and how parts reach the model.

### Commands

`axo` owns the command catalog, argument validation, the `/name args` text parser, and dispatch; apps only render them. App-only commands (opening settings, a new chat) stay in the app.

| Command | Text form | `CommandPolicy` | Effect |
| :--- | :--- | :--- | :--- |
| `compact` | `/compact` | `WhenIdle` | `compact()` |
| `shake` | `/shake <elide\|images\|thinking>` | `WhenIdle` | `shake(mode)` |
| `model` | `/model <provider/id>` | `Alongside` | `set_model` with the catalog model |
| `thinking` | `/thinking <level>` | `Alongside` | `set_thinking(level)` |
| `navigate` | `/navigate <entry> [true\|false]` | `WhenIdle` | `navigate(Some(entry), NavigateOptions { summarize })` |
| each skill | `/<name> <free text>` | `Queued` | Expanded, then sent through whichever of `prompt`, `steer`, or `follow_up` the app used |

- **Policies:** `Alongside` commands run during a turn and take effect from the next request; `WhenIdle` commands return `Error::Busy` during a turn. Maintenance (compaction, shake, branch summary) blocks every command, including `Alongside` ones.
- **Text routing:** when the first part is `Text` starting with `/<builtin>`, `prompt`, `prompt_with`, `steer`, and `follow_up` decode and run the command instead of submitting: nothing is saved as a submission or sent as a user prompt (though `compact` and summarizing `navigate` may request a summary from the model). Positional words map to the schema's properties in order. Invalid arguments return `Error::InvalidCommand { command, message }` (`/shake bogus` → `/shake: unknown variant …`). An unknown `/word` stays ordinary prompt text.
- **Catalog JSON:** `CommandInfo` serializes as `{ "name", "description", "policy": "alongside" | "when_idle" | "queued", "kind": "builtin", "parameters" }` or `{ …, "kind": "skill", "argumentHint" }`. `parameters` is an inlined JSON Schema object (no `$ref`); string properties with `"format": "model"` or `"format": "entry"` tell hosts to offer a model list or a session-tree picker. `Command::decode(name, args)` validates JSON arguments (`null` counts as `{}`); `Command::parse(text)` is the text parser.

## 3. Rust Extension Traits

| Trait | Crate | Purpose |
| :--- | :--- | :--- |
| `ExecEnv` | `axo::exec::ExecEnv` | Resolves the environment (`EnvMap`, optional `prompt_note`, optional one-time `notice`) for `bash` tool calls. Built-in implementations: `HostEnv`, `DevShellEnv`, and `StaticEnv`. Passed via `SessionOptions::exec_env` |
| `Scheme` | `axo::Scheme` | A URL scheme (`name://path`) the file tools accept as a path. `resolve` maps a URL to `Target::File { path, writable }` (the normal file pipeline runs on it; bash rewrites it to that path) or `Target::Virtual` (the scheme's own `read`/`write` handle it). `write_replay` defaults to `Replay::Never`; action writes must keep it at `Never`. Every call gets the session's `SchemeContext`. Built-ins: `artifact://`, `skill://`, `local://`, `axo://`. Passed via `SessionOptions::schemes`; a host scheme named like a built-in replaces it, except `artifact` and `file`, which are reserved |
| `SessionStore` / `SessionWriter` | `axo::store::{SessionStore, SessionWriter}` | Pluggable persistence backend for creating, locking, reading, listing, and appending session entries. Default implementation: `JsonlStore`. Passed via `AxoBuilder::store` |
| `Tool` | `axo_agent::Tool` | Defines a tool (`name`, `description`, JSON Schema `parameters`, `execution_mode`, `replay`, `prompt_guidelines`, `prepare_arguments`, `execute`) for `axo_agent::Agent`. Pass host tools as `SessionOptions::tools` `ToolFactory` closures; axo calls each with the session's `Arc<SessionTools>` (again per fork), so the tool can use `locate`, `schemes`, `rewrite_urls` and the ledger. One named like a built-in replaces it in place |
| `LoopHooks` | `axo_agent::LoopHooks` | Lifecycle hooks on `axo_agent::Agent`: `prepare_request` (can update context/system prompt or fail the run), `should_skip_tools` (intercepts unusable responses before any tool runs), `before_tool_call` (can return `ToolCallDecision::Block { reason }`), `after_tool_call` (can modify `ToolOutput`), and `finish_turn` (can return `TurnFinish::Stop` or `Continue`) |
| `Persist` | `axo_agent::Persist` | Ordered async subscriber on `axo_agent::Agent` awaited before the next model request (used by `Session` to persist `Entry::Message` on `MessageEnd`) |
| `Api` / `Streamer` | `axo_ai::{Api, Streamer}` | Streaming LLM wire adapter (`AnthropicApi`, `CompletionsApi`, `CodexApi`, or `ScriptedApi` behind the `testing` feature). Custom streamers can be injected via `AxoBuilder::streamer` |
| `CredentialStore` | `axo_ai::CredentialStore` | Loads and saves per-provider `Credential`s (`FileCredentialStore` by default) |

### Implementing `axo_agent::Tool`

When driving `axo_agent::Agent` directly (or adding tools to a custom loop), implement `axo_agent::Tool`:

```rust
use async_trait::async_trait;
use axo_agent::{ExecutionMode, Replay, Tool, ToolError, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub struct PingTool {
    schema: Value,
}

impl PingTool {
    pub fn new() -> Self {
        Self {
            schema: json!({
                "type": "object",
                "properties": { "target": { "type": "string" } },
                "required": ["target"]
            }),
        }
    }
}

#[async_trait]
impl Tool for PingTool {
    fn name(&self) -> &str {
        "ping"
    }

    fn description(&self) -> &str {
        "Check connectivity to a target."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn replay(&self, _call: &ToolCall) -> Replay {
        Replay::Safe
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &["Use ping to check target reachability before opening a connection."]
    }

    async fn execute(
        &self,
        call: &ToolCall,
        _cancel: CancellationToken,
        _updates: &(dyn Fn(ToolUpdate) + Send + Sync),
    ) -> Result<ToolOutput, ToolError> {
        let target = call
            .arguments
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError("missing target".into()))?;
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: format!("pong from {target}"),
            }],
            details: Value::Null,
        })
    }
}
```

- **`ExecutionMode::Parallel` vs `ExecutionMode::Sequential`:** `Agent` runs a turn's tool batch concurrently when `AgentConfig::tool_execution` is `Parallel` and every tool in the batch returns `ExecutionMode::Parallel` (preparation and `before_tool_call` always run sequentially in call order, and `ToolResult` messages are always emitted in call order). If any tool in the batch returns `ExecutionMode::Sequential`, the batch runs one call at a time.
- **`Replay::Safe` vs `Replay::Never`:** Controls whether an unfinished tool call is re-executed (`Safe`) or marked interrupted (`Never`) when `Session` recovers a crashed run.
