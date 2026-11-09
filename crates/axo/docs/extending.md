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
| `steer` | `async fn steer(&self, input: impl Into<PromptInput>) -> Result<(), Error>` | Persists a `Steer` submission and queues it after the current tool batch, before the next model request. On an idle session it starts a run; input after the last drain point starts the next run. While queued, tools see `ToolContext::steer` pending: foreground `bash` promotes and `wait` returns. Skills expand here too |
| `follow_up` | `async fn follow_up(&self, input: impl Into<PromptInput>) -> Result<(), Error>` | Persists a `FollowUp` submission and queues it for when the active run would otherwise finish; on an idle session it starts a run. A message arriving after the run's last drain point starts the next run rather than remaining queued. Skills expand here too |
| `post` | `async fn post(&self, message: HostMessage) -> Result<(), Error>` | Persists host text with custom-message metadata and queues it under `HostDelivery`; bypasses skill expansion. An empty `custom_type` or blank `text`, or the reserved type `job_result`, returns `Error::InvalidInput`. See [Host messages](#host-messages) |
| `abort` | `fn abort(&self)` | Stop (`session.abort()`, without awaiting): cancels active maintenance and stops the current run with `CancelReason::User` (persisting an `Aborted` outcome), including foreground tool calls and `bash`. Background jobs keep running. A Stop during an active run pauses automatic row-message delivery until user input, a host `Steer` / `FollowUp`, or incoming teammate mail; a host `Quiet` message does not clear the pause. A Stop while idle changes nothing (see [Background Job Durability](sessions-and-compaction.md#background-job-durability)) |
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
| `spawn_team` | `async fn spawn_team(&self, request: TeamRequest) -> Result<TeamHandle, Error>` | Creates and admits a catalog or ad-hoc team's children and starts their first prompts asynchronously; see [Spawning child sessions](#spawning-child-sessions) |
| `agent` | `fn agent(&self) -> Option<AgentSpec>` | The current root persona, or for a child the spec it was spawned with; distinct from the catalog returned by `agents`. `spawn_link` returns a child's full persisted link |
| `spawn_link` | `fn spawn_link(&self) -> Option<&SpawnLink>` | The persisted spawn link for a child; `None` for roots and forks |
| `set_agent` | `async fn set_agent(&self, name: Option<&str>) -> Result<(), Error>` | Resolves a root-capable catalog persona, persists `Entry::AgentChange`, and rebuilds the next request's prompt and tools from the unfiltered base; `None` clears the persona. When the persona declares a `model` selector, it is resolved first (errors before any write) and applied with the `agent_change` in one batch, clamping thinking; otherwise model and thinking are unchanged. A missing/team name returns `Error::AgentNotFound`; a subagent-only persona returns `Error::AgentNotRoot` |
| `set_section` | `async fn set_section(&self, name: &str, body: Option<String>) -> Result<(), Error>` | Replaces or removes a host prompt section in the live base and rebuilds the effective shape. Host section overrides apply after persona section switches. An invalid section name returns `Error::InvalidInput` |
| `set_tools` | `async fn set_tools(&self, tools: Vec<ToolFactory>) -> Result<(), Error>` | Replaces host tool factories, constructs this session's tools, and rebuilds the effective tools under the current persona allowlist. Future children and forks use the current factories |
| `model` / `thinking` / `config` | `fn model(&self) -> Model`<br>`fn thinking(&self) -> ThinkingLevel`<br>`fn config(&self) -> &EffectiveConfig` | The live model and thinking selection, and the session's resolved configuration |
| `context_usage` | `fn context_usage(&self) -> Option<ContextUsage>` | Current context estimate and model limits, also available in `LiveSnapshot::context`; distinct from cumulative token spend. See [Context usage](#context-usage) |
| `reload_exec_env` | `async fn reload_exec_env(&self)` | Invalidates and re-resolves the `ExecEnv` (such as a Nix flake dev shell), queuing a `dev_shell` system-prompt diff if the note changed |
| `commands` | `fn commands(&self) -> Vec<CommandInfo>` | The command catalog a host may offer: the five built-ins, then each skill whose name is not reserved. See [Commands](#commands) |
| `run_command` | `async fn run_command(&self, command: Command) -> Result<(), Error>` | Runs a built-in `Command` under its policy; the only way a built-in runs. `Command::Model` accepts [model selectors](configuration.md#model-selectors), including roles and thinking-only changes |
| `id` / `path` / `skills` | `fn id(&self) -> &str`<br>`fn path(&self) -> &Path`<br>`fn skills(&self) -> &[Skill]` | The header UUIDv7 (known before the file exists); the session file (roots and children create it on their first submission, non-system message, or child admission, so a child's file exists once `spawn` returns; forks create it immediately); the skills loaded at start, which feed the prompt catalog and `/name` commands (a child reuses its parent's skills and `axo://` docs instead of loading them again) |
| `agents` | `fn agents(&self) -> &[AgentDef]` | Agent and team catalog loaded at start; forks share it. `SessionOptions::agent` selects a root at creation. Reopening re-resolves the last recorded persona by name, including an explicit clear, unless options explicitly select another root; a recorded persona that is now missing or subagent-only yields a diagnostic and no persona |
| `diagnostics` | `fn diagnostics(&self) -> &[Diagnostic]` | Start-time diagnostics retained for the host and still logged as warnings; excludes runtime `NestedContext` read errors, which are only logged |
| `schemes` | `fn schemes(&self) -> &Schemes` | This session's URL schemes: `exact` parses a URL, `resolve` exposes its target, and `read` reads UTF-8 text from file or virtual targets; a fork resolves to its own files. For binary or ranged reads, resolve and use file I/O |
| `jobs` | `fn jobs(&self) -> Vec<JobInfo>` | Command and child rows, command namespace first then child namespace, each in numeric order; finished rows included. The same list as `read proc://`; see [Background jobs for hosts](#background-jobs-for-hosts) |
| `kill_job` | `fn kill_job(&self, id: &str) -> bool` | Stops a command row (including retained helpers) or aborts a running child row. Returns `false` for an unknown or invalid id, an ended command, or a child without a live running session |
| `command_output` | `fn command_output(&self, call_id: &str) -> Option<CommandOutput>` | Opens a command's live merged output spool, including an unlisted foreground command; unavailable before spawn, after the row/spool disappears, or on open failure |
| `background` | `fn background(&self, call_id: &str) -> bool` | Requests host promotion of a matching foreground command. Returns false if none matches; a command that finishes first remains foreground |

Prompt input is a `PromptInput`; see [`prompts-and-context.md`](prompts-and-context.md#prompt-parts-cratesaxosrcinputrs) for each `PromptPart` and how parts reach the model.

The persona follows the live selection across navigation: `navigate` re-records it on the new branch, so reopening restores the last live persona. Switching away from a restrictive persona restores tools and sections from the live unfiltered base rather than cumulatively filtering the prior shape. These setters may be called mid-run; changes apply from the next model request. Child sessions are rejected with `Error::InvalidInput`. `set_agent` is durable; host sections and tool factories are runtime resources, so hosts must re-supply them through `SessionOptions` when reopening. Forks inherit the current persona, host sections, and host factories. Changing the system prompt or tools is cache-heavy on models without mid-conversation system-message and tool changes; use `post` for instructions that toggle often.

### Spawning child sessions

`Session::spawn` is the host entry point for one child; `Session::spawn_team` starts a team. The model uses the [spawn tool](prompts-and-context.md#spawning-and-returning-results). All start first prompts asynchronously and report each settled run through the parent's job registry. Host calls return handles after creating and admitting the children, not after they finish; tool admission commits with its result. Each child has a separate journal and no copy of the parent's transcript. [Child sessions](sessions-and-compaction.md#child-sessions) describes delivery, recovery, parking, and budgets.

| Type | Fields / method | Contract |
| :--- | :--- | :--- |
| `SpawnRequest` | `agent: Option<String>`, `spec: Option<InlineSpec>`, `task: PromptInput`, `model: Option<String>`, `output: Option<Value>` | `agent` selects a catalog agent; `spec` supplies an inline one. They are exclusive; neither means a plain delegate. `task` is the first prompt. The explicit model selector wins over other model sources; `output` overrides the agent's result schema before spawn hooks run |
| `InlineSpec` | `prompt: String`, `tools: Option<Vec<String>>`, `model: Option<String>`, `output: Option<Value>` | Trimmed prompt becomes the agent body. Omitted tools keep the parent's available tools except `ask` and `yield`; a supplied list must be a subset of those tools. The output JSON Schema must compile and is enforced through the child's `yield` tool |
| `ChildHandle` | `job: String`, `session: Arc<Session>` | The parent's persistent child row and a live child handle. Hosts can attach, prompt again after settling, steer, or abort through the session |
| `TeamRequest` | `team: Option<String>`, `members: Vec<TeamMemberRequest>`, `charter: Option<String>`, `tasks: IndexMap<String, PromptInput>` | Select either a catalog team or a non-empty ad-hoc roster. Every roster name requires a task; extra keys fail. Unlike the tool, this API has no shared task fallback or context parameter: prepare every member's input explicitly |
| `TeamMemberRequest` | `name: String`, `agent: Option<String>`, `spec: Option<InlineSpec>`, `brief: Option<String>` | Ordered ad-hoc roster entry. Agent and spec are exclusive; neither means a plain delegate. The brief is standing instruction text, not the member's task |
| `TeamHandle` | `team: String`, `members: Vec<(String, ChildHandle)>` | Per-parent team id (team_N) and member names with child handles in roster order |
| `SpawnHook` | `async fn before_spawn(&self, spawn: &SpawnContext<'_>) -> SpawnDecision` | Ordered host policy, supplied as `SessionOptions::spawn_hooks: Vec<Arc<dyn SpawnHook>>` (empty by default), inherited by children |
| `SpawnContext` | `spec`, `agent`, `model`, `thinking`, `task`, `depth`, `parent_session_id`, `origin: &SpawnOrigin`, `team: Option<SpawnTeam<'_>>` | Borrowed normalized spec, optional catalog agent name, resolved model/thinking, first prompt, child depth, parent id, requester and optional team identity |
| `SpawnTeam` | `name: Option<&str>`, `member: &str` | Catalog team name (`None` for ad-hoc teams) and roster member name, supplied to hooks for each member |
| `SpawnOrigin` | `Host`, `Tool { call_id: String }` | Distinguishes `Session::spawn` / `Session::spawn_team` from the model's `spawn` call, letting the same hook apply origin-specific policy |
| `SpawnDecision` | `Allow`, `Block(String)`, `Replace(AgentSpec)` | The first block rejects the spawn. A replacement keeps its own output schema rather than reapplying the request's override; it is normalized, its schema checked, and its model cascade resolved again before the next hook. The catalog name stays unchanged |

Child defaults come from user-only `subagents` settings. Model resolution considers the explicit request, the named agent's settings override, the spec, and the child default, then inherits the parent's live model; see [Child-session settings](configuration.md#child-sessions-subagents--axosubagentsettings). Missing or team agent names return `Error::AgentNotFound`; conflicting agent/spec inputs, unavailable inline tools, and invalid output schemas return `Error::InvalidInput`. A spawn hook, the depth limit, or a shutting-down parent can reject with `Error::SpawnBlocked`.

For `Session::spawn_team`, a missing catalog name is `Error::AgentNotFound`; selecting an agent as a team, setting both selectors, giving a catalog team a charter, invalid/duplicate/reserved roster names, conflicting member agent/spec, or incomplete/extra task keys is `Error::InvalidInput`. Roster name `parent` is reserved. Teams exceeding `subagents.maxConcurrent` return `Error::SpawnBlocked`. Every member's spec and hook decision is resolved before any child is created; there is no call-level model or output override. Members use the usual child model cascade and their own result schemas. See [Teams](prompts-and-context.md#teams) for file format and model-facing usage.

Children inherit the resolved configuration, cwd, execution environment, supervisor, store, models, streamer, agent catalog, current host tool factories (filtered by the child's allowlist), current host prompt sections, schemes, and tool/spawn hooks. They disable `ask` and receive fixed delegation framing plus their resolved spec. `SpawnLink::spec` persists the spec after defaults and hooks, so later agent-file edits do not change it. Root-only personas are omitted from the model's spawn catalog and rejected with `Error::AgentNotSubagent`, including when selected as a team member.

`Axo::session(&self, id: &str) -> Result<Arc<Session>, Error>` is async and returns a known live child or revives a parked one, resuming unfinished work. Children become known when spawned or when their root opens in this process; an unknown id first waits for reopened sessions to finish registering their children, then returns `Error::SessionNotFound`. `Axo::open_session` rejects child files with `Error::ChildSession { path, root }`, naming the owning root when its chain is found: open that root and use `Axo::session` instead. `Session::spawn_link()` returns the child's `SpawnLink`, and session listings include `SessionInfo::spawn`.

### Host messages

`Session::post` takes `HostMessage { custom_type: String, text: String, display: bool, delivery: HostDelivery }`. The text enters model context as a custom message (lowered to a user message for model requests); `custom_type` identifies its host-defined purpose and `display` tells clients whether to show it. Hiding it in the UI does not hide it from the model. Unlike `append_custom`, which stores metadata only, `post` supplies model-facing content.

| `HostDelivery` | Active run | Idle session |
| :--- | :--- | :--- |
| `Steer` | Read at the next turn boundary, after the current tool batch; signals pending steering to tools | Starts a run |
| `FollowUp` | Read when the run would otherwise finish | Starts a run |
| `Quiet` | Read at the next turn boundary without steering tools | Waits for the next run; never starts one |

User input and host `Steer` / `FollowUp` clear the automatic-continuation pause; `Quiet` does not. Queueing is durable: `post` stores one `Text` prompt part plus `SubmittedCustom { custom_type, display }` on `Entry::Submission` before delivery. Host text is not a user slash command: skill expansion is bypassed both on initial delivery and recovery.

Use `post` for changing host instructions or notices that belong in the conversation. It preserves the system prompt and tool definitions, keeping their stable request prefix cacheable; repeatedly changing `set_section` or `set_tools` for per-turn notices would change that prefix. Those setters remain appropriate for actual changes to the session's standing instructions or available tools.

### Context usage

`ContextUsage { tokens: u64, window: u64, trigger: u64, reserve: u64 }` describes current context, not historical spend: `tokens` uses the compaction estimator over the current projection, `window` is the effective model context window, `reserve` is the configured compaction headroom, and `trigger` is `window - reserve`. Limits reflect the current model and compaction settings. `Session::context_usage()` and `LiveSnapshot::context` return `None` when those limits cannot be resolved.

`SessionEvent::Context` refreshes this state after assistant responses, context rewrites, and model or persona changes. Use the snapshot as the initial value and subsequent events as replacements. `LiveSnapshot::auto_paused` and `SessionEvent::AutoPaused { paused }` similarly expose whether automatic continuation is paused; this is separate from whether a run is currently active.

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
| `Job` | `JobInfo` | A listed job appeared or changed: a command or child row's state after a promotion, exit, admission, run start, or run message |
| `JobGone` | `id: String` | A job left the list |
| `Context` | `ContextUsage` | Current context estimate and model window, compaction trigger, and reserve |
| `AutoPaused` | `paused: bool` | Automatic-continuation pause state changed |

Events arrive in one total order per session. `attach` takes its snapshot and subscription under the journal, live, and job-registry locks, so every event falls cleanly before or after `LiveSnapshot`. State-valued events such as `Job`, `Context`, and `AutoPaused` may repeat a value the snapshot already holds, but are never missed.

For a promoted command, the stream reports the running row, then its persisted `Entry::JobEnd` with typed exit, then the finished/failed row with its message delivery state. Entries carry the durable result text; row updates carry state, not a second result transcript.

### Background jobs for hosts

`bash` commands that move to the background become jobs `bg_N` (see [Background Jobs](prompts-and-context.md#background-jobs-cratesaxosrcjobsmodrs-cratesaxosrcjobscommandrs)); spawned children have persistent rows `agent_N`. The model reaches both through `proc://`; hosts get the same state as data without parsing that text:

- **State:** `Session::jobs()` returns `Vec<JobInfo>`, and `attach().live` carries the same list as `LiveSnapshot::jobs`.
- **Spend and selection:** `LiveSnapshot::usage` sums the session's original journal usage, including maintenance and failed responses across branches, without counting `run_end` or `job_end` rollups again. Child spend is separate; child-run reports expose it through `Entry::JobEnd`. The snapshot also carries the live `model`, `thinking`, and selected persona name in `agent`; getters expose the same current selection.
- **Changes:** each listed row that appears or changes emits `SessionEvent::Job(info)`, and a row leaving the list emits `SessionEvent::JobGone { id }`; upsert rows by `id` over the snapshot. Foreground commands that are never listed emit nothing.
- **Stop:** `Session::kill_job(id)` stops a job; the result is then journaled and delivered like any other job result. Commands record `CommandExit::Killed { by: Killer::Host }` and read `Command was stopped by the user`; model writes to `proc://…/kill` record `Killer::Model` instead.
- **Scheme text:** `session.schemes().read(&url).await` resolves file targets and reads them as UTF-8, while virtual targets use their handler. A missing file reports `{url} not found`; binary data reports `{url} is not UTF-8 text; resolve it and read the file`. Use `CommandOutput::read` for live command byte ranges, or resolve a file target and perform host file I/O.
- **Replay:** `jobs_from_entries(&header, &entries)` reconstructs listed rows without opening a live session. Supply all raw entries, as returned by `Axo::open_session_read`, not just a projected branch. An unfinished command remains `Running`; replay does not reconcile it or invent live-only facts.
- **Output:** `command_output(call_id)` returns a `CommandOutput` with `job()` (`bg_N`), `artifact()` (the numeric artifact id), `progress()` (a watch receiver of `OutputProgress { bytes, done }`), and async `read(range: Range<u64>)`. Output is raw merged stdout/stderr bytes, clamped to flushed bytes. `done` means capture finished, including retained helpers. A held handle stays readable after the foreground spool is unlinked on Unix. No stdin or detach API is exposed; background commands die with their session.

`JobInfo` (`axo::JobInfo`) has `id: String`, `status: JobStatus`, `call_id: Option<String>` (`None` for host-spawned children), `started_at: i64` (epoch milliseconds: command spawn or child admission time), `kind: JobKind`, and `messages: Vec<JobMessageInfo>`. Messages appear in sequence order with `id`, `note`, `is_error`, `partial`, `run_id`, and `delivery`. `MessageDelivery` is `Held`, `Writing` (journal write in flight), `Pending`, `Reserved`, or `Delivered`. Text, typed child result value, and child-run usage remain in the corresponding `job_end` entry rather than the row.

| `JobKind` | Fields | Meaning |
| :--- | :--- | :--- |
| `Command` | `command: String`, `tool: String`, `cwd: PathBuf`, `artifact: u64`, `promotion: Option<Promotion>`, `promoted_at: Option<i64>`, `ended_at: Option<i64>`, `exit: Option<CommandExit>` | Tool-supplied display label, tool name, working directory, merged output spool, promotion cause and times, and root-process outcome. Times are epoch milliseconds; promotion is `Async`, `Timer`, `Steer`, or `Host`. Never-promoted retained helpers have no promotion |
| `Child` | `session_id: String`, `path: PathBuf`, `depth: u32`, `agent: Option<String>`, `model: String`, `team: Option<TeamRef>`, `resident: bool`, `progress: Option<ChildProgress>` | Child identity, path, nesting depth (root children are depth 1), optional catalog agent, expanded provider/model, and team identity. `resident` is true while the child session is loaded, including queued and externally retained children, and false when parked. Progress describes the current or last observed run; it is absent before the first completed assistant message and after reopen until new progress arrives |

`CommandExit` is `Exited { code }`, `Signaled { signal }`, `TimedOut { after_secs }`, `Killed { by: Killer::Model | Killer::Host }`, `Aborted` (foreground cancellation), or `Interrupted` (reconciliation on reopen). A process-wait failure can leave `exit` absent. Promotion causes and killers serialize in camelCase; exits use a camelCase `type` tag and fields.

`ChildProgress` contains `turns: u32` (request count), `max_turns: u32` (configured budget), `wrapping_up: bool` (request count has reached the budget), `yield_failures: u32` (rejected output attempts, zero without an output schema), and `usage: Usage` (persisted assistant spend for the run). Child state changes, progress, and parking/revival publish `SessionEvent::Job` updates.

`JobStatus` (`axo::JobStatus`) is `Queued` (child claimed but waiting for a run slot), `Running` (child owns its slot or command root is active), `HelpersRunning` (commands only: retained supervised processes), `Finished`, or `Failed`. Otherwise a child's latest non-note run message determines success/failure (no run message means failed). Mid-run notes never settle a child or change its completed-run status. `JobStatus::label` returns the `proc://` wording (`queued`, `running`, `helpers running`, `finished`, `failed`); `agent://` uses the same child labels, with no separate idle state. `Axo::schemes` has neither `proc://` nor `agent://`; only open sessions expose these live schemes. The [agent scheme](prompts-and-context.md#agent) documents status, results, transcripts and messaging routes. Parent `shutdown` also shuts down children, while dropping a parent need not stop a child held independently by the host.

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
| `Scheme` | `axo::Scheme` | A URL scheme (`name://path`) the file tools accept as a path. `fn name(&self) -> &str`, `async fn resolve(&self, url: &SchemeUrl, cx: &SchemeContext) -> Result<Target, ToolError>`, `async fn read(&self, url: &SchemeUrl, cx: &SchemeContext) -> Result<Vec<u8>, ToolError>`, `async fn write(&self, url: &SchemeUrl, content: &[u8], cx: &SchemeContext) -> Result<(), ToolError>`. `resolve` maps a URL to `Target::File { path, writable }` (the normal file pipeline runs on it; bash rewrites whole-word, `--flag=`/`NAME=`, and redirect-target URLs to that path) or `Target::Virtual` (the scheme's own `read`/`write` handle it; defaults return `{url} cannot be read` / `{url} is read-only`). `edit`/`write` refuse non-writable files (`{url} is read-only`); bash refuses virtual URLs (`Cannot use {url} in bash: no file backs it. Use read {url}.`) and output redirects onto read-only files. `SchemeUrl { scheme, path }` carries lowercase scheme and verbatim path; `SchemeContext { session_id, session_dir, cwd }` carries per-session facts. `prompt_doc` (default empty) is the one-line bullet listed in the model's `schemes` prompt section. Host scheme names must match `^[a-z][a-z0-9+.-]+$` (lowercase, two or more characters); `artifact` and `file` are reserved (`Error::Scheme`). `write_replay` defaults to `Replay::Never`. Built-ins: `artifact://`, `skill://`, `local://`, `axo://` (only when the host set `AxoBuilder::cache_dir`), and in open sessions `proc://` (background jobs; see [Background Jobs](prompts-and-context.md#background-jobs-cratesaxosrcjobsmodrs-cratesaxosrcjobscommandrs)) and `agent://` (see [agent scheme](prompts-and-context.md#agent)). Passed via `SessionOptions::schemes`; a host scheme named like a built-in replaces it. Hosts resolve URLs they display through `Session::schemes()` or `Axo::schemes(path, project_access, host_schemes)` |
| `SessionStore` / `SessionWriter` | `axo::{SessionStore, SessionWriter}` | Pluggable persistence backend for creating, locking, reading (whole sessions or just the header), listing, and appending session entries. `SessionWriter::path` is known before the file exists; `append` must complete durable storage for `Entry::requires_sync` entries before returning. Bundled implementation: `JsonlStore`. Passed as the required `store` argument of `Axo::builder` |
| `Tool` | `axo_agent::Tool` | Defines a tool (`name`, `description`, JSON Schema `parameters`, `execution_mode`, `replay`, `prompt_guidelines`, and `execute(cx: ToolContext)`) for `axo_agent::Agent`. `execution_mode` defaults to `Parallel`, `replay` to `Never`, `prompt_guidelines` to empty. Tools hold their own session handles; `cx` carries per-call state. Pass host tools as `SessionOptions::tools` `ToolFactory` closures; axo calls each with the session's `Arc<SessionTools>` (again per fork), so the tool can use `locate`, `schemes`, `rewrite_urls`, the ledger, the `exec` environment, and the `supervisor` for subprocesses. One named like a built-in replaces it in place |
| `ToolHooks` | `axo_agent::ToolHooks` | Host tool policy, supplied as an ordered `Vec<Arc<dyn ToolHooks>>` via `SessionOptions::tool_hooks` (empty by default; inherited by forks and children), or directly via `AgentConfig::tool_hooks`. Children's internal turn-budget hook runs before host hooks. `before_tool_call` receives `BeforeToolCallContext` and returns `ToolCallDecision`; the first `Block { reason, terminate }` or hook error blocks the call and skips later hooks. `after_tool_call` receives `AfterToolCallContext` and returns `AfterToolCallResult` overrides; each hook sees the result after earlier overrides, and errors are logged and skipped |
| `SpawnHook` | `axo::SpawnHook` | Child-creation policy via `SessionOptions::spawn_hooks`; returns `SpawnDecision` from the resolved `SpawnContext`. See [Spawning child sessions](#spawning-child-sessions) |
| `LoopHooks` | `axo_agent::LoopHooks` | The loop driver's contract, implemented internally by each `Session` (a private hooks object), not supplied through session options. `prepare_request(&AgentContext, cancel)` returns `Result<Option<Vec<AgentMessage>>, HookError>` to optionally replace messages; `convert_to_llm` lowers `Custom` messages (default drops them); `should_skip_tools` intercepts unusable responses; `finish_turn` returns `TurnFinish { continue_turn: bool, messages: Option<Vec<AgentMessage>> }`; `prepare_next_turn` returns `Vec<AgentMessage>` appended with normal events and `MessageOrigin::Run`; `steering` supplies queued host input (see [Queued inputs and message origins](#queued-inputs-and-message-origins)). Model, thinking, and tools remain controlled through `Agent::set_model`, `Agent::set_thinking`, and `Agent::set_tools` |
| `Persist` | `axo_agent::Persist` | The agent's only event sink: ordered, awaited `async fn persist(&self, event: &AgentEvent) -> Result<(), PersistError>`; an error stops the run. `Session` uses it to persist `Entry::Message` on `MessageEnd { message, origin }` before the next request, maintain live run state, and relay session events. The agent has no broadcast or snapshot API; clients attach to `Session` |
| `Streamer` | `axo_ai::Streamer` | Host streaming extension, injected via `AxoBuilder::streamer` for tests and custom transports. `Models` implements it with provider dispatch; `ScriptedApi` implements it behind the `testing` feature |
| `Api` | `axo_ai::Api` | Wire adapter contract that `Models` dispatches to: `AnthropicApi`, `CompletionsApi`, or `CodexApi` |
| `CredentialStore` | `axo_ai::CredentialStore` | Loads and saves per-provider `Credential`s (`FileCredentialStore` over a host-named file is bundled). Supports `get`, `set`, `remove`, and `update` (runs a transform under the store's cross-process file lock). Injected via `ModelsOptions::credentials` into the `Models` the host passes to `Axo::builder` |

For deterministic streaming tests, `axo_ai::testing::ScriptedApi` records requests and serves scripted turns. `ScriptedApi::lane(&self, model_id: &str, turns: Vec<ScriptedTurn>)` appends a dedicated FIFO for that exact model id (not provider/model). Requests without a lane use the shared FIFO; an exhausted lane returns “script exhausted” rather than falling back to that queue. Separate child model ids keep concurrently running parent/child scripts independent.

### Implementing `axo_agent::Tool`

When driving `axo_agent::Agent` directly (or adding tools to a custom loop): build an `AgentConfig::new(streamer, model)` (fields: `hooks`, `tool_hooks`, `persist`, `steering_mode`/`follow_up_mode: QueueMode`, `session_id`; defaults to `NoHooks`, no persist, one-at-a-time queues, thinking off), construct with `Agent::new(config)`, and interact via `prompt`, `steer`, `follow_up`, `continue_run`, `cancel(CancelReason)`, and `shutdown`. Implement `axo_agent::Tool`:

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

File-mutating host tools must follow the same invariants as built-ins: resolve targets with `SessionTools::locate` (`Located::File { url, path, writable }` or `Located::Virtual { url }`; plain filesystem paths have no URL and are writable), verify `writable`, hold `axo::tools::lock_file(path)` throughout the read-modify-write cycle, check existing bytes with `FileLedger::check_fresh` on `SessionTools::ledger`, and record the written hash with `ledger.record(path, bytes, FileAccess::Modify)` (or `FileAccess::Read` for reads), returning the record as `ToolOutput::details` `{"ledger": record}` so `FileLedger::restore` can rebuild the ledger on reopen and navigation.
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
| `Supervision` | `enum { ProcessGroup, Cgroup { program: PathBuf, args: Vec<OsString>, cgroup_root: Option<PathBuf> } }` | Host-selected command supervision mode; see [Command supervision](configuration.md#command-supervision-axosupervision) |
| `ExecSpec` | `{ program: PathBuf, args: Vec<OsString>, cwd: PathBuf, env: EnvMap }` | One command. `env` is the complete environment (`env_clear` semantics); a bare `program` resolves on its `PATH` |
| `ProcessSupervisor::new` | `fn new(supervision: Supervision) -> Arc<Self>` | Checks nothing yet, so hosts that never run commands need no delegated cgroup even in `Cgroup` mode. `AxoBuilder::build` creates one per `Axo` with the builder's `Supervision` (default `ProcessGroup`) |
| `ProcessSupervisor::spawn` | `async fn spawn(&self, spec: ExecSpec) -> Result<Process, String>` | Starts `spec` in a fresh container (process group or cgroup job) with stdin closed and stdout/stderr piped. In process-group mode an unstartable program returns `Err`; in cgroup mode `spawn` succeeds and the root exits 127 (program not found) or 126 (other `exec` failure) with `axo supervisor: cannot run …` on stderr (fails closed when supervision is unavailable; see [Command supervision](configuration.md#command-supervision-axosupervision)). Cancelling leaves nothing behind |
| `ProcessSupervisor::output` | `async fn output(&self, spec: ExecSpec, deadline: Option<tokio::time::Instant>) -> Result<std::process::Output, OutputError>` | Like `Command::output`: reads stdout and stderr while waiting for the root, then kills anything left in the container and closes it. Reaching `deadline` kills the job and returns `OutputError::TimedOut`; a spawn or wait failure returns `OutputError::Failed(message)`. `OutputError` implements `Display` (`timed out`, or the message) and `std::error::Error` |
| `Process` | `pub stdout: Option<ChildStdout>`, `pub stderr: Option<ChildStderr>` | A running job. `capture_output()` merges both pipes into one `mpsc::Receiver<Vec<u8>>` (plus the reader `JoinSet`); `wait()` waits for the root (cancel-safe; in cgroup mode a guardian that dies first takes the job with it); `guardian_lost().await` resolves once the cgroup guardian has died, after killing the job (for watching leftovers after the root exited; cancel-safe, resolves at most once, and never in process-group mode, whose guard is best effort); `container()` returns a cloneable `Container`; `close().await` waits until the container is empty, removes the cgroup directory in cgroup mode, and stands the watcher down (call `container().kill()` first to end leftovers). Dropping an unclosed `Process` kills the whole container |
| `Container` | `kill(&self)`, `populated(&self) -> bool`, `async terminate(&self)` | Handle to one job's container: `kill` sends SIGKILL to the process group or writes `cgroup.kill` (errors ignored); `populated` reports whether any process remains (a cgroup that is unreadable counts as empty; a group member that refuses a signal still counts as present); `terminate` sends SIGTERM (to the group, or to each cgroup member through a pidfd), waits up to `KILL_GRACE` (1 s) for the container to empty, then kills it |
| `KILL_GRACE` | `const KILL_GRACE: Duration` | Fixed 1 s grace period between SIGTERM and SIGKILL used by `Container::terminate` |
| `find_on_path` | `fn find_on_path(name: &str, env: &EnvMap, cwd: &Path) -> Option<PathBuf>` | Path search helper resolving a command against `env["PATH"]` and `cwd` |

Commands run through `SessionTools::run_command` have the same background behavior as `bash`: immediate promotion with `run_async`, timed promotion controlled by `bash.promoteAfterMs`, or promotion when a steer is pending; retained helpers remain supervised. Their jobs appear in `Session::jobs` and `proc://`, and results are delivered exactly once through `wait` or a later message. Hosts needing a strictly call-scoped subprocess can use `ProcessSupervisor::output` directly; those subprocesses are not registered as session jobs or promoted.

### Queued inputs and message origins

`axo_agent` knows nothing about jobs; it offers two generic primitives that `Session` builds job delivery on.

- **`QueuedMessage { message: AgentMessage, id: Option<String> }`:** an input waiting to enter the transcript, with the host's id for it (`From<AgentMessage>` gives `id: None`). `Agent::prompt(Vec<QueuedMessage>)`, `Agent::steer(QueuedMessage)`, and `Agent::follow_up(QueuedMessage)` take it; `agent.prompt(vec![message.into()])` keeps the id-less form short. `Agent::steer` also sets the steer signal until the steering queue is drained.
- **`LoopHooks::steering`:** `async fn steering(&self) -> Vec<QueuedMessage>` (default empty) supplies host messages drained after the agent's own steering queue, after the current tool batch and before the next model request. It never sets the steer signal. `Session` uses it for pending command/child row messages and teammate mail. Follow-up input comes from the agent's follow-up queue, drained when the run would otherwise end. `Agent::queued()` returns the queued messages as `(Vec<AgentMessage>, Vec<AgentMessage>)` in steering, follow-up order; teammate mail is held separately by the session's registry.
- **`AgentEvent::MessageEnd { message, origin: MessageOrigin }`:** every finished message carries its provenance:

| `MessageOrigin` | Assigned to |
| :--- | :--- |
| `Input(Option<String>)` | A prompted or queued input (including hook-supplied ones), with the id it was queued under |
| `Tool(ToolInvocationId)` | The result of a tool call that started executing with that invocation: what it returned (`Ok` or `Err`), even if cancellation arrived meanwhile, or the aborted result when Stop ended its task |
| `Run` | Everything else: assistant replies, tool-change declarations, messages returned by `prepare_next_turn`, and synthetic tool results (blocked, failed-preparation, unanswered, or aborted before they started) |

An executed result keeps its `Tool` origin exactly when it is the result persisted. `Session`'s `Persist` maps `Input(Some(id))` to the submission id (or, for a job delivery message, to the jobs it delivers) and `Tool(invocation)` to the jobs that call delivered or started, so receipts never depend on comparing message text (see [Background Job Durability](sessions-and-compaction.md#background-job-durability)).
