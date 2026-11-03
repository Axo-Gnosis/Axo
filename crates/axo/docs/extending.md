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

```rust,no_run
# async fn example() -> Result<(), axo::Error> {
use std::path::PathBuf;
use axo::{
    Answer, Axo, Command, CommandInfo, NavigateOptions, PromptPart, PromptRequest, PromptResult,
    RecoveryPolicy, SessionEvent, SessionOptions, ShakeMode,
};
use serde_json::json;

let axo = Axo::builder()
    .dir(PathBuf::from("/var/lib/my-host/axo"))
    // Commands run in process groups by default (`Supervision::ProcessGroup`); see below
    // for the opt-in cgroup mode.
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
}

// Offer whichever catalog entries suit the app, in any form (menu, buttons, typed
// `/name args`), then run a picked command with JSON arguments, typed text, or a value.
let catalog: Vec<CommandInfo> = session.commands();
session.run_command(Command::decode("shake", json!({ "mode": "images" }))?).await?;
let typed = "/thinking high";
match Command::parse(typed) {
    Some(command) => session.run_command(command?).await?,
    None => drop(session.prompt(typed).await?),
}
session
    .run_command(Command::Model { model: "anthropic/claude-sonnet-4-5".into() })
    .await?;
# Ok(())
# }
```

Local commands (`bash`, dev-shell capture, and host tools using `SessionTools::supervisor`) run under the host-selected `Supervision`, passed to `AxoBuilder::supervision`. The default, `Supervision::ProcessGroup`, needs nothing from the host: each command leads its own process group, guarded best-effort by an `sh` process that kills the group if the host dies; descendants that leave the group (`setsid`, double fork) are not tracked. Durable server hosts can select `Supervision::Cgroup { program, args, cgroup_root }`, under which every command runs in its own cgroup v2 job directory watched by a guardian, so detached descendants are contained too (only the simultaneous death of host and guardian leaves a job behind). axo ships no supervisor binary: the host provides it by routing one of its own command lines to `axo::exec::supervisor::run(args)` and naming that route as `program` + `args`:

```rust,no_run
use std::ffi::OsStr;
use std::process::ExitCode;
use axo::{Axo, Supervision};

const SUPERVISOR_ROUTE: &str = "my-host-supervisor";

fn main() -> ExitCode {
    // Must be the first statement: before any runtime, thread, or host setup. axo spawns
    // `<exe> my-host-supervisor guard <dir>` and `<exe> my-host-supervisor enter <dir> <command…>`
    // directly (an `exec`-only wrapper is fine; a forking one breaks the guard's parent check).
    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(SUPERVISOR_ROUTE)) {
        return axo::exec::supervisor::run(std::env::args_os().skip(2));
    }
    tokio::runtime::Runtime::new().expect("tokio runtime").block_on(async {
        let axo = Axo::builder()
            .supervision(Supervision::Cgroup {
                program: std::env::current_exe().expect("own executable"),
                args: vec![SUPERVISOR_ROUTE.into()],
                cgroup_root: None, // axo's own cgroup; or a delegated cgroup v2 directory
            })
            .build()
            .await
            .expect("axo");
        drop(axo);
    });
    ExitCode::SUCCESS
}
```

The supervisor process inherits the host's launch environment (so wrapped executables keep working); the command's own environment is applied only after cgroup entry. Cgroup mode needs a usable cgroup v2 root: axo's own cgroup when `cgroup_root` is `None` (run the host with systemd `Delegate=yes`), or the delegated directory given. Without one, or when the route does not reach `run`, `bash` and dev-shell capture fail closed with `Command supervision unavailable` / `Command supervisor … did not report ready` while everything else keeps working; an explicit cgroup choice never falls back to process groups. The `drive` example does this with `--cgroup`. See [Command supervision](configuration.md#command-supervision-axosupervision).

### `Session` control methods

| Method | Signature | Behavior |
| :--- | :--- | :--- |
| `prompt` | `async fn prompt(&self, input: impl Into<PromptInput>) -> Result<RunOutcome, Error>` | Saves the parts (handed-over content under `local://`), expands `/<skill> args`, persists a `Prompt` submission, runs the agent loop to completion, and returns `RunOutcome` (`Done`, `Aborted`, `Error`). Never runs a built-in command: `/model x` is ordinary prompt text. Returns `Error::Busy` if a run or maintenance task is already active, `Error::InvalidInput` for a missing referenced file or invalid content name |
| `prompt_with` | `async fn prompt_with(&self, request: PromptRequest) -> Result<PromptResult, Error>` | `PromptRequest { input: PromptInput, request_id: Option<String> }`. Like `prompt`, but deduplicates by `request.request_id`: if a submission with the same `request_id` is already in the log, returns `PromptResult::Duplicate(existing)` without starting a second run |
| `steer` | `async fn steer(&self, input: impl Into<PromptInput>) -> Result<(), Error>` | Persists a `Steer` submission and queues it to be injected after the current batch of tool calls finishes, before the next model request. While it is queued, tools see `ToolContext::steer` pending: a foreground `bash` command moves to the background and `wait` returns. Skills expand here too |
| `follow_up` | `async fn follow_up(&self, input: impl Into<PromptInput>) -> Result<(), Error>` | Persists a `FollowUp` submission and queues it to run when the active run would otherwise finish. Skills expand here too |
| `abort` | `async fn abort(&self)` | Stop: cancels active maintenance and stops the current run with `CancelReason::User` (persisting an `Aborted` outcome), including its foreground tool calls and foreground `bash` command. Background jobs keep running. A Stop that interrupted an active run pauses automatic delivery of finished job results until the next `prompt`, `steer`, or `follow_up`; a Stop while idle changes nothing (see [Background Job Durability](sessions-and-compaction.md#background-job-durability)) |
| `shutdown` | `async fn shutdown(&self)` | Cancels with `CancelReason::Shutdown`, waits for the active run task to settle without recording it as user-aborted (so reopening the session can resume it), then stops every open job (SIGTERM to its container, SIGKILL after 1 s) and waits for each job's task. A job stopped this way records no result; the next open marks it interrupted. Dropping a `Session` without `shutdown` cancels the run and synchronously kills every open job's container |
| `set_model` | `async fn set_model(&self, model: Model) -> Result<(), Error>` | Validates compaction limits against `model`, appends `Entry::ModelChange` (and `ThinkingLevelChange` if clamped), and switches the model for the next request |
| `set_thinking` | `async fn set_thinking(&self, level: ThinkingLevel) -> Result<ThinkingLevel, Error>` | Clamps `level` to the current model, appends `Entry::ThinkingLevelChange` when changed, and returns the applied level |
| `answer` / `dismiss` | `fn answer(&self, call_id: &str, answers: Vec<Answer>) -> Result<(), Error>`<br>`fn dismiss(&self, call_id: &str) -> Result<(), Error>` | Resolves or aborts a pending `ask` tool call |
| `attach` | `async fn attach(&self, from: Option<u64>) -> Attach` | Returns `missed` entries after `from`, the current `LiveSnapshot`, and a live `broadcast::Receiver<SessionEvent>` |
| `compact` | `async fn compact(&self) -> Result<(), Error>` | Runs a standalone compaction summary checkpoint in a session-owned task |
| `shake` | `async fn shake(&self, mode: ShakeMode) -> Result<(), Error>` | Runs recoverable artifact elision (`Elide`, `Images`, or `Thinking`) without model calls |
| `navigate` | `async fn navigate(&self, target: Option<EntryId>, options: NavigateOptions) -> Result<(), Error>` | Moves the leaf to `target` (`None` = root), optionally summarizing the abandoned branch into an `Entry::BranchSummary`. Running jobs keep running; their results are delivered on whichever branch is selected when they finish, naming the originating command |
| `fork` | `async fn fork(&self, entry_id: &EntryId) -> Result<Fork, Error>` | Copies the root-to-`entry_id` path into a new session file; hard-links its `artifacts/` (copy fallback; the output files of still-running jobs are always copied as snapshots) and copies its `local/` files. The fork gets its own job registry and no handles to the parent's running jobs. Forking at a user message returns its parts as `Fork::editable: Option<Vec<PromptPart>>` (the submission's parts, or one `Text` part for messages without one) for the client to edit and resubmit; `Saved` parts resolve in the fork's copied `local/` |
| `reload_exec_env` | `async fn reload_exec_env(&self)` | Invalidates and re-resolves the `ExecEnv` (such as a Nix flake dev shell), queuing a `dev_shell` system-prompt diff if the note changed |
| `commands` | `fn commands(&self) -> Vec<CommandInfo>` | The command catalog a host may offer: the five built-ins, then each skill whose name is not reserved. See [Commands](#commands) |
| `run_command` | `async fn run_command(&self, command: Command) -> Result<(), Error>` | Runs a built-in `Command` under its policy; the only way a built-in runs. `Command::Model` takes `provider/id` only (otherwise `Error::ModelNotFound`) |
| `id` / `path` / `skills` | `fn id(&self) -> &str`<br>`fn path(&self) -> &Path`<br>`fn skills(&self) -> &[Skill]` | The header UUIDv7 (known before the file exists); the session file, created lazily on the first submission; the skills loaded at start, which feed the prompt catalog and `/name` commands |

Prompt input is a `PromptInput` (`From<&str>`, `From<String>`, `From<Vec<PromptPart>>`); see [`prompts-and-context.md`](prompts-and-context.md) for each `PromptPart` and how parts reach the model.

### Commands

`axo` owns the command catalog, argument validation, the `/name args` text parser, and dispatch. Hosts decide which commands to offer and in what form: a slash menu, buttons, a model picker, or nothing. A built-in only runs when the host calls `run_command`, so a host never inherits a command it does not expose. App-only commands (opening settings, a new chat) stay in the app.

| Command | Text form | `CommandPolicy` | Effect |
| :--- | :--- | :--- | :--- |
| `compact` | `/compact` | `WhenIdle` | `compact()` |
| `shake` | `/shake <elide\|images\|thinking>` | `WhenIdle` | `shake(mode)` |
| `model` | `/model <provider/id>` | `Alongside` | `set_model` with the catalog model |
| `thinking` | `/thinking <level>` | `Alongside` | `set_thinking(level)` |
| `navigate` | `/navigate <entry> [true\|false]` | `WhenIdle` | `navigate(Some(entry), NavigateOptions { summarize })` |
| each skill | `/<name> <free text>` | `Queued` | Prompt text: expanded, then sent through whichever of `prompt`, `steer`, or `follow_up` the app used |

- **Policies:** `Alongside` commands run during a turn and take effect from the next request; `WhenIdle` commands return `Error::Busy` during a turn. Maintenance (compaction, shake, branch summary) blocks every command, including `Alongside` ones.
- **Typed commands:** a host that accepts typed `/name args` text calls `Command::parse(text)` before prompting. It returns `None` for text that does not start with `/<builtin>` (including skills and unknown `/word`s, which stay prompt text), and otherwise the decoded `Command` for `run_command`. Positional words map to the schema's properties in order. Invalid arguments return `Error::InvalidCommand { command, message }` (`/shake bogus` → `/shake: unknown variant …`). A built-in run this way is never saved as a submission or sent as a user prompt (though `compact` and summarizing `navigate` may request a summary from the model).
- **Catalog JSON:** `CommandInfo` serializes as `{ "name", "description", "policy": "alongside" | "when_idle" | "queued", "kind": "builtin", "parameters" }` or `{ …, "kind": "skill", "argumentHint" }`. `parameters` is an inlined JSON Schema object (no `$ref`); string properties with `"format": "model"` or `"format": "entry"` tell hosts to offer a model list or a session-tree picker, or to leave the command out where the app already has that control (a transcript with branch buttons covers `navigate`). `Command::decode(name, args)` validates JSON arguments (`null` counts as `{}`).

## 3. Rust Extension Traits

| Trait | Crate | Purpose |
| :--- | :--- | :--- |
| `ExecEnv` | `axo::exec::ExecEnv` | Resolves the environment (`EnvMap`, optional `prompt_note`, optional one-time `notice`) for `bash` tool calls. Built-in implementations: `HostEnv`, `DevShellEnv`, and `StaticEnv`. Passed via `SessionOptions::exec_env`. `DevShellEnv::new(base, settings, cache_dir, supervisor: Arc<ProcessSupervisor>)` runs its direnv, git, nix and capture subprocesses through that supervisor; `ExecEnv` only resolves the environment, and every command still runs supervised |
| `Scheme` | `axo::Scheme` | A URL scheme (`name://path`) the file tools accept as a path. `resolve` maps a URL to `Target::File { path, writable }` (the normal file pipeline runs on it; bash rewrites it to that path) or `Target::Virtual` (the scheme's own `read`/`write` handle it). `write_replay` defaults to `Replay::Never`; action writes must keep it at `Never`. Every call gets the session's `SchemeContext`. Built-ins: `artifact://`, `skill://`, `local://`, `axo://`, and in open sessions `proc://` (background jobs; see [Background Jobs](prompts-and-context.md#background-jobs-cratesaxosrcjobsrs)). Passed via `SessionOptions::schemes`; a host scheme named like a built-in replaces it, except `artifact` and `file`, which are reserved. Hosts resolve URLs they display through `Session::schemes()` (`exact`, then `resolve`, then `read` for `Target::Virtual`), or for a session that is not open through `Axo::schemes(path, project_access, host_schemes)` (pass the same `project_access` the session is opened with): the same file-backed schemes built from the session header alone (no lock, no recovery, no model; `skill://` over the skills on disk now; no `proc://`, since a closed session has no live jobs). Use it for display; write to a session's files through an open `Session` |
| `SessionStore` / `SessionWriter` | `axo::store::{SessionStore, SessionWriter}` | Pluggable persistence backend for creating, locking, reading (whole sessions or just the header), listing, and appending session entries. Default implementation: `JsonlStore`. Passed via `AxoBuilder::store` |
| `Tool` | `axo_agent::Tool` | Defines a tool (`name`, `description`, JSON Schema `parameters`, `execution_mode`, `replay`, `prompt_guidelines`, and `execute(cx: ToolContext<'_>)`) for `axo_agent::Agent`. Tools hold their own session handles; `cx` carries per-call state. Pass host tools as `SessionOptions::tools` `ToolFactory` closures; axo calls each with the session's `Arc<SessionTools>` (again per fork), so the tool can use `locate`, `schemes`, `rewrite_urls`, the ledger, the `exec` environment, and the `supervisor` for subprocesses. One named like a built-in replaces it in place |
| `LoopHooks` | `axo_agent::LoopHooks` | Lifecycle hooks on `axo_agent::Agent`: `prepare_request` (can update context/system prompt or fail the run), `should_skip_tools` (intercepts unusable responses before any tool runs), `before_tool_call` (can return `ToolCallDecision::Block { reason }`), `after_tool_call` (can modify `ToolOutput`), `finish_turn` (can return `TurnFinish::Stop` or `Continue`), and `steering` / `follow_up` (host-supplied `Vec<QueuedMessage>` drained after the agent's own queues; see [Queued inputs and message origins](#queued-inputs-and-message-origins)) |
| `Persist` | `axo_agent::Persist` | Ordered async subscriber on `axo_agent::Agent` awaited before the next model request (used by `Session` to persist `Entry::Message` on `MessageEnd { message, origin }`) |
| `Api` / `Streamer` | `axo_ai::{Api, Streamer}` | Streaming LLM wire adapter (`AnthropicApi`, `CompletionsApi`, `CodexApi`, or `ScriptedApi` behind the `testing` feature). Custom streamers can be injected via `AxoBuilder::streamer` |
| `CredentialStore` | `axo_ai::CredentialStore` | Loads and saves per-provider `Credential`s (`FileCredentialStore` by default) |

### Implementing `axo_agent::Tool`

When driving `axo_agent::Agent` directly (or adding tools to a custom loop), implement `axo_agent::Tool`:

```rust
use async_trait::async_trait;
use axo_agent::{ExecutionMode, Replay, Tool, ToolContext, ToolError, ToolOutput, ToolUpdate};
use axo_ai::{ToolCall, UserBlock};
use serde_json::{Value, json};

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

    async fn execute(&self, cx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let target = cx
            .call
            .arguments
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError("missing target".into()))?;
        (cx.updates)(ToolUpdate {
            content: vec![UserBlock::Text { text: format!("pinging {target}…") }],
            details: None,
        });
        if cx.cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        Ok(ToolOutput {
            content: vec![UserBlock::Text {
                text: format!("pong from {target}"),
            }],
            ..ToolOutput::default()
        })
    }
}
```

- **`ToolContext<'a>`:** `call: &ToolCall` (the validated call), `cancel: CancellationToken` (the tool must honor it), `updates: &(dyn Fn(ToolUpdate) + Send + Sync)` (partial results while the call runs), `steer: SteerSignal`, and `invocation: ToolInvocationId`.
- **`SteerSignal`:** `is_pending()` is true while steering input is queued and not yet drained into the transcript; `pending().await` resolves when it becomes so (and never if the agent is gone). The built-in `bash` uses it to move a foreground command to the background and `wait` uses it to return early. Results the host injects through `LoopHooks::steering` never set it. `SteerSignal::never()` gives a signal that never fires, for calling `execute` outside an `Agent`.
- **`ToolInvocationId(u64)`:** unique per execution within one `Agent`, assigned right before `execute`. It is the identity reported back as `MessageOrigin::Tool` for that call's persisted result.
- **`ToolOutput`:** `content` (model-facing), `details: Option<Value>` (host/log data, never sent to the model), `is_error` (a failure with output, e.g. a failing command; an `Err(ToolError)` also becomes an error result), and `terminate` (stop after this batch, honored only when every result in the batch sets it).
- **`ExecutionMode::Parallel` vs `ExecutionMode::Sequential`:** `Agent` runs a turn's tool batch concurrently when `AgentConfig::tool_execution` is `Parallel` and every tool in the batch returns `ExecutionMode::Parallel` (preparation and `before_tool_call` always run sequentially in call order, and `ToolResult` messages are always emitted in call order). If any tool in the batch returns `ExecutionMode::Sequential`, the batch runs one call at a time.
- **`Replay::Safe` vs `Replay::Never`:** Controls whether an unfinished tool call is re-executed (`Safe`) or marked interrupted (`Never`) when `Session` recovers a crashed run.

### Session tools and supervised subprocesses

A `ToolFactory` (`Arc<dyn Fn(&Arc<SessionTools>) -> Arc<dyn Tool> + Send + Sync>`) receives the session's `SessionTools`:

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `cwd` | `PathBuf` | Relative paths and commands resolve against it |
| `exec` | `Arc<dyn ExecEnv>` | The command environment (host or dev shell) |
| `ledger` | `FileLedger` | Content hashes of files this session has read or written (the stale-file guard) |
| `context` | `NestedContext` | Nested project instructions attached once to file tool results |
| `schemes` | `Schemes` | The session's scheme registry |
| `questions` | `Arc<PendingQuestions>` | Pending `ask` questions |
| `ask_timeout` | `Option<Duration>` | `ask` auto-selection timeout |
| `ci_env` | `bool` | Whether `bash` sets `CI=true` |
| `supervisor` | `Arc<ProcessSupervisor>` | Runs local commands in supervised containers, process groups or cgroup jobs per the host's `Supervision` (shared by every session of one `Axo`) |
| `jobs` | `Arc<JobRegistry>` | The session's command jobs; every `bash` command registers here while it runs, and background jobs stay listed for `proc://` and result delivery. Its job-mutating methods are crate-internal |

Host tools that run subprocesses should go through `supervisor`, never `std::process::Command`/`tokio::process::Command` directly, so their descendants are contained and killed with the job exactly like `bash` (to the extent the selected mode tracks them):

```rust
use std::sync::Arc;
use std::time::Duration;
use axo::exec::host::ExecSpec;
use axo::{SessionTools, ToolFactory};
use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};
use axo_ai::UserBlock;

pub struct HeadTool {
    tools: Arc<SessionTools>,
    schema: serde_json::Value,
}

#[async_trait::async_trait]
impl Tool for HeadTool {
    fn name(&self) -> &str { "git_head" }
    fn description(&self) -> &str { "Print the current git commit." }
    fn parameters(&self) -> &serde_json::Value { &self.schema }

    async fn execute(&self, cx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let tools = &self.tools;
        let resolved = tools.exec.resolve(&tools.cwd, &cx.cancel, &|_| {}).await;
        let spec = ExecSpec {
            program: "git".into(), // bare names resolve on `env`'s PATH
            args: vec!["rev-parse".into(), "HEAD".into()],
            cwd: tools.cwd.clone(),
            env: (*resolved.env).clone(),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        // Dropping the future on cancel drops the `Process`, which kills its container.
        let output = tokio::select! {
            output = tools.supervisor.output(spec, Some(deadline)) => output.map_err(|err| ToolError(err.to_string()))?,
            () = cx.cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
        };
        Ok(ToolOutput {
            content: vec![UserBlock::Text { text: String::from_utf8_lossy(&output.stdout).into_owned() }],
            is_error: !output.status.success(),
            ..ToolOutput::default()
        })
    }
}

let head: ToolFactory = Arc::new(|tools| {
    Arc::new(HeadTool { tools: tools.clone(), schema: serde_json::json!({ "type": "object" }) })
});
// SessionOptions { tools: vec![head], .. }
```

The supervision API (`axo::exec::host`, with `OutputError`, `ProcessSupervisor` and `Supervision` also re-exported at the crate root):

| Item | Signature | Behavior |
| :--- | :--- | :--- |
| `Supervision` | `enum { ProcessGroup, Cgroup { program: PathBuf, args: Vec<OsString>, cgroup_root: Option<PathBuf> } }` | How commands are contained; `Default` is `ProcessGroup`. In `Cgroup`, axo runs `program args… guard <dir>` and `program args… enter <dir> <command…>`, which the host must route to `axo::exec::supervisor::run`; a bare `program` resolves on axo's own PATH at spawn |
| `ExecSpec` | `{ program: PathBuf, args: Vec<OsString>, cwd: PathBuf, env: EnvMap }` | One command. `env` is the complete environment (`env_clear` semantics); a bare `program` resolves on its `PATH` |
| `ProcessSupervisor::new` | `fn new(supervision: Supervision) -> Arc<Self>` | Checks nothing yet, so hosts that never run commands need no delegated cgroup even in `Cgroup` mode. `Axo::build` creates one per `Axo` with the builder's `Supervision` (default `ProcessGroup`) |
| `ProcessSupervisor::spawn` | `async fn spawn(&self, spec: ExecSpec) -> Result<Process, String>` | Starts `spec` in a fresh container (process group or cgroup job) with stdin closed and stdout/stderr piped. In `Cgroup` mode it probes the root once per `Axo` and fails closed when supervision is unavailable or the supervisor route does not answer (see [Command supervision](configuration.md#command-supervision-axosupervision)); either mode fails when the program cannot start. Cancelling it at any point leaves nothing behind |
| `ProcessSupervisor::output` | `async fn output(&self, spec: ExecSpec, deadline: Option<tokio::time::Instant>) -> Result<std::process::Output, OutputError>` | Like `Command::output`: reads stdout and stderr while waiting for the root, then kills anything left in the container and closes it. Reaching `deadline` kills the job and returns `OutputError::TimedOut`; a spawn or wait failure returns `OutputError::Failed(message)`. `OutputError` implements `Display` (`timed out`, or the message) and `std::error::Error` |
| `Process` | `pub stdout: Option<ChildStdout>`, `pub stderr: Option<ChildStderr>` | A running job. `capture_output()` merges both pipes into one `mpsc::Receiver<Vec<u8>>` (plus the reader `JoinSet`); `wait()` waits for the root (cancel-safe; in cgroup mode a guardian that dies first takes the job with it); `guardian_lost().await` resolves once the cgroup guardian has died, after killing the job (for watching leftovers after the root exited; cancel-safe, resolves at most once, and never in process-group mode, whose guard is best effort); `container()` returns a cloneable `Container`; `close().await` waits until the container is empty, removes the cgroup directory in cgroup mode, and stands the watcher down (call `container().kill()` first to end leftovers). Dropping an unclosed `Process` kills the whole container |
| `Container` | `kill(&self)`, `populated(&self) -> bool`, `async terminate(&self)` | Handle to one job's container: `kill` sends SIGKILL to the process group or writes `cgroup.kill` (errors ignored); `populated` reports whether any process remains (a cgroup that is unreadable counts as empty; a group member that refuses a signal still counts as present); `terminate` sends SIGTERM (to the group, or to each cgroup member through a pidfd), waits up to 1 s for the container to empty, then kills it |

Subprocesses a host tool runs this way are not `bash` jobs: they are never promoted to the background, listed under `proc://`, or delivered later; the tool owns them for the duration of its call.

### Queued inputs and message origins

`axo_agent` knows nothing about jobs; it offers two generic primitives that `Session` builds job delivery on.

- **`QueuedMessage { message: AgentMessage, id: Option<String> }`:** an input waiting to enter the transcript, with the host's id for it (`From<AgentMessage>` gives `id: None`). `Agent::prompt(Vec<QueuedMessage>)`, `Agent::steer(QueuedMessage)`, and `Agent::follow_up(QueuedMessage)` take it; `agent.prompt(vec![message.into()])` keeps the id-less form short. `Agent::steer` also sets the steer signal until the steering queue is drained.
- **`LoopHooks::steering` / `LoopHooks::follow_up`:** `async fn steering(&self) -> Vec<QueuedMessage>` and `async fn follow_up(&self) -> Vec<QueuedMessage>` (both default to empty) supply host messages drained after the agent's own steering or follow-up queue at each drain point: steering after the current tool batch, before the next model request; follow-up when the run would otherwise end. They never set the steer signal. `Session` uses `steering` to inject finished background job results into an active run.
- **`AgentEvent::MessageEnd { message, origin: MessageOrigin }`:** every finished message carries its provenance:

| `MessageOrigin` | Assigned to |
| :--- | :--- |
| `Input(Option<String>)` | A prompted or queued input (including hook-supplied ones), with the id it was queued under |
| `Tool(ToolInvocationId)` | The result returned by executing a tool with that invocation, `Ok` or `Err`, even if cancellation arrived meanwhile |
| `Run` | Everything else: assistant replies, tool-change declarations, hook and `prepare_request` messages, and synthetic tool results (blocked, failed-preparation, unanswered or aborted calls) |

An executed result keeps its `Tool` origin exactly when it is the result persisted. `Session`'s `Persist` maps `Input(Some(id))` to the submission id (or, for a job delivery message, to the jobs it delivers) and `Tool(invocation)` to the jobs that call delivered or started, so receipts never depend on comparing message text (see [Background Job Durability](sessions-and-compaction.md#background-job-durability)).
