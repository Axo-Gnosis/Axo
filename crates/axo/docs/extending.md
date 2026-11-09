# Embedding & Extending axo

`axo` is a library with no configuration directory of its own. Hosts customize it by supplying configuration through `ConfigSource` and resources through `Axo::builder`, and extend it in Rust by embedding `axo`, `axo-agent`, and `axo-ai`. Repositories and users customize sessions through the ecosystem `.agents/` and `AGENTS.md` conventions and whatever the host exposes.

## 1. Configuration and Declarative Files

axo reads the ecosystem files itself; everything else arrives from the host, in whatever names, locations, and formats the host chooses (see [`configuration.md`](configuration.md#configuration-source-axoconfigsource)):

- **Context files (`AGENTS.md`, `AGENTS.local.md`, `CLAUDE.md`, `CLAUDE.local.md`, `.agents/AGENTS.md`, plus any path enabled in `settings.contextFiles`):** Add persona, architecture notes, and coding rules at the repository root or in subdirectories (loaded on demand when `read`, `write`, or `edit` touches that subtree). User-wide files come from the host as `UserConfig::context_files`, and `~/.agents/AGENTS.md` with `homeAgentsDir: true`. See [`prompts-and-context.md`](prompts-and-context.md#context-files-cratesaxosrccontextfilesrs).
- **Prompt overrides (`ConfigLayer::prompts`):** The user and project layers may override `SYSTEM`, `WORK`, `PROSE`, or the summarizer prompts (`SUMMARY`, `COMPACTION`, `COMPACTION_UPDATE`, `COMPACTION_SPLIT_TURN`, `BRANCH_SUMMARY`); the project's override wins per name. An empty `SYSTEM`, `WORK`, or `PROSE` override disables that prompt section, an empty summarizer override keeps the bundled prompt with a `Diagnostic`, and a name outside `PROMPT_NAMES` is an `Error::Config`.
- **Skills (`.agents/skills/<name>/SKILL.md`, host skill roots, and `~/.agents/skills/<name>/SKILL.md`):** Package task instructions and reference files that the model reads on demand from `<available_skills>` or the user invokes inline with `/<name> [args]`. Roots are searched in order: project `.agents/skills` (needs project access and `project.files.skills`), each `UserConfig::skill_roots` entry, then `~/.agents/skills` (only with `homeAgentsDir: true`). Optional `argument-hint` frontmatter is the placeholder apps show after `/<name>`; a skill named like a built-in command (`compact`, `shake`, `model`, `thinking`, `navigate`) gets a warning diagnostic and is left out of the command catalog. See [`prompts-and-context.md`](prompts-and-context.md#skills-cratesaxosrccontextskillsrs).
- **Custom providers and model patches (`axo_ai::ModelsConfig`):** The host's `ModelsConfig` registers OpenAI- or Anthropic-compatible endpoints, custom headers, literal or `$ENV` API keys, and per-model overrides (`contextWindow`, `maxTokens`, `reasoning`, `thinkingLevelMap`, `cost`, `compat`). Config values starting with `!` are rejected; hosts resolve command-based secrets before passing configuration. See [`configuration.md`](configuration.md#models-axo_aimodelsconfig--credentials).
- **Agent definitions:** Supply `*.md` files with YAML frontmatter in `ConfigLayer::agent_roots` (project roots require `Trust`), or `AgentSpec` values through `SessionOptions::agents`. Select a root persona with `SessionOptions::agent`; its body, tool allowlist, context-file switches, and section removals configure the session. See [Agent definitions](prompts-and-context.md#agent-definitions).
- **Model roles:** User-only `modelRoles` maps role names to model selectors. `SessionOptions::model`, agent models, `defaultModel`, and `/model` accept `provider/model`, `@role`, optional `:thinking` suffixes, or `:thinking` alone. See [Model selectors](configuration.md#model-selectors).

Start-time diagnostics for context files, skills, agent definitions and tool allowlists, schemes and bundled docs, prompt overrides, host sections, and reserved command names are available through `session.diagnostics()` and still logged as warnings. Runtime `NestedContext` read errors are only logged; they are not added to that startup snapshot.

## 2. Embedding `axo` in a Rust Host

Hosts link the `axo` crate to manage `Models`, a `SessionStore`, a `ConfigSource`, and `Session` instances in a single process. `Axo::builder(models, store)` takes the two required inputs; `AxoBuilder::config` and `AxoBuilder::cache_dir` are optional (see [Absent inputs](configuration.md#absent-inputs)), and `AxoBuilder::build` is synchronous, infallible, and performs no configuration I/O. The user layer is sampled at every `create_session`, `open_session`, and `Axo::schemes` call; a fork inherits its parent's resolved configuration.

### Building `Axo` and running a `Session`

```rust,no_run
# async fn example() -> Result<(), axo::Error> {
use std::path::{Path, PathBuf};
use std::sync::Arc;
use axo::JsonlStore;
use axo::{
    Answer, Axo, Command, CommandInfo, ConfigLayer, ConfigSource, NavigateOptions, PromptPart,
    PromptRequest, PromptResult, RecoveryPolicy, SessionEvent, SessionOptions, ShakeMode,
    UserConfig,
};
use axo_ai::{FileCredentialStore, Models, ModelsConfig, ModelsOptions};
use serde_json::json;

/// The host's configuration: names, locations, and formats are its own choice.
struct HostConfig;

impl ConfigSource for HostConfig {
    fn user(&self) -> Result<UserConfig, axo::Error> {
        Ok(UserConfig {
            layer: ConfigLayer {
                origin: "my-host user settings".into(),
                settings: Some(json!({ "project": { "access": "read" } })),
                ..ConfigLayer::default()
            },
            skill_roots: vec![PathBuf::from("/var/lib/my-host/skills")],
            context_files: vec![PathBuf::from("/var/lib/my-host/AGENTS.md")],
        })
    }

    fn project(&self, _cwd: &Path) -> Result<ConfigLayer, axo::Error> {
        // Untrusted data: axo validates and filters whatever is returned here.
        Ok(ConfigLayer::default())
    }
}

let state = PathBuf::from("/var/lib/my-host");
let models = Models::load(ModelsOptions {
    http_client: None,
    cache_dir: Some(state.join("cache")),
    extended_context: false,
    config: ModelsConfig::default(),
    credentials: Arc::new(FileCredentialStore::new(state.join("credentials.json"))),
})
.await?;
let axo = Axo::builder(Arc::new(models), Arc::new(JsonlStore::new(state.join("sessions"))))
    .config(Arc::new(HostConfig))
    // Bundled docs (`axo://`) and dev-shell captures; omit to run without them.
    .cache_dir(state.join("cache"))
    // Commands run in process groups by default (`Supervision::ProcessGroup`); see below
    // for the opt-in cgroup mode.
    .build();

let session = axo
    .create_session(SessionOptions {
        cwd: PathBuf::from("/repo/my-project"),
        model: Some("anthropic/claude-sonnet-4-5".into()),
        agent: None,
        agents: Vec::new(),
        thinking: None,
        ask_enabled: true,
        project_access: None,
        exec_env: None,
        sections: vec![(
            "host_context".into(),
            Some("Running inside my-host.".into()),
        )],
        tools: Vec::new(),
        tool_hooks: Vec::new(),
        spawn_hooks: Vec::new(),
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

Local commands (`bash`, dev-shell capture, and host tools using `SessionTools::supervisor`) run under the host-selected `Supervision`, passed to `AxoBuilder::supervision`. The default, `Supervision::ProcessGroup`, requires no host routing or privileges. Hosts selecting `Supervision::Cgroup` route arguments in `main` to `axo::exec::supervisor::run(args)` and run inside a delegated cgroup v2 subtree; without a usable root or route, `bash` fails closed with `Command supervision unavailable` / `Command supervisor … did not report ready`, while dev-shell resolution falls back to the host environment. See [Command supervision](configuration.md#command-supervision-axosupervision) for the synchronous routing `main` example and failure details.

### `Session` control methods

| Method | Signature | Behavior |
| :--- | :--- | :--- |
| `prompt` | `async fn prompt(&self, input: impl Into<PromptInput>) -> Result<RunOutcome, Error>` | Saves the parts (handed-over content under `local://`), expands `/<skill> args`, persists a `Prompt` submission, runs the agent loop to completion, and returns `RunOutcome` (`Done`, `Aborted(CancelReason)`, `Failed(message)`). Never runs a built-in command: `/model x` is ordinary prompt text. Returns `Error::Busy` if a run or maintenance task is already active, `Error::InvalidInput` for a missing referenced file or invalid content name |
| `prompt_with` | `async fn prompt_with(&self, request: PromptRequest) -> Result<PromptResult, Error>` | `PromptRequest { input: PromptInput, request_id: Option<String> }`. Like `prompt`, but deduplicates by `request.request_id`: if a submission with the same `request_id` is already in the log, returns `PromptResult::Duplicate(existing)` without starting a second run |
| `steer` | `async fn steer(&self, input: impl Into<PromptInput>) -> Result<(), Error>` | Persists a `Steer` submission and queues it to be injected after the current batch of tool calls finishes, before the next model request. While it is queued, tools see `ToolContext::steer` pending: a foreground `bash` command moves to the background and `wait` returns. Skills expand here too |
| `follow_up` | `async fn follow_up(&self, input: impl Into<PromptInput>) -> Result<(), Error>` | Persists a `FollowUp` submission and queues it for when the active run would otherwise finish; on an idle session it starts a run. A message arriving after the run's last drain point starts the next run rather than remaining queued. Skills expand here too |
| `abort` | `fn abort(&self)` | Stop (`session.abort()`, without awaiting): cancels active maintenance and stops the current run with `CancelReason::User` (persisting an `Aborted` outcome), including its foreground tool calls and foreground `bash` command. Background jobs keep running. A Stop that interrupted an active run pauses automatic delivery of finished job results until the next `prompt`, `steer`, or `follow_up`; a Stop while idle changes nothing (see [Background Job Durability](sessions-and-compaction.md#background-job-durability)) |
| `shutdown` | `async fn shutdown(&self)` | Cancels with `CancelReason::Shutdown`, waits for the active run without recording it as user-aborted, recursively shuts down live children, then stops command jobs and awaits their tasks. Child runs resume when the parent reopens; commands are marked interrupted. Dropping a `Session` cancels its run, kills command containers, and releases table-owned child handles; independently held children can remain live |
| `set_model` | `async fn set_model(&self, model: Model) -> Result<(), Error>` | Appends `Entry::ModelChange` (and `ThinkingLevelChange` if clamped) and, once written, switches the model for the next request; allowed during maintenance, which it never invalidates |
| `set_thinking` | `async fn set_thinking(&self, level: ThinkingLevel) -> Result<ThinkingLevel, Error>` | Clamps `level` to the current model, appends `Entry::ThinkingLevelChange` when changed (applied once written), and returns the applied level |
| `set_name` | `async fn set_name(&self, name: Option<String>) -> Result<SeqEntry, Error>` | Appends `Entry::SessionInfo` at the current position; replaces newlines with spaces, trims, and treats an empty name as `None`. May run at any time; adds no model-context messages |
| `name` | `fn name(&self) -> Option<String>` | Returns the latest session name in file order, globally rather than scoped to the current branch |
| `append_custom` | `async fn append_custom(&self, custom_type: &str, data: Option<Value>) -> Result<SeqEntry, Error>` | Appends `Entry::Custom` at the current position. May run at any time; adds no model-context messages. Rejects an empty `custom_type` with `Error::InvalidInput` |
| `custom` | `fn custom(&self, custom_type: &str) -> Option<Value>` | Returns the latest data of that type on the current root-to-leaf path (branch-scoped), or `None` if absent or the latest entry has no data |
| `answer` / `dismiss` | `fn answer(&self, call_id: &str, answers: Vec<Answer>) -> Result<(), Error>`<br>`fn dismiss(&self, call_id: &str) -> Result<(), Error>` | `answer` settles the pending question; `dismiss` drops it and stops the run like `abort` (`CancelReason::User`). Both return `Error::NoPendingQuestion` for an unknown call id |
| `attach` | `async fn attach(&self, from: Option<u64>) -> Attach` | Returns `missed` entries after `from`, the current `LiveSnapshot` (including `jobs`, the same list as `Session::jobs`), and a live `broadcast::Receiver<SessionEvent>` |
| `compact` | `async fn compact(&self, focus: Option<String>) -> Result<(), Error>` | Runs a standalone compaction summary checkpoint in a session-owned task; `focus` tells the summary what to prioritize and is recorded on the `Entry::Compaction`; blank focus counts as none |
| `shake` | `async fn shake(&self, mode: ShakeMode) -> Result<(), Error>` | Runs recoverable artifact elision (`Elide`, `Images`, or `Thinking`) without model calls |
| `navigate` | `async fn navigate(&self, target: Option<EntryId>, options: NavigateOptions) -> Result<(), Error>` | Moves the leaf to `target` (`None` = root), optionally summarizing the abandoned branch into an `Entry::BranchSummary` (`NavigateOptions { summarize, focus }`; `focus` without `summarize` is `Error::InvalidInput` (blank counts as none)). Running jobs keep running; their results are delivered on whichever branch is selected when they finish, naming the originating command or child agent |
| `fork` | `async fn fork(&self, entry_id: &EntryId) -> Result<Fork, Error>` | Copies the root-to-`entry_id` path (to its parent when `entry_id` is a user message, returned as `Fork::editable`) into a new session; see [Attaching, Navigating & Forking](sessions-and-compaction.md#attaching-navigating--forking-session) |
| `spawn` | `async fn spawn(&self, request: SpawnRequest) -> Result<ChildHandle, Error>` | Creates a child session, durably admits its parent-registry row, and starts its first prompt asynchronously; see [Spawning child sessions](#spawning-child-sessions) |
| `agent` | `fn agent(&self) -> Option<&SpawnLink>` | The persisted spawn link for a child; `None` for roots and forks. Distinct from the catalog returned by `agents` |
| `reload_exec_env` | `async fn reload_exec_env(&self)` | Invalidates and re-resolves the `ExecEnv` (such as a Nix flake dev shell), queuing a `dev_shell` system-prompt diff if the note changed |
| `commands` | `fn commands(&self) -> Vec<CommandInfo>` | The command catalog a host may offer: the five built-ins, then each skill whose name is not reserved. See [Commands](#commands) |
| `run_command` | `async fn run_command(&self, command: Command) -> Result<(), Error>` | Runs a built-in `Command` under its policy; the only way a built-in runs. `Command::Model` accepts [model selectors](configuration.md#model-selectors), including roles and thinking-only changes |
| `id` / `path` / `skills` | `fn id(&self) -> &str`<br>`fn path(&self) -> &Path`<br>`fn skills(&self) -> &[Skill]` | The header UUIDv7 (known before the file exists); the session file (roots create it on their first submission, non-system message, or child admission; children and forks create it immediately); the skills loaded at start, which feed the prompt catalog and `/name` commands |
| `agents` | `fn agents(&self) -> &[AgentDef]` | Agent and team definitions loaded at start; forks share them. Select a root with `SessionOptions::agent`; a missing name or a team is `Error::AgentNotFound`. Reopening must pass the root name again |
| `diagnostics` | `fn diagnostics(&self) -> &[Diagnostic]` | Start-time diagnostics retained for the host and still logged as warnings; excludes runtime `NestedContext` read errors, which are only logged |
| `schemes` | `fn schemes(&self) -> &Schemes` | This session's URL schemes for hosts resolving URLs they display: `exact`, then `resolve`, then `read` for `Target::Virtual`; a fork resolves to its own files |
| `jobs` | `fn jobs(&self) -> Vec<JobInfo>` | Command and child rows, command namespace first then child namespace, each in numeric order; finished rows included. The same list as `read proc://`; see [Background jobs for hosts](#background-jobs-for-hosts) |
| `kill_job` | `fn kill_job(&self, id: &str) -> bool` | Stops a command row (including retained helpers) or aborts a running child row. Returns `false` for an unknown or invalid id, an ended command, or a child without a live running session |

Prompt input is a `PromptInput`; see [`prompts-and-context.md`](prompts-and-context.md#prompt-parts-cratesaxosrcinputrs) for each `PromptPart` and how parts reach the model.

### Spawning child sessions

`Session::spawn` is the host entry point; the model uses the [spawn tool](prompts-and-context.md#spawning-and-returning-results). Both start the first prompt asynchronously and report each settled run through the parent's job registry. The host call returns a `ChildHandle` after creating and admitting the child, not after it finishes; the tool returns `agent_N`, with admission committed by its tool result. The child has a separate journal and no copy of the parent's transcript. [Child sessions](sessions-and-compaction.md#child-sessions) describes delivery, recovery, parking, and budgets.

| Type | Fields / method | Contract |
| :--- | :--- | :--- |
| `SpawnRequest` | `agent: Option<String>`, `spec: Option<InlineSpec>`, `task: PromptInput`, `model: Option<String>`, `output: Option<Value>` | `agent` selects a catalog agent; `spec` supplies an inline one. They are exclusive; neither means a plain delegate. `task` is the first prompt. The explicit model selector wins over other model sources; `output` overrides the agent's result schema before spawn hooks run |
| `InlineSpec` | `prompt: String`, `tools: Option<Vec<String>>`, `model: Option<String>`, `output: Option<Value>` | Trimmed prompt becomes the agent body. Omitted tools keep the parent's available tools except `ask` and `yield`; a supplied list must be a subset of those tools. The output JSON Schema must compile and is enforced through the child's `yield` tool |
| `ChildHandle` | `job: String`, `session: Arc<Session>` | The parent's persistent child row and a live child handle. Hosts can attach, prompt again after settling, steer, or abort through the session |
| `SpawnHook` | `async fn before_spawn(&self, spawn: &SpawnContext<'_>) -> SpawnDecision` | Ordered host policy, supplied as `SessionOptions::spawn_hooks: Vec<Arc<dyn SpawnHook>>` (empty by default), inherited by children |
| `SpawnContext` | `spec`, `agent`, `model`, `thinking`, `task`, `depth`, `parent_session_id`, `origin: &SpawnOrigin` | Borrowed normalized spec, optional catalog name, resolved model/thinking, first prompt, child depth, parent id, and who requested the spawn |
| `SpawnOrigin` | `Host`, `Tool { call_id: String }` | Distinguishes `Session::spawn` from the model's `spawn` call, letting the same hook apply origin-specific policy |
| `SpawnDecision` | `Allow`, `Block(String)`, `Replace(AgentSpec)` | The first block rejects the spawn. A replacement keeps its own output schema rather than reapplying the request's override; it is normalized, its schema checked, and its model cascade resolved again before the next hook. The catalog name stays unchanged |

Child defaults come from user-only `subagents` settings. Model resolution considers the explicit request, the named agent's settings override, the spec, and the child default, then inherits the parent's live model; see [Child-session settings](configuration.md#child-sessions-subagents--axosubagentsettings). Missing or team agent names return `Error::AgentNotFound`; conflicting agent/spec inputs, unavailable inline tools, and invalid output schemas return `Error::InvalidInput`. A spawn hook, the depth limit, or a shutting-down parent can reject with `Error::SpawnBlocked`.

Children inherit the resolved configuration, cwd, execution environment, supervisor, store, models, streamer, agent catalog, host tool factories (filtered by the child's allowlist), schemes, and tool/spawn hooks. They disable `ask`, omit host prompt sections, and receive fixed delegation framing plus their resolved spec. `SpawnLink::spec` persists the spec after defaults and hooks, so later agent-file edits do not change it.

`Axo::session(&self, id: &str) -> Result<Arc<Session>, Error>` is async and returns a known live child or revives a parked one, resuming unfinished work. Children become known when spawned or when their parent opens in this process; an unknown id returns `Error::SessionNotFound`. `Axo::open_session` rejects child files with `Error::ChildSession`: open their parent and use `Axo::session` instead. `Session::agent()` returns the child's `SpawnLink`, and session listings include `SessionInfo::spawn`.

### `SessionEvent`

Emitted by `attach().events` (`broadcast::Receiver<SessionEvent>`):

| Variant | Fields | Meaning |
| :--- | :--- | :--- |
| `Agent(AgentEvent)` | `AgentEvent` | Passthrough of stateful turn loop events (model streaming, tool call execution, message completions) |
| `Entry(SeqEntry)` | `SeqEntry` | New entry persisted to the session store |
| `QuestionPending` | `call_id: String`, `questions: Vec<Question>` | An `ask` tool call needs user input |
| `QuestionResolved` | `call_id: String` | A pending question was answered or dismissed |
| `MaintenanceStart` | `kind: MaintenanceKind` | Compaction, `/shake`, or branch summary started |
| `MaintenanceEnd` | `kind: MaintenanceKind`, `error: Option<String>`, `aborted: bool` | Maintenance task finished |
| `Retry` | `attempt: u32`, `delay_ms: u64` | Transient model error backoff retry (see [Transient Retry & Overflow Recovery](sessions-and-compaction.md#transient-retry--overflow-recovery)) |
| `Jobs` | `jobs: Vec<JobInfo>` | The full `Session::jobs` list after a command or child row changes, including child admission, run start, and run-message completion |

### Background jobs for hosts

`bash` commands that move to the background become jobs `bg_N` (see [Background Jobs](prompts-and-context.md#background-jobs-cratesaxosrcjobsrs)); spawned children have persistent rows `agent_N`. The model reaches both through `proc://`; hosts get the same state as data without parsing that text:

- **State:** `Session::jobs()` returns `Vec<JobInfo>`, and `attach().live` carries the same list as `LiveSnapshot::jobs`.
- **Changes:** every change emits `SessionEvent::Jobs { jobs }` with the full list; replace the previous snapshot rather than patching it.
- **Stop:** `Session::kill_job(id)` stops a job; the result is then journaled and delivered like any other job result.

`JobInfo` (`axo::JobInfo`) has `id: String`, `status: JobStatus`, and `kind: JobKind`. Match the kind for row-specific fields:

| `JobKind` | Fields | Meaning |
| :--- | :--- | :--- |
| `Command` | `command: String`, `artifact: u64` | Full command and output spool, readable as `artifact://<artifact>` |
| `Child` | `session_id: String`, `agent: Option<String>`, `model: String` | Child session id, optional catalog agent, and expanded provider/model selected at spawn |

`JobStatus` (`axo::JobStatus`) is `Running`, `HelpersRunning` (commands only: retained supervised processes), `Finished`, or `Failed`. A child is running while its run is active; otherwise its latest message determines success/failure (no message means failed). `JobStatus::label` returns the `proc://` wording (`running`, `helpers running`, `finished`, `failed`); `agent://` uses the same child labels, with no separate idle state. `Axo::schemes` has neither `proc://` nor `agent://`; only open sessions expose these live schemes. The [agent scheme](prompts-and-context.md#agent) also reads results and projected transcripts and sends follow-ups. Parent `shutdown` also shuts down children, while dropping a parent need not stop a child held independently by the host.

### Commands

`axo` owns the command catalog, argument validation, the `/name args` text parser, and dispatch. Hosts decide which commands to offer and in what form: a slash menu, buttons, a model picker, or nothing. A built-in only runs when the host calls `run_command`, so a host never inherits a command it does not expose. App-only commands (opening settings, a new chat) stay in the app.

| Command | Text form | `CommandPolicy` | Effect |
| :--- | :--- | :--- | :--- |
| `compact` | `/compact [focus…]` | `WhenIdle` | `compact(focus)` |
| `shake` | `/shake <elide\|images\|thinking>` | `WhenIdle` | `shake(mode)` |
| `model` | `/model <selector>` | `Alongside` | Select a model and/or thinking level using the [model selector grammar](configuration.md#model-selectors) |
| `thinking` | `/thinking <level>` | `Alongside` | `set_thinking(level)` |
| `navigate` | `/navigate <entry> [true\|false] [focus…]` | `WhenIdle` | `navigate(Some(entry), NavigateOptions { summarize, focus })`; focus requires `true` |
| each skill | `/<name> <free text>` | `Queued` | Prompt text: expanded, then sent through whichever of `prompt`, `steer`, or `follow_up` the app used |

- **Policies:** `Alongside` commands run during a turn or maintenance and take effect from the next request; `WhenIdle` commands return `Error::Busy` during a turn or maintenance.
- **Typed commands:** a host that accepts typed `/name args` text calls `Command::parse(text)` before prompting. It returns `None` for text that does not start with `/<builtin>` (including skills and unknown `/word`s, which stay prompt text), and otherwise the decoded `Command` for `run_command`. Positional words map to the schema's properties in order, honoring single and double quotes; a final property with `"format": "text"` (the `focus` of `compact` and `navigate`) takes the rest of the line verbatim and trimmed, quotes and apostrophes included, and blank focus counts as none. Invalid arguments return `Error::InvalidCommand { command, message }` (`/shake bogus` → `/shake: unknown variant …`). A built-in run this way is never saved as a submission or sent as a user prompt (though `compact` and summarizing `navigate` may request a summary from the model).
- **Catalog JSON:** `CommandInfo` serializes as `{ "name", "description", "policy": "alongside" | "when_idle" | "queued", "kind": "builtin", "parameters" }` or `{ …, "kind": "skill", "argumentHint" }`. The Rust representation is `CommandInfo { name, description, policy: CommandPolicy, kind: CommandKind }`, with `CommandKind::Builtin { parameters: Value }` or `CommandKind::Skill { argument_hint: Option<String> }` (flattened into JSON as `kind`). `parameters` is an inlined JSON Schema object (draft 2020-12, no `$ref`, `$defs`, `$schema`, or `title`); string properties with `"format": "model"` or `"format": "entry"` tell hosts to offer a model list or a session-tree picker, or to leave the command out where the app already has that control, and `"format": "text"` marks free-form guidance that hosts should give a wide input. `Command::decode(name, args)` validates JSON arguments (`null` counts as `{}`).

## 3. Rust Extension Traits

| Trait | Crate | Purpose |
| :--- | :--- | :--- |
| `ConfigSource` | `axo::ConfigSource` | Supplies configuration: `user()` returns the trusted `UserConfig` (sampled at every `create_session`, `open_session`, and `Axo::schemes`), `project(cwd)` the untrusted project `ConfigLayer` (called only at `Read` or `Trust`, and only when the user's `project.files.config` is `true`). axo validates, merges, and filters both through `EffectiveConfig::resolve`. Passed via `AxoBuilder::config`; without one, sessions use default settings and no project layer. See [Configuration source](configuration.md#configuration-source-axoconfigsource) |
| `ExecEnv` | `axo::exec::ExecEnv` | Resolves the environment (`EnvMap`, optional `prompt_note`, optional one-time `notice`) for `bash` tool calls. Built-in implementations: `DevShellEnv` and `StaticEnv`; `StaticEnv::host()` snapshots the host environment. An `Arc<EnvMap>` converts to `Resolved` with no notice or prompt note. Passed via `SessionOptions::exec_env`. `DevShellEnv::new(base, settings, cache_dir: Option<PathBuf>, supervisor: Arc<ProcessSupervisor>)` runs its direnv, git, nix and capture subprocesses through that supervisor (a `None` `cache_dir` makes Nix mode resolve to `base`); `ExecEnv` only resolves the environment, and every command still runs supervised |
| `Scheme` | `axo::Scheme` | A URL scheme (`name://path`) the file tools accept as a path. `resolve` maps a URL to `Target::File { path, writable }` (the normal file pipeline runs on it; bash rewrites whole-word, `--flag=`/`NAME=`, and redirect-target URLs to that path) or `Target::Virtual` (the scheme's own `read`/`write` handle it). `edit`/`write` refuse non-writable files (`{url} is read-only`); bash refuses virtual URLs (`Cannot use {url} in bash: no file backs it. Use read {url}.`) and output redirects onto read-only files. `prompt_doc` (default empty) is the one-line bullet listed in the model's `schemes` prompt section. Host scheme names must match `^[a-z][a-z0-9+.-]+$` (lowercase, two or more characters); `artifact` and `file` are reserved (`Error::Scheme`). `write_replay` defaults to `Replay::Never`. Built-ins: `artifact://`, `skill://`, `local://`, `axo://` (only when the host set `AxoBuilder::cache_dir`), and in open sessions `proc://` (background jobs; see [Background Jobs](prompts-and-context.md#background-jobs-cratesaxosrcjobsrs)). Passed via `SessionOptions::schemes`; a host scheme named like a built-in replaces it. Hosts resolve URLs they display through `Session::schemes()` or `Axo::schemes(path, project_access, host_schemes)` |
| `SessionStore` / `SessionWriter` | `axo::{SessionStore, SessionWriter}` | Pluggable persistence backend for creating, locking, reading (whole sessions or just the header), listing, and appending session entries. `SessionWriter::path` is known before the file exists; `append` must complete durable storage for `Entry::requires_sync` entries before returning. Bundled implementation: `JsonlStore`. Passed as the required `store` argument of `Axo::builder` |
| `Tool` | `axo_agent::Tool` | Defines a tool (`name`, `description`, JSON Schema `parameters`, `execution_mode`, `replay`, `prompt_guidelines`, and `execute(cx: ToolContext)`) for `axo_agent::Agent`. `execution_mode` defaults to `Parallel`, `replay` to `Never`, `prompt_guidelines` to empty. Tools hold their own session handles; `cx` carries per-call state. Pass host tools as `SessionOptions::tools` `ToolFactory` closures; axo calls each with the session's `Arc<SessionTools>` (again per fork), so the tool can use `locate`, `schemes`, `rewrite_urls`, the ledger, the `exec` environment, and the `supervisor` for subprocesses. One named like a built-in replaces it in place |
| `ToolHooks` | `axo_agent::ToolHooks` | Host tool policy, supplied as an ordered `Vec<Arc<dyn ToolHooks>>` via `SessionOptions::tool_hooks` (empty by default; inherited by forks and children), or directly via `AgentConfig::tool_hooks`. Children's internal turn-budget hook runs before host hooks. `before_tool_call` receives `BeforeToolCallContext` and returns `ToolCallDecision`; the first `Block { reason, terminate }` or hook error blocks the call and skips later hooks. `after_tool_call` receives `AfterToolCallContext` and returns `AfterToolCallResult` overrides; each hook sees the result after earlier overrides, and errors are logged and skipped |
| `SpawnHook` | `axo::SpawnHook` | Child-creation policy via `SessionOptions::spawn_hooks`; returns `SpawnDecision` from the resolved `SpawnContext`. See [Spawning child sessions](#spawning-child-sessions) |
| `LoopHooks` | `axo_agent::LoopHooks` | The loop driver's contract, implemented by `Session` itself rather than supplied through session options. `prepare_request(&AgentContext, cancel)` returns `Result<Option<Vec<AgentMessage>>, HookError>` to optionally replace messages; `convert_to_llm` lowers `Custom` messages (default drops them); `should_skip_tools` intercepts unusable responses; `finish_turn` returns `TurnFinish { continue_turn: bool, messages: Option<Vec<AgentMessage>> }`; `prepare_next_turn` returns `Vec<AgentMessage>` appended with normal events and `MessageOrigin::Run`; `steering` supplies queued host input (see [Queued inputs and message origins](#queued-inputs-and-message-origins)). Model, thinking, and tools remain controlled through `Agent::set_model`, `Agent::set_thinking`, and `Agent::set_tools` |
| `Persist` | `axo_agent::Persist` | The agent's only event sink: ordered, awaited `persist(&self, event: &AgentEvent)` observes immutable events and cannot rewrite messages. `Session` uses it to persist `Entry::Message` on `MessageEnd { message, origin }` before the next request, maintain live run state, and relay session events. The agent has no broadcast or snapshot API; clients attach to `Session` |
| `Streamer` | `axo_ai::Streamer` | Host streaming extension, injected via `AxoBuilder::streamer` for tests and custom transports. `Models` implements it with provider dispatch; `ScriptedApi` implements it behind the `testing` feature |
| `Api` | `axo_ai::Api` | Wire adapter contract that `Models` dispatches to: `AnthropicApi`, `CompletionsApi`, or `CodexApi` |
| `CredentialStore` | `axo_ai::CredentialStore` | Loads and saves per-provider `Credential`s (`FileCredentialStore` over a host-named file is bundled). Supports `get`, `set`, `remove`, and `update` (runs a transform under the store's cross-process file lock). Injected via `ModelsOptions::credentials` into the `Models` the host passes to `Axo::builder` |

For deterministic streaming tests, `axo_ai::testing::ScriptedApi` records requests and serves scripted turns. `ScriptedApi::lane(&self, model_id: &str, turns: Vec<ScriptedTurn>)` appends a dedicated FIFO for that exact model id (not provider/model). Requests without a lane use the shared FIFO; an exhausted lane returns “script exhausted” rather than falling back to that queue. Separate child model ids keep concurrently running parent/child scripts independent.

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

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let target = cx
            .call
            .arguments
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError("missing target".into()))?;
        cx.updates.send(ToolUpdate {
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

- **`ToolContext`:** owned per-call state, because each call runs on its own Tokio task: `call: Arc<ToolCall>` (the validated call), `cancel: CancellationToken` (the tool must honor it; see the task lifecycle below), `updates: ToolUpdates` (partial results while the call runs), `steer: SteerSignal`, and `invocation: ToolInvocationId`.
- **Task lifecycle:** the loop never polls a tool future itself, so a tool may await anything, including state the host's `Persist` holds. Calls start only after the batch's preparation and `ToolHooks::before_tool_call` decisions complete. When the run is stopped or shut down, every unfinished call's `cancel` fires and the loop keeps waiting; a call still running 3 s later is aborted and answered with an `Operation aborted` error result. A panicking call becomes an error result. Cleanup that must happen however the call ends belongs in a `Drop` guard. `ToolHooks::after_tool_call` runs on the loop after the task returns.
- **`ToolUpdates`:** `send(update)` never blocks; updates sent before `execute` returns are emitted before that call's `ToolExecutionEnd`, whose `result.is_error` reports failure. Clonable, so spawned helpers can stream too. `ToolUpdates::new(f)` streams to a closure and `ToolUpdates::sink()` discards updates, for calling `execute` outside an `Agent`.
- **`SteerSignal`:** `is_pending()` is true while steering input is queued and not yet drained into the transcript; `pending().await` resolves when it becomes so (and never if the agent is gone). The built-in `bash` uses it to move a foreground command to the background and `wait` uses it to return early. Results the host injects through `LoopHooks::steering` never set it. `SteerSignal::never()` gives a signal that never fires, for calling `execute` outside an `Agent`.
- **`ToolInvocationId(u64)`:** unique per execution within one `Agent`, assigned right before `execute`. It is the identity reported back as `MessageOrigin::Tool` for that call's persisted result.
- **`ToolOutput`:** `content` (model-facing), `details: Option<Value>` (host/log data, never sent to the model), `is_error` (a failure with output, e.g. a failing command; an `Err(ToolError)` also becomes an error result), and `terminate` (stop after this batch, honored only when every result in the batch sets it).
- **`ExecutionMode::Parallel` vs `ExecutionMode::Sequential`:** `Agent` runs a turn's tool batch concurrently when every tool in the batch returns `ExecutionMode::Parallel` (preparation and `ToolHooks::before_tool_call` always run sequentially in call order, and `ToolResult` messages are always emitted in call order). If any tool in the batch returns `ExecutionMode::Sequential`, the batch runs one call at a time. Execution mode is controlled only by tools, not by agent configuration.
- **`Replay::Safe` vs `Replay::Never`:** Controls whether an unfinished tool call is re-executed (`Safe`) or marked interrupted (`Never`) when `Session` recovers a crashed run. `replay(&self, call: &ToolCall)` decides per call; `write`, for example, is `Safe` for plain paths and defers to the scheme's `write_replay` for scheme URLs.

### Session tools and supervised subprocesses

A `ToolFactory` (`Arc<dyn Fn(&Arc<SessionTools>) -> Arc<dyn Tool> + Send + Sync>`) receives the session's `SessionTools`:

| Field or method | Type | Meaning |
| :--- | :--- | :--- |
| `cwd` | `PathBuf` | Relative paths and commands resolve against it |
| `exec` | `Arc<dyn ExecEnv>` | The command environment (host or dev shell) |
| `ledger` | `FileLedger` | Content hashes of files this session has read or written (the stale-file guard) |
| `context` | `NestedContext` | Nested project instructions attached once to file tool results |
| `schemes` | `Schemes` | The session's scheme registry |
| `supervisor` | `Arc<ProcessSupervisor>` | Runs local commands in supervised containers, process groups or cgroup jobs per the host's `Supervision` (shared by every session of one `Axo`) |
| `run_command` | `async fn run_command(&self, cx: ToolContext, request: CommandRequest) -> Result<ToolOutput, ToolError>` | Runs a supervised command with output capture, background promotion, retained helpers, and exactly-once result delivery; background jobs are listed by `Session::jobs` and `proc://` |

`axo::tools::CommandRequest` has `spec: ExecSpec`, `label: String` (the command shown in job listings), `timeout: Option<Duration>` (no default deadline), and `run_async: bool` (promote immediately). The runner applies the non-interactive environment overlay and, when `bash.ci` is enabled, `CI=true` after the environment supplied in `spec`. Output is spooled as `<n>.<tool>.<escaped call id>.log`, using `ToolContext::call` for the tool name and call id.

File-mutating host tools must follow the same invariants as built-ins: resolve targets with `SessionTools::locate` (`Located::File { url, path, writable }` or `Located::Virtual { url }`; plain filesystem paths have no URL and are writable), verify `writable`, hold `axo::tools::lock_file(path)` throughout the read-modify-write cycle, check existing bytes with `FileLedger::check_fresh` on `SessionTools::ledger`, and record the written hash via `ledger.record`.
Host tools that run commands should use `SessionTools::run_command` to share `bash`'s job lifecycle. All subprocesses must go through the session's `ProcessSupervisor`, directly or via this runner, never `std::process::Command`/`tokio::process::Command` directly, so descendants stay contained to the extent the selected supervision mode tracks them:

```rust
use std::sync::Arc;
use std::time::Duration;
use axo::exec::host::ExecSpec;
use axo::tools::CommandRequest;
use axo::{SessionTools, ToolFactory};
use axo_agent::{Tool, ToolContext, ToolError, ToolOutput};

pub struct HeadTool {
    tools: Arc<SessionTools>,
    schema: serde_json::Value,
}

#[async_trait::async_trait]
impl Tool for HeadTool {
    fn name(&self) -> &str { "git_head" }
    fn description(&self) -> &str { "Print the current git commit." }
    fn parameters(&self) -> &serde_json::Value { &self.schema }

    async fn execute(&self, cx: ToolContext) -> Result<ToolOutput, ToolError> {
        let tools = &self.tools;
        let resolved = tools.exec.resolve(&tools.cwd, &cx.cancel, &|_| {}).await;
        let spec = ExecSpec {
            program: "git".into(), // bare names resolve on `env`'s PATH
            args: vec!["rev-parse".into(), "HEAD".into()],
            cwd: tools.cwd.clone(),
            env: (*resolved.env).clone(),
        };
        tools.run_command(cx, CommandRequest {
            spec,
            label: "git rev-parse HEAD".into(),
            timeout: Some(Duration::from_secs(10)),
            run_async: false,
        }).await
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
| `ProcessSupervisor::new` | `fn new(supervision: Supervision) -> Arc<Self>` | Checks nothing yet, so hosts that never run commands need no delegated cgroup even in `Cgroup` mode. `AxoBuilder::build` creates one per `Axo` with the builder's `Supervision` (default `ProcessGroup`) |
| `ProcessSupervisor::spawn` | `async fn spawn(&self, spec: ExecSpec) -> Result<Process, String>` | Starts `spec` in a fresh container (process group or cgroup job) with stdin closed and stdout/stderr piped. In process-group mode an unstartable program returns `Err`; in cgroup mode `spawn` succeeds and the root exits 127 with `axo supervisor: cannot run …` on stderr (fails closed when supervision is unavailable; see [Command supervision](configuration.md#command-supervision-axosupervision)). Cancelling leaves nothing behind |
| `ProcessSupervisor::output` | `async fn output(&self, spec: ExecSpec, deadline: Option<tokio::time::Instant>) -> Result<std::process::Output, OutputError>` | Like `Command::output`: reads stdout and stderr while waiting for the root, then kills anything left in the container and closes it. Reaching `deadline` kills the job and returns `OutputError::TimedOut`; a spawn or wait failure returns `OutputError::Failed(message)`. `OutputError` implements `Display` (`timed out`, or the message) and `std::error::Error` |
| `Process` | `pub stdout: Option<ChildStdout>`, `pub stderr: Option<ChildStderr>` | A running job. `capture_output()` merges both pipes into one `mpsc::Receiver<Vec<u8>>` (plus the reader `JoinSet`); `wait()` waits for the root (cancel-safe; in cgroup mode a guardian that dies first takes the job with it); `guardian_lost().await` resolves once the cgroup guardian has died, after killing the job (for watching leftovers after the root exited; cancel-safe, resolves at most once, and never in process-group mode, whose guard is best effort); `container()` returns a cloneable `Container`; `close().await` waits until the container is empty, removes the cgroup directory in cgroup mode, and stands the watcher down (call `container().kill()` first to end leftovers). Dropping an unclosed `Process` kills the whole container |
| `Container` | `kill(&self)`, `populated(&self) -> bool`, `async terminate(&self)` | Handle to one job's container: `kill` sends SIGKILL to the process group or writes `cgroup.kill` (errors ignored); `populated` reports whether any process remains (a cgroup that is unreadable counts as empty; a group member that refuses a signal still counts as present); `terminate` sends SIGTERM (to the group, or to each cgroup member through a pidfd), waits up to `KILL_GRACE` (1 s) for the container to empty, then kills it |
| `KILL_GRACE` | `const KILL_GRACE: Duration` | Fixed 1 s grace period between SIGTERM and SIGKILL used by `Container::terminate` |
| `find_on_path` | `fn find_on_path(name: &str, env: &EnvMap, cwd: &Path) -> Option<PathBuf>` | Path search helper resolving a command against `env["PATH"]` and `cwd` |

Commands run through `SessionTools::run_command` have the same background behavior as `bash`: immediate promotion with `run_async`, timed promotion controlled by `bash.promoteAfterMs`, or promotion when a steer is pending; retained helpers remain supervised. Their jobs appear in `Session::jobs` and `proc://`, and results are delivered exactly once through `wait` or a later message. Hosts needing a strictly call-scoped subprocess can use `ProcessSupervisor::output` directly; those subprocesses are not registered as session jobs or promoted.

### Queued inputs and message origins

`axo_agent` knows nothing about jobs; it offers two generic primitives that `Session` builds job delivery on.

- **`QueuedMessage { message: AgentMessage, id: Option<String> }`:** an input waiting to enter the transcript, with the host's id for it (`From<AgentMessage>` gives `id: None`). `Agent::prompt(Vec<QueuedMessage>)`, `Agent::steer(QueuedMessage)`, and `Agent::follow_up(QueuedMessage)` take it; `agent.prompt(vec![message.into()])` keeps the id-less form short. `Agent::steer` also sets the steer signal until the steering queue is drained.
- **`LoopHooks::steering`:** `async fn steering(&self) -> Vec<QueuedMessage>` (default empty) supplies host messages drained after the agent's own steering queue, after the current tool batch and before the next model request. It never sets the steer signal. `Session` uses it to inject finished background job results into an active run. Follow-up input comes only from the agent's follow-up queue, drained when the run would otherwise end. `Agent::queued()` returns the queued messages as `(Vec<AgentMessage>, Vec<AgentMessage>)` in steering, follow-up order.
- **`AgentEvent::MessageEnd { message, origin: MessageOrigin }`:** every finished message carries its provenance:

| `MessageOrigin` | Assigned to |
| :--- | :--- |
| `Input(Option<String>)` | A prompted or queued input (including hook-supplied ones), with the id it was queued under |
| `Tool(ToolInvocationId)` | The result of a tool call that started executing with that invocation: what it returned (`Ok` or `Err`), even if cancellation arrived meanwhile, or the aborted result when Stop ended its task |
| `Run` | Everything else: assistant replies, tool-change declarations, messages returned by `prepare_next_turn`, and synthetic tool results (blocked, failed-preparation, unanswered, or aborted before they started) |

An executed result keeps its `Tool` origin exactly when it is the result persisted. `Session`'s `Persist` maps `Input(Some(id))` to the submission id (or, for a job delivery message, to the jobs it delivers) and `Tool(invocation)` to the jobs that call delivered or started, so receipts never depend on comparing message text (see [Background Job Durability](sessions-and-compaction.md#background-job-durability)).
