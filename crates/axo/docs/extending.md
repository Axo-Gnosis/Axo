# Embedding & Extending axo

`axo` is a library with no configuration directory of its own. Hosts customize it by supplying configuration through `ConfigSource` and resources through `Axo::builder`, and extend it in Rust by embedding `axo`, `axo-agent`, and `axo-ai`. Hosts that load trusted TypeScript/JavaScript packages add `axo-extensions` (see [Bun extensions](#4-bun-extensions-axo-extensions)). Repositories and users customize sessions through the ecosystem `.agents/` and `AGENTS.md` conventions and whatever the host exposes.

## 1. Configuration and Declarative Files

axo reads the ecosystem files itself; everything else arrives from the host, in whatever names, locations, and formats the host chooses (see [`configuration.md`](configuration.md#configuration-source-axoconfigsource)):

- **Context files (`AGENTS.md`, `AGENTS.local.md`, `CLAUDE.md`, `CLAUDE.local.md`, `.agents/AGENTS.md`, plus any path enabled in `settings.contextFiles`):** Add persona, architecture notes, and coding rules at the repository root or in subdirectories (loaded on demand when `read`, `write`, or `edit` touches that subtree). User-wide files come from the host as `UserConfig::context_files`, and `~/.agents/AGENTS.md` with `homeAgentsDir: true`. See [`prompts-and-context.md`](prompts-and-context.md#context-files-cratesaxosrccontextfilesrs).
- **Prompt overrides (`ConfigLayer::prompts`):** The user and project layers may override `SYSTEM`, `WORK`, `PROSE`, or the summarizer prompts (`SUMMARY`, `COMPACTION`, `COMPACTION_UPDATE`, `COMPACTION_SPLIT_TURN`, `BRANCH_SUMMARY`); the project's override wins per name. An empty `SYSTEM`, `WORK`, or `PROSE` override disables that prompt section, an empty summarizer override keeps the bundled prompt with a `Diagnostic`, and a name outside `PROMPT_NAMES` is an `Error::Config`.
- **Skills (`.agents/skills/<name>/SKILL.md`, host skill roots, and `~/.agents/skills/<name>/SKILL.md`):** Package task instructions and reference files that the model reads on demand from `<available_skills>` or the user invokes inline with `/<name> [args]`. Roots are searched in order: project `.agents/skills`, allowed project `ConfigLayer::skill_roots`, user `ConfigLayer::skill_roots`, then `~/.agents/skills` (only with `homeAgentsDir: true`). Project skills require `Read` or `Trust` and the user's `project.files.skills`; explicit project roots also require acquisition through `project.files.config`. Optional `argument-hint` frontmatter is the placeholder apps show after `/<name>`; a skill named like a built-in command gets a warning diagnostic and is left out of the command catalog. See [`prompts-and-context.md`](prompts-and-context.md#skills-cratesaxosrccontextskillsrs).
- **Custom providers and model patches (`axo_ai::ModelsConfig`):** The host's `ModelsConfig` registers OpenAI- or Anthropic-compatible endpoints, custom headers, literal or `$ENV` API keys, and per-model overrides (`contextWindow`, `maxTokens`, `reasoning`, `thinkingLevelMap`, `serviceTiers`, `cost`, `compat`). Config values starting with `!` are rejected; hosts resolve command-based secrets before passing configuration. See [`configuration.md`](configuration.md#models-axo_aimodelsconfig--credentials).
- **Agent definitions:** Supply `*.md` files with YAML frontmatter in `ConfigLayer::agent_roots` (project roots require `Trust`), or `AgentSpec` values through `Definitions::agents`. Select a root persona with `SessionOptions::agent`; its body, tool allowlist, context-file switches, and section removals configure the session. See [Agent definitions](prompts-and-context.md#agent-definitions).
- **Mode definitions:** Supply markdown files through `ConfigLayer::mode_roots` (project roots require `Trust`), or `ModeSpec` values through `Definitions::modes`. Root sessions select them with `Session::set_mode` or session-parsed commands; see [Root-session modes](#root-session-modes) and [Mode definitions](prompts-and-context.md#mode-definitions).
- **Keyword definitions:** Supply markdown files through `ConfigLayer::keyword_roots` (project roots require `Trust`), or `KeywordSpec` values through `Definitions::keywords`. A root session fires an active keyword when its name appears as a prose word in user-typed text: its body joins that user message and its optional `model` applies like `/model`. Settings `keywords.enabled` and `keywords.disabled` switch them off; see [Keyword definitions](prompts-and-context.md#keyword-definitions).
- **Bundled definitions:** `ModeSpec::parse(default_name, markdown)`, `AgentSpec::parse(default_name, markdown)`, and `KeywordSpec::parse(default_name, markdown)` turn host-bundled markdown (for example `include_str!("modes/plan.md")`) into programmatic specs. They run the definition-file parser, so frontmatter, field types, names and schemas are validated immediately, during the `parse` call; only name collisions wait until the specs are loaded. They return `(Option<ModeSpec>, Vec<Diagnostic>)`, `(Option<AgentSpec>, Vec<Diagnostic>)`, and `(Option<KeywordSpec>, Vec<Diagnostic>)`: `None` plus diagnostics for an invalid definition (markdown without frontmatter reports `missing YAML frontmatter`), diagnostics without an origin, and a spec without a `path`. `default_name` applies only when frontmatter has no `name` key; an explicit empty `name` is invalid. Pass the specs through `Definitions` or `Axo::catalog`. File roots load before programmatic specs and the first name wins, so user and project files override bundled defaults by name. `AgentSpec::parse` rejects team definitions (frontmatter `members`) with `None` and only the diagnostic `team definitions load only from agent roots`, before any other validation. To supply a team in code, construct a `TeamSpec`.
- **Model roles:** User-only `modelRoles` maps role names to model selectors. `SessionOptions::model`, agent, mode, and keyword models, `defaultModel`, and `/model` accept `provider/model`, `@role`, and thinking or service tier suffixes, together or alone. See [Model selectors](configuration.md#model-selectors).

Start-time diagnostics for context files, skills, agent, mode, and keyword definitions, tool allowlists, schemes and bundled docs, prompt overrides, host sections, and reserved or hidden command names are available through `session.diagnostics()` and logged as warnings (see [`prompts-and-context.md`](prompts-and-context.md)); runtime `NestedContext` read errors are only logged.

## 2. Embedding `axo` in a Rust Host

Hosts link the `axo` crate to manage `Models`, a `SessionStore`, a `ConfigSource`, and `Session` instances in a single process. `Axo::builder(models, store)` takes the two required inputs; `AxoBuilder::config` and `AxoBuilder::cache_dir` are optional (see [Absent inputs](configuration.md#absent-inputs)), and `AxoBuilder::build` is synchronous, infallible, and performs no configuration I/O. See [Configuration sampling](configuration.md#configuration-source-axoconfigsource) for when the host is called.

`SessionOptions::host` and `SessionOptions::layer` are both `Registrations`: definitions, tools, tool policies, spawn hooks, schemes, sections, and commands. Host setters replace only the host side; `Session::set_layer` replaces only the composition layer. Definitions rank files, then layer, then host (first name wins); the other lists apply host before layer.

### Live definitions

Use one `Definitions` value for agents, teams, modes, keywords and skills. Pass the host’s value in `SessionOptions::host.definitions` and the layer’s in `SessionOptions::layer.definitions`. A code-supplied `Skill` points to a `SKILL.md` on disk; its body and sibling reference files are read on demand.

A session reads its configured definition roots once at start. `Session::set_definitions` replaces the host’s value; `Session::set_layer` replaces the layer’s. Each change merges the registrations with the original file snapshot, without rescanning. Files come first, then the layer, then the host. Within each kind the first name wins; later collisions are diagnostics. Agents and teams share a namespace, and teams are checked against agents from all sources.

The definition accessors return owned snapshots of what is in force now. A selected persona or mode keeps its copy even if its definition changes or disappears; select again to use the new definition. Supply registrations again on reopen.

`Axo::catalog(cwd, project_access, definitions)` previews a session without creating one. It performs blocking file I/O and merges the discovered files with the supplied `&Definitions`. The returned `Catalog` contains the resolved config, agents (including teams), modes, keywords, skills and diagnostics.

`Axo::models() -> &Arc<Models>` returns the shared model registry. `Axo::supervisor() -> Arc<ProcessSupervisor>` gives hosts the supervisor used by sessions, so other processes (such as extension runners) use the same container mechanism. `Axo::invoke(method, args, caller)` dispatches positional JSON to host methods only. `Axo::invoke_models(models, method, args, caller)` dispatches model methods against the supplied base or scoped registry. Both return `Result<Value, InvokeError>`. See the [generated reference](extension-api.md) for exposed methods.

### One-off completions

`Axo::stream(request: CompletionRequest, cancel: CancellationToken)` returns `Result<BoxStream<'static, AssistantEvent>, Error>`; cancel the token to stop the stream. `Axo::complete(request)` asynchronously collects the final `AssistantMessage`. Both run outside a session through the host's configured streamer, using [user-only configuration](configuration.md#configuration-source-axoconfigsource).

`CompletionRequest` requires `model: String` and `messages: Vec<Message>`. Optional `thinking` and `service_tier` override the corresponding [model selector](configuration.md#model-selectors) suffixes; if neither source specifies a value, thinking is `off` (not `defaultThinkingLevel`) and the tier is standard. The thinking level is passed through without clamping. The selector must resolve a model (`provider/id` or `@role`); a suffix-only selector returns `Error::InvalidModelSelector`. Other optional fields are `max_tokens: Option<u64>`, `temperature: Option<f64>`, and `tool_choice: Option<ToolChoice>`. The JSON form uses camelCase and rejects unknown fields.

If the stream reports a terminal error or ends without a final message, `complete` returns `Error::Completion { message, details: Option<Box<CompletionDetails>> }`. Only a terminal error carries `CompletionDetails { failure, usage, stop_reason }`, including any billed usage.

### Building `Axo` and running a `Session`

```rust,no_run
# async fn example() -> Result<(), axo::Error> {
use std::path::{Path, PathBuf};
use std::sync::Arc;
use axo::JsonlStore;
use axo::{
    Answer, Axo, Command, CommandInfo, ConfigLayer, ConfigSource, NavigateOptions, PromptPart,
    PromptRequest, PromptResult, RecoveryPolicy, Registrations, SessionEvent, SessionOptions,
    ShakeMode, UserConfig,
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
                skill_roots: vec![PathBuf::from("/var/lib/my-host/skills")],
                ..ConfigLayer::default()
            },
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
        models: None,
        agent: None,
        keywords_enabled: true,
        thinking: None,
        ask_enabled: true,
        project_access: None,
        exec_env: None,
        host: Registrations {
            sections: vec![(
                "host_context".into(),
                Some("Running inside my-host.".into()),
            )],
            ..Default::default()
        },
        layer: Registrations::default(),
        recovery: RecoveryPolicy::Resume,
    })
    .await?;

let mut attach = session.attach(None).await;
tokio::spawn(async move {
    while let Ok(event) = attach.events.recv().await {
        if let SessionEvent::QuestionPending { call_id, blocks } = &*event {
            // Render the `ask` document, then `answer` or `dismiss` it from the client UI.
            let _ = (call_id, blocks);
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
match session.parse_command(typed) {
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
| `prompt` | `async fn prompt(&self, input: impl Into<PromptInput>) -> Result<axo_agent::RunOutcome, Error>` | Saves the parts (handed-over content under `local://`), expands `/<skill> args`, adds matched [keyword](prompts-and-context.md#keyword-definitions) blocks on roots, persists a `Prompt` submission, applies the first matched keyword's model, runs the agent loop to completion, and returns `axo_agent::RunOutcome` (`Done`, `Aborted(CancelReason)`, `Failed(message)`), distinct from the journal’s `axo::RunOutcome`. Never runs a built-in command: `/model x` is ordinary prompt text. Returns `Error::Busy` if a run or maintenance task is already active, `Error::InvalidInput` for a missing referenced file or invalid content name, and a model-selector error from a matched keyword before anything is saved |
| `prompt_with` | `async fn prompt_with(&self, request: PromptRequest) -> Result<PromptResult, Error>` | `PromptRequest { input: PromptInput, request_id: Option<String> }`. Like `prompt`, but deduplicates by `request.request_id`: if a submission with the same `request_id` is already in the log, returns `PromptResult::Duplicate(existing)` without starting a second run |
| `steer` | `async fn steer(&self, input: impl Into<PromptInput>) -> Result<(), Error>` | Persists a `Steer` submission and queues it after the current tool batch, before the next model request. On an idle session it starts a run; input after the last drain point starts the next run. While queued, tools see `ToolContext::steer` pending: foreground `bash` promotes, `wait` returns, and a pending `ask` question or reviewed `yield` settles (see [Queued input for hosts](#queued-input-for-hosts)). Skills expand here too, and on roots keywords match as for `prompt`, with a matched model applied before the steer is queued |
| `follow_up` | `async fn follow_up(&self, input: impl Into<PromptInput>) -> Result<(), Error>` | Persists a `FollowUp` submission and queues it for when the active run would otherwise finish; on an idle session it starts a run. A message arriving after the run's last drain point starts the next run rather than remaining queued. Skills expand here too, and on roots keywords match as for `prompt`, with a matched model applied before the follow-up is queued |
| `withdraw` | `async fn withdraw(&self, submission_id: &str) -> Result<QueuedInput, Error>` | Takes a queued steer or follow-up back before delivery: removes it from its queue, records `Entry::SubmissionWithdrawn`, and returns the input, including any host-message metadata. Returns `Error::NotQueued` once it was delivered or withdrawn, and for mail or `Quiet` messages. Leaves the Stop latch as it is. See [Queued input for hosts](#queued-input-for-hosts) |
| `post` | `async fn post(&self, message: HostMessage) -> Result<(), Error>` | Persists host text with custom-message metadata and queues it under `HostDelivery`; bypasses skill expansion. An empty `custom_type` or blank `text`, or a type the engine reserves for its own messages (`tool_catalog`, `mode_notice`, `job_result`, `turn_budget`, `yield_nudge`, `compactionSummary`, `branchSummary`), returns `Error::InvalidInput`. See [Host messages](#host-messages) |
| `send` | `async fn send(&self, to: &str, message: &str, steer: bool) -> Result<SendReceipt, Error>` | Same child, parent-note, and teammate routes as the model's `send`, with durable identity and originating-session provenance; see [Coordination receipts](#coordination-receipts) |
| `is_running` | `fn is_running(&self) -> bool` | Whether a run, maintenance, or navigation owns the session. Observing idle does not reserve it against later submissions or background wakes; use operation results such as `Error::Busy` |
| `abort` | `fn abort(&self)` | Stop (`session.abort()`, without awaiting): cancels active maintenance and stops the current run with `CancelReason::User` (persisting an `Aborted` outcome), including foreground tool calls and `bash`. Background jobs keep running. A Stop during an active run latches automatic continuation off when pressed: queued steers and follow-ups, row messages, and teammate mail are held until user input, a host `Steer` / `FollowUp`, or `resume_wakes`; incoming teammate mail and a host `Quiet` message do not clear the latch. Input sent after the Stop clears it and is delivered, with everything held, once the stopped run settles. A Stop while idle changes nothing (see [Queued input for hosts](#queued-input-for-hosts)) |
| `resume_wakes` | `fn resume_wakes(&self)` | Clears the automatic-continuation latch without submitting input. Emits `AutoPaused { paused: false }` only if the session was paused; then, on an idle session, starts a run with the held steers (else follow-ups) per `steeringMode` / `followUpMode`, or else delivers pending row messages and queued teammate mail. While a run or maintenance is active, everything waits for its next boundary as usual |
| `shutdown` | `async fn shutdown(&self)` | Cancels with `CancelReason::Shutdown`, waits for the active run without recording it as user-aborted, recursively shuts down live children, then stops command jobs and awaits their tasks. Child runs resume when the parent reopens; commands are marked interrupted. Dropping a `Session` cancels its run, kills command containers, and releases table-owned child handles; independently held children can remain live |
| `set_model` | `async fn set_model(&self, model: Model) -> Result<(), Error>` | On a model change, appends `Entry::ModelChange`, plus `ThinkingLevelChange` if thinking is clamped and `ServiceTierChange` if the tier resets. Once written, the selection applies to the next request; allowed during maintenance, which it never invalidates |
| `set_thinking` | `async fn set_thinking(&self, level: ThinkingLevel) -> Result<ThinkingLevel, Error>` | Clamps `level` to the current model, appends `Entry::ThinkingLevelChange` when changed (applied once written), and returns the applied level |
| `set_service_tier` | `async fn set_service_tier(&self, tier: ServiceTier) -> Result<ServiceTier, Error>` | Appends `Entry::ServiceTierChange` when the choice changes, then applies it from the next request. Returns the tier the current model applies |
| `set_name` | `async fn set_name(&self, name: Option<String>) -> Result<SeqEntry, Error>` | Appends `Entry::SessionInfo` at the current position; replaces newlines with spaces, trims, and treats an empty name as `None`. May run at any time; adds no model-context messages |
| `name` | `fn name(&self) -> Option<String>` | Returns the latest session name in file order, globally rather than scoped to the current branch |
| `append_custom` | `async fn append_custom(&self, custom_type: &str, data: Option<Value>) -> Result<SeqEntry, Error>` | Appends `Entry::Custom` at the current position. May run at any time; adds no model-context messages. Rejects an empty `custom_type` with `Error::InvalidInput` |
| `custom` | `fn custom(&self, custom_type: &str) -> Option<Value>` | Returns the latest data of that type on the current root-to-leaf path (branch-scoped), or `None` if absent or the latest entry has no data |
| `answer` / `dismiss` | `fn answer(&self, call_id: &str, answers: Vec<Answer>) -> Result<(), Error>`<br>`fn dismiss(&self, call_id: &str) -> Result<(), Error>` | `answer` settles a pending question; a submission that does not fit the document returns `Error::InvalidInput` and leaves the question pending (see [Ask documents for hosts](#ask-documents-for-hosts)). `dismiss` settles a pending question or mode result and stops the run like `abort` (`CancelReason::User`), leaving the selected mode active. Both return `Error::NoPendingQuestion` for an id absent from their supported pending registries, including a question a steer already settled |
| `attach` | `async fn attach(&self, from: Option<u64>) -> Attach` | Returns `missed` entries after `from`, the current `LiveSnapshot` (including `jobs`, the same list as `Session::jobs`), and a live `broadcast::Receiver<Arc<SessionEvent>>` |
| `subscribe` | `fn subscribe(&self) -> broadcast::Receiver<Arc<SessionEvent>>` | Events from now on, without the journal and live snapshot `attach` takes; for consumers that need no catch-up |
| `compact` | `async fn compact(&self, focus: Option<String>) -> Result<(), Error>` | Runs a standalone compaction summary checkpoint in a session-owned task; `focus` tells the summary what to prioritize and is recorded on the `Entry::Compaction`; blank focus counts as none |
| `shake` | `async fn shake(&self, mode: ShakeMode) -> Result<(), Error>` | Runs recoverable artifact elision (`Elide`, `Images`, or `Thinking`) without model calls |
| `navigate` | `async fn navigate(&self, target: Option<EntryId>, options: NavigateOptions) -> Result<(), Error>` | Moves the leaf to `target` (`None` = root), optionally summarizing the abandoned branch into an `Entry::BranchSummary` (`NavigateOptions { summarize, focus }`; `focus` without `summarize` is `Error::InvalidInput` (blank counts as none)). Running jobs keep running; their results are delivered on whichever branch is selected when they finish, naming the originating command or child agent |
| `fork` | `async fn fork(&self, entry_id: &EntryId) -> Result<Fork, Error>` | Copies the root-to-`entry_id` path (to its parent when `entry_id` is a user message, returned as `Fork::editable`) into a new session; see [Attaching, Navigating & Forking](sessions-and-compaction.md#attaching-navigating--forking-session) |
| `spawn` | `async fn spawn(&self, request: SpawnRequest) -> Result<ChildHandle, Error>` | Creates a child session, durably admits its parent-registry row, and starts its first prompt asynchronously; see [Spawning child sessions](#spawning-child-sessions) |
| `spawn_team` | `async fn spawn_team(&self, request: TeamRequest) -> Result<TeamHandle, Error>` | Creates and admits a catalog or ad-hoc team's children and starts their first prompts asynchronously; see [Spawning child sessions](#spawning-child-sessions) |
| `agent` | `fn agent(&self) -> Option<AgentSpec>` | The current root persona, or for a child the spec it was spawned with; distinct from the catalog returned by `agents`. `spawn_link` returns a child's full persisted link |
| `spawn_link` | `fn spawn_link(&self) -> Option<&SpawnLink>` | The persisted spawn link for a child; `None` for roots and forks |
| `set_agent` | `async fn set_agent(&self, name: Option<&str>) -> Result<(), Error>` | Resolves a root-capable catalog persona, persists `Entry::AgentChange`, and rebuilds the next request's prompt and tools from the unfiltered base; `None` clears the persona. When the persona declares a `model` selector, it is resolved first (errors before any write) and applied with the `agent_change` in one batch, clamping thinking; otherwise model and thinking are unchanged. A missing/team name returns `Error::AgentNotFound`; a subagent-only persona returns `Error::AgentNotRoot` |
| `set_section` | `async fn set_section(&self, name: &str, body: Option<String>) -> Result<(), Error>` | Replaces or removes one host prompt section in the live base and rebuilds the effective shape. Host section overrides apply after persona section switches. An invalid section name returns `Error::InvalidInput`. Withheld from extensions |
| `set_sections` | `async fn set_sections(&self, sections: Vec<(String, Option<String>)>) -> Result<(), Error>` | Replaces every host section from the next request; a section set before and omitted now disappears. An invalid name rejects the whole set with `Error::InvalidInput` and keeps the previous one. Children return `Error::InvalidInput` |
| `set_tools` | `async fn set_tools(&self, tools: Vec<HostTool>) -> Result<Vec<Diagnostic>, Error>` | Replaces host tool registrations and updates only the direct/deferred tool lists under the current persona allowlist; returns the new tool shape's diagnostics (labeled replacements, reserved names, deferred collisions). It does not re-apply context files, persona instructions, or section overrides; the prompt is rebuilt from the retained inputs and updated tools. Future children and forks use the current factories and exposure choices |
| `set_tool_policies` | `fn set_tool_policies(&self, policies: Vec<Arc<dyn ToolPolicy>>) -> Result<(), Error>` | Replaces the host tool policies for hook invocations that start after it returns; a hook pass already under way finishes with the list it started with. Future forks and children start with the new list, live children keep theirs. Children return `Error::InvalidInput` |
| `set_spawn_hooks` | `fn set_spawn_hooks(&self, hooks: Vec<Arc<dyn SpawnHook>>) -> Result<(), Error>` | Replaces the host spawn hooks for spawns resolved after it returns; future forks and children start with the new list. Children return `Error::InvalidInput` |
| `set_schemes` | `async fn set_schemes(&self, schemes: Vec<Arc<dyn Scheme>>) -> Result<(), Error>` | Replaces the host schemes over the built-ins: URLs resolve through the new set at once (`Session::schemes()` included), and the prompt's `schemes` section changes from the next request. An invalid or reserved name rejects the whole set with `Error::Scheme` and keeps the previous one. Future forks and children start with the new set. Children return `Error::InvalidInput` |
| `set_commands` | `fn set_commands(&self, commands: Vec<RegisteredCommand>) -> Result<Vec<Diagnostic>, Error>` | Replaces the host's [registered commands](#commands) and returns the diagnostics of every registered command, layer included; future forks start with the new list. Children return `Error::InvalidInput` (`This operation is only available on root sessions`) |
| `set_definitions` | `async fn set_definitions(&self, definitions: Definitions) -> Result<Vec<Diagnostic>, Error>` | Replaces the host’s [live definitions](#live-definitions) from the next request. Returns merge, skill and command diagnostics, then tool-shape diagnostics |
| `set_layer` | `async fn set_layer(&self, layer: Registrations) -> Result<Vec<Diagnostic>, Error>` | Replaces the layer’s tools, policies, spawn hooks, schemes, sections, commands and definitions together without changing host registrations. An invalid section or scheme name rejects the whole layer. Returns tool-shape, definition and command diagnostics. Children reject both setters |
| `model` / `thinking` / `config` | `fn model(&self) -> Model`<br>`fn thinking(&self) -> ThinkingLevel`<br>`fn config(&self) -> &EffectiveConfig` | The live model and thinking selection, and the session's resolved configuration |
| `service_tier` | `fn service_tier(&self) -> ServiceTier` | The chosen tier, even when unsupported by the current model |
| `effective_service_tier` | `fn effective_service_tier(&self) -> ServiceTier` | The tier requests use on the current model: the chosen tier if supported, otherwise standard |
| `context_usage` | `fn context_usage(&self) -> Option<ContextUsage>` | Current context estimate and model limits, also available in `LiveSnapshot::context`; distinct from cumulative token spend. See [Context usage](#context-usage) |
| `tree_usage` | `fn tree_usage(&self) -> Usage` | Cumulative token spend of this session and every descendant child, including maintenance; also `LiveSnapshot::tree_usage` and `SessionEvent::TreeUsage`. See [Background jobs for hosts](#background-jobs-for-hosts) |
| `reload_exec_env` | `async fn reload_exec_env(&self)` | Invalidates and re-resolves the `ExecEnv` (such as a Nix flake dev shell), queuing a `dev_shell` system-prompt diff if the note changed |
| `commands` | `fn commands(&self) -> Vec<CommandInfo>` | The command catalog a host may offer: built-ins, then modes, then registered commands, then skills not hidden by any of them. See [Commands](#commands) |
| `parse_command` | `fn parse_command(&self, text: &str) -> Option<Result<Command, Error>>` | Parses built-ins first, then `/<mode> [prompt…]` against this session's mode catalog, then `/<registered> [args…]` as `Command::Registered { name, args }`; other input remains prompt text |
| `run_command` | `async fn run_command(&self, command: Command) -> Result<(), Error>` | Runs a command under its policy. `Command::Model` accepts [model selectors](configuration.md#model-selectors); `Command::Mode { name, prompt }` selects the mode and passes an optional prompt to `steer`; `Command::Registered { name, args }` awaits the registered `CommandHandler::run(session, args)` (an unknown name is `Error::InvalidCommand`) |
| `id` / `path` / `skills` | `fn id(&self) -> &str`<br>`fn path(&self) -> &Path`<br>`fn skills(&self) -> Vec<Skill>` | The header UUIDv7 (known before the file exists); the session file (roots and children create it on their first submission, non-system message, or child admission, so a child's file exists once `spawn` returns; forks create it immediately); the skills in force now, which feed the prompt catalog and `/name` commands |
| `render_skill` | `async fn render_skill(&self, name: &str, args: &str) -> Result<String, Error>` | Renders a loaded skill's instruction block with arguments, including skills hidden from model invocation. Does not submit it: pass the result to `follow_up` as input or to `compact` as focus. An unknown name returns `Error::SkillNotFound`; a read failure returns `Error::Io` |
| `invoke` | `async fn invoke(&self, method: &str, args: Vec<Value>, caller: &Caller) -> Result<Value, InvokeError>` | Calls a method by its camelCase wire name with positional JSON arguments. `axo::ext::Caller` supplies an optional namespace for custom types; the Bun bridge supplies the extension ID. See [Extension session handles](#extension-session-handles) for wire shapes and errors |
| `agents` | `fn agents(&self) -> Vec<AgentDef>` | Agents and teams in force now. `SessionOptions::agent` selects a root at creation. Reopening re-resolves the last recorded persona by name, including an explicit clear, unless options explicitly select another root; a recorded persona that is now missing or subagent-only yields a diagnostic and no persona |
| `modes` | `fn modes(&self) -> Vec<Arc<ModeSpec>>` | Mode definitions in force now for a root session; empty for children |
| `keywords` | `fn keywords(&self) -> Vec<Arc<KeywordSpec>>` | Every current keyword definition for a root session, including ones disabled in settings or inactive for lack of tools, so settings UIs can list toggles; empty for children |
| `keyword_matches` | `fn keyword_matches(&self, text: &str) -> Vec<KeywordMatch>` | Runs the keyword matcher over `text`, treated as a message's first typed text part (a leading `/` token is skipped), against the currently active keywords. Returns `KeywordMatch { name, range }` in text order, `range` being a byte range into `text`, so a composer highlights exactly what a submission would fire; empty for children or with `keywords.enabled: false` |
| `set_mode` | `async fn set_mode(&self, name: Option<&str>) -> Result<(), Error>` | Persists a root mode selection; `None` clears it. Unknown names return `Error::ModeNotFound`; children return `Error::InvalidInput`. Instructions and limits take effect at the next request boundary |
| `mode` | `fn mode(&self) -> Option<Arc<ModeSpec>>` | Selected mode, including a selection waiting for the next request boundary; not necessarily the mode still governing an in-flight tool batch |
| `effective_mode` | `fn effective_mode(&self) -> Option<Arc<ModeSpec>>` | Mode applied at the latest request boundary, distinct from the selected `mode`; also `LiveSnapshot::effective_mode` and `SessionEvent::EffectiveMode` |
| `tools` | `fn tools(&self) -> Vec<SessionToolInfo>` | Read-only listing of the session's tools: every tool in the current prompt shape as `Exposure::Direct` (built-ins, host direct tools, and `call` when present), then every deferred catalog tool not already listed, as `Exposure::Deferred`. `SessionToolInfo` has `name`, `summary` (the deferred-catalog summary: first non-blank description line), full `description`, JSON Schema `parameters`, `exposure`, `promoted` (a deferred tool already promoted to native declarations), `allowed_in_mode` (`true` for `yield` and when no mode is effective, otherwise the effective mode's `ToolLimits::permits`, matching the call-time mode gate; host `ToolPolicy` decisions are not reflected), and `origin` (the `HostTool::origin` of the registration supplying it; `None` for built-ins and unlabeled host tools). Calling it never requests promotion |
| `accept_result` | `fn accept_result(&self, call_id: &str, options: AcceptResultOptions) -> Result<(), Error>` | Accepts a pending mode review with optional schema-validated `replacement: Option<Value>` and continuation `message: Option<String>`. Invalid replacements leave review pending. Unknown or settled ids return `Error::NoPendingResult` |
| `revise_result` | `fn revise_result(&self, call_id: &str, feedback: String) -> Result<(), Error>` | Settles the review with feedback while keeping the mode active. Blank feedback returns `Error::InvalidInput`; otherwise unknown or settled ids return `Error::NoPendingResult` |
| `diagnostics` | `fn diagnostics(&self) -> &[Diagnostic]` | Start-time diagnostics retained for the host and still logged as warnings; excludes runtime `NestedContext` read errors, which are only logged |
| `inspect_prompt` | `fn inspect_prompt(&self) -> PromptInspection` | `latest_built` is the latest local build, possibly not recorded; `recorded` replays system sections from the selected transcript. Both are `Option<IndexMap<String, String>>`. Does no rebuild or I/O; neither is a provider-wire receipt |
| `context_files` | `fn context_files(&self) -> ContextFilesInspection` | `startup: Vec<ContextFile>` is the loader inventory; `projected_nested: Vec<ContextFile>` contains nested instruction blocks still in projected tool results. No disk reread; overrides and projection can omit previously loaded instructions |
| `file_ledger` | `fn file_ledger(&self) -> HashMap<PathBuf, String>` | Read-only copy of full-file hashes keyed by canonical path, not a promise that files still match disk |
| `read_snapshot` | `async fn read_snapshot(&self, sha256: &str) -> Result<Vec<u8>, Error>` | One snapshot blob of this session, verified against its hash; see [File changes for hosts](#file-changes-for-hosts) |
| `squash_changes` | `async fn squash_changes(&self, changes: &[FileChange]) -> Vec<SquashedChange>` | The net change per file across `changes`, re-diffed from this session's blobs; see [File changes for hosts](#file-changes-for-hosts) |
| `schemes` | `fn schemes(&self) -> &Schemes` | This session's URL schemes: `exact` parses a URL, `resolve` exposes its target, and `read` reads UTF-8 text from file or virtual targets; a fork resolves to its own files. For binary or ranged reads, resolve and use file I/O |
| `jobs` | `fn jobs(&self) -> Vec<JobInfo>` | Command and child rows, command namespace first then child namespace, each in numeric order; finished rows included. The same list as `read proc://`; see [Background jobs for hosts](#background-jobs-for-hosts) |
| `kill_job` | `fn kill_job(&self, id: &str) -> KillReceipt` | Stops a command row (including retained helpers) or aborts a running child row. Returns `{ id, stopping }`; `stopping` is false for invalid/unknown ids, ended commands, or children without a live running session |
| `command_output` | `fn command_output(&self, call_id: &str) -> Option<CommandOutput>` | Opens output for the newest command the call started; see [Background jobs for hosts](#background-jobs-for-hosts) |
| `job_output` | `fn job_output(&self, job_id: &str) -> Option<CommandOutput>` | Opens a specific command’s output by job id (`bg_N`) |
| `command_jobs` | `fn command_jobs(&self, call_id: &str) -> Vec<String>` | Lists all the call’s command job ids, oldest first, including foreground commands |
| `background` | `fn background(&self, call_id: &str) -> bool` | Requests host promotion of every foreground command the call started. Returns false if none matches; a command that finishes first remains foreground |

Prompt input is a `PromptInput`; see [`prompts-and-context.md`](prompts-and-context.md#prompt-parts-cratesaxosrcinputrs) for each `PromptPart` and how parts reach the model.

`axo::ext::objects()` returns metadata for the four dispatched objects, including withheld methods on their dispatch blocks. A bare `#[expose]` validates withheld-only impls without registering a block. Each `Object { name, blocks: Vec<BlockMeta> }` lists blocks with a name and `entries: Vec<&Meta>`. A `Meta` gives the camelCase wire `name`, `rust_name`, `doc`, `params` (each with a name and optional flag), generated `args_type` and `result_type` names, `skip` reason, and `register` callback for type registration.

The persona follows the live selection across navigation: `navigate` re-records it on the new branch, so reopening restores the last live persona. Switching away from a restrictive persona restores tools and sections from the live unfiltered base rather than cumulatively filtering the prior shape. These setters may be called mid-run; changes apply from the next model request. Child sessions are rejected with `Error::InvalidInput`. `set_agent` is durable; host sections and tool factories are runtime resources, so hosts must re-supply them through `SessionOptions` when reopening. Forks inherit the current persona, host sections, and host factories. Changing the system prompt or tools is cache-heavy on models without mid-conversation system-message and tool changes; use `post` for instructions that toggle often.

### Root-session modes

Modes are a journal-derived, branch-scoped selection independent of the root persona. `Session::set_mode(Some(name))` records `Entry::ModeChange`; `set_mode(None)` exits. Neither call starts a run. Navigation and forks derive the selection from their selected journal path rather than carrying the current mode onto another branch. Reopening resolves the recorded name against the available definitions.

The selected mode is exposed by `Session::mode`, `LiveSnapshot::mode: Option<String>`, and `SessionEvent::Mode { mode: Option<String> }`. Instructions and tool limits become effective at the next request boundary, so changing selection during a tool batch does not change that batch's permissions. `ToolScope::mode` and a tool-originated `SpawnContext::mode` expose the effective mode rather than a queued selection; a host spawn (`SpawnOrigin::Host`) applies the selected mode, so `set_mode` followed by `Session::spawn` on an idle session already restricts the child.

Entry, switching, and exit are announced to the model through hidden, engine-owned `mode_notice` custom messages. Mode instructions do not replace the persona or rewrite the stable system prompt. Tool limits are execution gates for direct and routed calls, not merely catalog hints; they do not grant ordinary tools excluded by the persona. Mode `yield` remains available independently of the persona allowlist and is exempt from mode limits. Internal mode checks run before host tool policies, which still apply to `yield`.

A mode's optional model selector uses the [model selector grammar](configuration.md#model-selectors), commits together with the mode change, and restores the pre-mode selection on exit unless an explicit model, thinking, or tier change intervened; see [Model restoration](sessions-and-compaction.md#model-restoration).

Children do not inherit mode identity or instructions and cannot select modes. A child spawned under a mode with tool limits records them in `SpawnLink::limits`; its descendants inherit that same ceiling regardless of depth. The ceiling filters the child's shaped tool list after its persona allowlist, also removing `kill` and `send` when not permitted, and is exposed to spawn hooks as `SpawnContext::limits`. Ending the parent's mode therefore does not relax an already-created child's limits.

`ModeSpec::bash` and `AgentSpec::bash` take `BashAccess` (`axo::context::modes`, re-exported as `axo::BashAccess`): `Full` (default) or `ReadOnly`, written `bash: readOnly` in frontmatter. A session's effective access is `ReadOnly` when its root persona or child spec, its effective mode, or its child ceiling says so; it is exposed as `ToolScope::bash` and `SessionTools::bash_access`, and makes the built-in `bash` run every command with an `ExecSpec::sandbox` (see [Read-only bash](prompts-and-context.md#read-only-bash)). A spawned child records its parent's effective access at spawn time as `SpawnLink::bash` (omitted when `Full`), shown to spawn hooks as `SpawnContext::bash`; like tool limits, this ceiling passes to all descendants and never relaxes after spawn.

`ModeSpec::writes` and `AgentSpec::writes` (`Option<Vec<String>>`, frontmatter `writes: [...]`) scope where `write` and `edit` may write (see [Write scope](prompts-and-context.md#write-scope)). `None` adds no layer; an empty list adds a layer that permits nothing. `ToolScope::writes` and `SessionTools::write_scope` expose the effective `Option<WriteScope>`, with `None` meaning unrestricted. Both `WriteScope` and `WriteRoot` are re-exported by `axo`. `WriteScope` holds `layers: Vec<Vec<WriteRoot>>`, where a root is either `WriteRoot::Path(PathBuf)` (an absolute directory) or `WriteRoot::Url(String)` (a normalized scheme URL prefix). A target must match a root in every layer; file-backed URLs and directories compare by real path.

A spawned child records its parent's effective layers as `SpawnLink::writes` (omitted when unrestricted), shown to spawn hooks as `SpawnContext::writes`. The child's own spec, including a `SpawnDecision::Replace` replacement, can add a restriction but cannot remove an inherited layer. `writes` and `bash` are independent: a definition that wants shell writes contained too sets `bash: readOnly` as well.

#### Model entry and result review

Root sessions offer deferred `enter_mode` when at least one definition permits model invocation (`disable-model-invocation: false` in frontmatter, `ModeSpec::disable_model_invocation` in Rust). Its `mode` argument selects from those definitions; the persona allowlist and current mode's tool limits still govern whether it can run. The model reads `tool://enter_mode` and uses `call {"tool": "enter_mode", "args": {"mode": "<name>"}}`. Host selection and mode commands remain available for definitions with model invocation disabled. A successful call selects immediately, but instructions, permissions, and result schema change at the next request boundary.

A mode with `result` mounts its own deferred `yield`; without `result`, only host selection or a mode switch ends it. Read `tool://yield`, then use `call {"tool": "yield", "args": …}`. Schemas with an explicit `"type": "object"` use the arguments directly; all others wrap the value in `{"result": …}`. Schema validation happens before execution, so invalid arguments neither publish a result nor end the mode. Deferred promotion follows [`Model::tool_support()`](configuration.md#tool-list-capabilities); addition-only models never promote mode `yield`.

Execution requires the yielding mode to be both selected and effective. If a new selection has superseded it, even before the next request boundary, the old `yield` fails with `yield is unavailable: its mode is no longer active` and publishes no result.

For `result.review: true`, a valid yield remains in flight until the host responds:

1. `SessionEvent::ModeResult { call_id, mode, result, awaiting_review: true }` announces the value. Attach/reconnect supplies `LiveSnapshot::pending_results: Vec<PendingResult>`, where `PendingResult` has `call_id: String`, `mode: String`, `result: Value`, and `schema: Value`. Upsert by call id across snapshot and events; the event may duplicate a snapshot item.
2. Present the value to the user while continuing to receive events. Do not wait for `prompt` to finish before handling review: it is waiting for this decision.
3. Call `accept_result`, `revise_result`, or `dismiss` synchronously with that call id. A successful decision removes the pending item and emits `ModeResultResolved { call_id, outcome }`; this event means the review is no longer pending, not that the tool has finished persisting its result. `outcome: ResultResolution` (`axo::ResultResolution`) says why: `Accepted` (`accept_result`), `Revised` (`revise_result`), `Dismissed` (`dismiss`), `Superseded` (a mode change), `Steered` (a queued steer; see [Queued input for hosts](#queued-input-for-hosts)), or `Withdrawn` (cancellation, including shutdown).

Acceptance takes `AcceptResultOptions { replacement: Option<Value>, message: Option<String> }`; `Default::default()` accepts the original without a host continuation. A replacement is validated against the original pending schema, with invalid values leaving review pending. Accepted edits are conveyed to the model and retained alongside the original value.

Acceptance ends the mode through the normal persisted selection path, including model restoration, only if that same mode is still selected when the tool finishes. If selection changes after acceptance but before completion, the tool fails with `yield is unavailable: its mode is no longer active` rather than clearing the replacement mode. On successful completion it returns “Result accepted. Mode "<name>" has ended.” followed by the optional host message, or `result.then` when the host passes `None`. Any `Some` message, including an empty string, overrides `then` and continues the run. With neither continuation the tool sets `terminate: true`; the loop stops after the batch only if every result in that batch requests termination.

Revision leaves the mode selected and returns “The result was not accepted. Revise it and call yield again.” followed by the feedback, without requesting termination. Whitespace-only feedback is rejected before looking up the id. Dismissal leaves the mode selected and cancels the run as a user stop. Its outcome branch returns an error result (`is_error: true`) reporting “The result was dismissed by the user”, but cancellation can instead produce “Operation aborted”.

An actual mode change supersedes every pending review and emits its resolution event with `Superseded`; choosing the already-selected mode is a no-op and does not supersede it. Superseded tools return an error result reporting “The mode ended before this result was reviewed.” rather than accepting the old result; a review dropped without a decision reports the same way. Cancellation (including shutdown) withdraws a waiting review and emits its resolution event with `Withdrawn`, without accepting it or clearing the mode. Late decisions fail: `accept_result` and nonblank `revise_result` return `Error::NoPendingResult`; `dismiss` returns `Error::NoPendingQuestion` when neither registry contains the id.

The persisted `ToolResult` of a mode `yield` records `mode`, `outcome`, and `result` in `details`. For accepted results, `result` is the accepted value and `originalResult` plus `acceptedResult` retain both values, even when equal. Edited acceptance never rewrites the published original. Revised, dismissed, superseded, and steered outcomes retain the published `result`, including error results; cancellation, unavailable `yield`, and invalid arguments carry no review outcome. See [Review durability](sessions-and-compaction.md#mode-result-review).

For `result.review: false`, a valid yield publishes `ModeResult` with `awaiting_review: false`, creates no pending review and no corresponding resolution event, then exits the mode automatically if that same mode is still selected. Its completion guard, `then` continuation, and termination rules are the same as acceptance above. Reviewed yield calls use `Replay::Safe` so recovery can request review again; unreviewed yield calls use `Replay::Never`.

Roots with loaded modes install the stable `call` dispatcher even when the deferred catalog is empty, and reserve `yield` alongside `call` (see [Model entry and results](prompts-and-context.md#model-entry-and-results)).

### Spawning child sessions

`Session::spawn` is the host entry point for one child; `Session::spawn_team` starts a team. The model uses the [spawn tool](prompts-and-context.md#spawning-and-returning-results). All start first prompts asynchronously and report each settled run through the parent's job registry. Host calls return handles after creating and admitting the children, not after they finish; tool admission commits with its result. Each child has a separate journal and no copy of the parent's transcript. [Child sessions](sessions-and-compaction.md#child-sessions) describes delivery, recovery, parking, and budgets.

| Type | Fields / method | Contract |
| :--- | :--- | :--- |
| `SpawnRequest` | `agent: Option<String>`, `spec: Option<InlineSpec>`, `task: PromptInput`, `model: Option<String>`, `output: Option<Value>` | `agent` selects a catalog agent; `spec` supplies an inline one. They are exclusive; neither means a plain delegate. `task` is the first prompt. The explicit model selector wins over other model sources; `output` overrides the agent's result schema before spawn hooks run |
| `InlineSpec` | `prompt: String`, `tools: Option<Vec<String>>`, `model: Option<String>`, `output: Option<Value>` | Trimmed prompt becomes the agent body. Omitted tools keep the parent's available tools except `ask`, `yield`, `call`, and `enter_mode`; a supplied list must be a subset of those tools. The child gets its own dispatcher when needed and its own `yield` tool to enforce the output JSON Schema |
| `ChildHandle` | `job: String`, `session: Arc<Session>`; `fn receipt(&self) -> SpawnReceipt` | The parent's persistent child row and a live child handle. `receipt()` returns `SpawnReceipt::Child { job }`, matching tool details. Hosts can attach, prompt again after settling, steer, or abort through the session |
| `TeamRequest` | `team: Option<String>`, `members: Vec<TeamMemberRequest>`, `charter: Option<String>`, `tasks: IndexMap<String, PromptInput>` | Select either a catalog team or a non-empty ad-hoc roster. Every roster name requires a task; extra keys fail. Unlike the tool, this API has no shared task fallback or context parameter: prepare every member's input explicitly |
| `TeamMemberRequest` | `name: String`, `agent: Option<String>`, `spec: Option<InlineSpec>`, `brief: Option<String>` | Ordered ad-hoc roster entry. Agent and spec are exclusive; neither means a plain delegate. The brief is standing instruction text, not the member's task |
| `TeamHandle` | `team: String`, `members: Vec<(String, ChildHandle)>`; `fn receipt(&self) -> SpawnReceipt` | Per-parent team id (team_N) and member names with child handles in roster order. `receipt()` returns `SpawnReceipt::Team { team, jobs }`, matching tool details while retaining live handles |
| `SpawnHook` | `async fn before_spawn(&self, spawn: &SpawnContext<'_>) -> SpawnDecision` | Ordered host policy, supplied as `SessionOptions::host.spawn_hooks: Vec<Arc<dyn SpawnHook>>` (empty by default), inherited by children |
| `SpawnContext` | `spec`, `agent`, `mode: Option<&ModeSpec>`, `limits: &ToolLimits`, `bash: BashAccess`, `writes: Option<&WriteScope>`, `model`, `thinking`, `service_tier`, `task`, `depth`, `parent_session_id`, `origin: &SpawnOrigin`, `team: Option<SpawnTeam<'_>>` | Borrowed normalized spec, optional catalog agent name, the parent's mode applied to this spawn (selected for host spawns, effective for tool spawns), the inherited tool ceiling the child will run under (the mode's limits at a root, the parent's own ceiling in a child), the bash ceiling recorded as `SpawnLink::bash` (the parent's effective access), the write-scope ceiling recorded as `SpawnLink::writes` (the parent's effective layers, using the mode applied to this spawn), resolved model, thinking, and chosen tier, first prompt, child depth, parent id, requester and optional team identity |
| `SpawnTeam` | `name: Option<&str>`, `member: &str` | Catalog team name (`None` for ad-hoc teams) and roster member name, supplied to hooks for each member |
| `SpawnOrigin` | `Host`, `Tool { call_id: String }` | Distinguishes `Session::spawn` / `Session::spawn_team` from the model's `spawn` call, letting the same hook apply origin-specific policy |
| `SpawnDecision` | `Allow`, `Block(String)`, `Replace(AgentSpec)` | The first block rejects the spawn. A replacement keeps its own output schema rather than reapplying the request's override; it is normalized, its schema checked, and its model cascade resolved again before the next hook. The catalog name stays unchanged |

Child defaults come from user-only `subagents` settings. Model resolution considers the explicit request, the named agent's settings override, the spec, and the child default, then inherits the parent's live model; see [Child-session settings](configuration.md#child-sessions-subagents--axosubagentsettings). Missing or team agent names return `Error::AgentNotFound`; conflicting agent/spec inputs, unavailable inline tools, and invalid output schemas return `Error::InvalidInput`. A spawn hook, the depth limit, or a shutting-down parent can reject with `Error::SpawnBlocked`.

For `Session::spawn_team`, a missing catalog name is `Error::AgentNotFound`; selecting an agent as a team, setting both selectors, giving a catalog team a charter, invalid/duplicate/reserved roster names, conflicting member agent/spec, or incomplete/extra task keys is `Error::InvalidInput`. Roster name `parent` is reserved. Teams exceeding `subagents.maxConcurrent` return `Error::SpawnBlocked`. Every member's spec and hook decision is resolved before any child is created; there is no call-level model or output override. Members use the usual child model cascade and their own result schemas. See [Teams](prompts-and-context.md#teams) for file format and model-facing usage.

Children inherit the resolved configuration, cwd, execution environment, supervisor, store, models, streamer, agent catalog, current host tool registrations (direct and deferred, filtered by the child's allowlist), current host prompt sections, schemes, tool policies, and spawn hooks. They disable `ask` and receive fixed delegation framing plus their resolved spec. `SpawnLink::spec` persists the spec after defaults and hooks, so later agent-file edits do not change it. Root-only personas are omitted from the model's spawn catalog and rejected with `Error::AgentNotSubagent`, including when selected as a team member.

`Axo::session(&self, id: &str) -> Result<Arc<Session>, Error>` is async and returns a known live child or revives a parked one, resuming unfinished work. Children become known when spawned or when their root opens in this process; an unknown id first waits for reopened sessions to finish registering their children, then returns `Error::SessionNotFound`. `Axo::open_session` rejects child files with `Error::ChildSession { path, root }`, naming the owning root when its chain is found: open that root, keep its `Session` alive (dropping it detaches its children), and use `Axo::session` instead; see [Session files](sessions-and-compaction.md) for the full flow. `Session::spawn_link()` returns the child's `SpawnLink`, and session listings include `SessionInfo::spawn`.

`Axo::observe_child(id: &str) -> Option<(broadcast::Receiver<Arc<SessionEvent>>, CancellationToken)>` subscribes only to a resident child, without reviving it. Its token is canceled when the child loses residency; canceling the token yourself does not affect the child or other observers.

`Axo::spawn_parent(id: &str) -> Option<String>` returns a known child's parent session id, live or parked, without reviving it; `None` for a root session or an unknown id (which first waits for reopened sessions to finish registering their children).

### Host messages

`Session::post` takes `HostMessage { custom_type: String, text: String, display: bool, delivery: HostDelivery }`. The text enters model context as a custom message (lowered to a user message for model requests); `custom_type` identifies its host-defined purpose and `display` tells clients whether to show it. Hiding it in the UI does not hide it from the model. Unlike `append_custom`, which stores metadata only, `post` supplies model-facing content.

| `HostDelivery` | Active run | Idle session |
| :--- | :--- | :--- |
| `Steer` | Read at the next turn boundary, after the current tool batch; signals pending steering to tools | Starts a run |
| `FollowUp` | Read when the run would otherwise finish | Starts a run |
| `Quiet` | Read at the next turn boundary without steering tools | Waits for the next run; never starts one |

User input and host `Steer` / `FollowUp` clear the automatic-continuation pause; `Quiet` does not. Queueing is durable: `post` stores one `Text` prompt part plus `SubmittedCustom { custom_type, display }` on `Entry::Submission` before delivery. Host text is not a user slash command: skill expansion is bypassed both on initial delivery and recovery.

Use `post` for changing host instructions or notices that belong in the conversation. It preserves the system prompt and tool definitions, keeping their stable request prefix cacheable; repeatedly changing `set_section` or `set_tools` for per-turn notices would change that prefix. Those setters remain appropriate for actual changes to the session's standing instructions or available tools.

`Session::is_running()`, `LiveSnapshot::running` / `maintenance`, and `SessionEvent::Activity` expose ownership separately from `auto_paused` and background job status. An idle observation is advisory: another submission or automatic wake can claim the session before the host's next operation.

### Queued input for hosts

Every queued steer or follow-up is delivered, held visibly with a way to send it, or withdrawn by the host.

**Listing.** `LiveSnapshot::queued: Vec<QueuedInput>` holds the input waiting in the agent's queues: steers, then follow-ups, each in delivery order. `QueuedInput { submission_id, kind, parts, custom }` carries the submission id, its `SubmissionKind` (`Steer`, `FollowUp`, or `Prompt` for input requeued when the session reopened, delivered like `Steer`), its `PromptPart`s, and `custom: Option<SubmittedCustom>` for messages submitted via `post`. Host `post` messages with `HostDelivery::Steer` / `FollowUp` and routed steers appear too; mail and `Quiet` messages do not. Refresh the live queue snapshot on `SessionEvent::QueueChanged`, which signals a queued steer or follow-up or restoration after a failed withdrawal. For removals, an `Entry::Message` whose `submission_id` names an item, or an `Entry::SubmissionWithdrawn`, removes it. Reattach after broadcast lag.

**Held input.** Input is held when `auto_paused && !running` and the list is non-empty. The latch behind `auto_paused`:

- A Stop (`abort`) during a run latches when pressed; a run that fails (`RunOutcome::Failed`, after retries are exhausted) latches when it settles. A run that ends `Done` never latches.
- Input still queued when a Stop latches, or when a failed run settles, stays held: nothing delivers it automatically.
- `resume_wakes`, `steer`, `follow_up`, `prompt`, or a host `Steer` / `FollowUp` release it, and everything held is delivered with the next run (`resume_wakes` starts that run itself on an idle session).
- Input sent after a Stop clears the latch, so it and everything held are delivered once the stopped run settles.

**Operations.**

- Cancel: `withdraw(submission_id)`.
- Edit: `withdraw(submission_id)` and load the returned input into the composer. If `custom` is `None`, resubmit `parts` with `steer` or `follow_up`, editing them as needed. The parts can also be resubmitted unchanged; files behind `PromptPart::Saved` stay on disk. If `custom` is `Some`, take the text from the single `PromptPart::Text` and resubmit it with `post`, using `custom.custom_type` and `custom.display` in the `HostMessage` and the corresponding `HostDelivery`.
- Promote a follow-up to a steer: `withdraw(submission_id)` and resubmit as above, using `steer` for ordinary input or `post` with `HostDelivery::Steer` for a host message.
- Interrupt and send now: `abort()`, wait for `SessionEvent::Activity` with `running == false`, then `resume_wakes()`.

`withdraw` waits for any submission already being queued to finish before looking for the input. It records an `Entry::SubmissionWithdrawn`, so reopening never requeues the input, and returns `Error::NotQueued` once the input was delivered or withdrawn. If writing that entry fails, it restores the input at its original position in the queue, triggers delivery if the session is idle and automatic delivery is allowed, and returns the write error. It does not clear the Stop latch.

**Steers settle waits for the user.** A queued steer (`Session::steer`, `HostDelivery::Steer`, or a routed steer) raises `SteerSignal`, and the built-in tools that wait for the user stop waiting instead of holding the steer behind them. Nothing is aborted: the tool returns a non-error result and the steer is delivered right after it.

| Waiting tool | Host-visible effect | Tool result text | `details.outcome` |
| :--- | :--- | :--- | :--- |
| `ask` | `QuestionResolved`; a later `answer` or `dismiss` returns `Error::NoPendingQuestion` | “The user did not answer. They sent a message instead; it follows this result.” | `"steered"` |
| reviewed mode `yield` | `ModeResultResolved { outcome: ResultResolution::Steered }`; the mode stays selected; a later `accept_result` or `revise_result` returns `Error::NoPendingResult` | “The result was not reviewed: the user sent a message instead; it follows this result. The mode is still active; call yield again when the result is ready.” | `"steered"` (with `mode` and `result`) |

A steer already queued when `ask` or a reviewed `yield` starts returns the same result without announcing the question or review. Follow-ups, mail, and `Quiet` messages never settle either.

### Ask documents for hosts

The `ask` tool (registered when `SessionOptions::ask_enabled` is `true`) sends one document: `blocks` in reading order, each Markdown or a typed question. `SessionEvent::QuestionPending { call_id, blocks }` and `LiveSnapshot::pending_questions` carry it as `Vec<AskBlock>`. Render the blocks in order, then settle the call with `Session::answer(call_id, answers)` or cancel it with `Session::dismiss(call_id)`.

| Block `type` | Rust type | Fields |
| :--- | :--- | :--- |
| `markdown` | `MarkdownBlock` | `text` |
| `choice` | `ChoiceQuestion` | `id`, `question`, `optional`, `options` (each a `QuestionOption`: `value`, `label`, `description`, `recommended`), `multi` |
| `text` | `TextQuestion` | `id`, `question`, `optional`, `multiline`, `placeholder`, `prefill` |
| `number` | `NumberQuestion` | `id`, `question`, `optional`, `integer`, `min`, `max`, `default` |

Axo checks the model's call before announcing it and returns each failure to the model as a tool error, so a host never sees an invalid document: at least one question; question ids non-blank and unique; a `choice` has at least two options with non-blank, unique values, and a single-select one recommends at most one; a `number` has `min` ≤ `max`, a `default` inside them, and when `integer` is set a whole `default` and at least one whole number between `min` and `max`.

**Rendering.** Axo passes Markdown through unparsed; the host renders it with whatever it has. The tool description only asks the model for Markdown, so a host may tell the model what it can render however it likes; host prompt sections (`SessionOptions::host.sections`, `Session::set_section`) are one way, and Axo prescribes no section name. The host always supplies a free-text "Other" answer on every `choice` (the model is told never to add one) and may offer one note per question. `recommended`, `prefill` and `default` are the model's pick, natural starting values for the controls.

**Submission.** Send one `Answer { id, value, other, note }` per answered question; `value` is an `AnswerValue` (`Text`, `Number` or `Many`, serialized untagged as a string, number or string array). Blank means empty after trimming: blank `other`, `note` and `Text` values count as absent. A question is skipped when it has no answer, or its answer has neither a `value` (absent, blank text or an empty list) nor `other`.

| Question | Accepted |
| :--- | :--- |
| any | each answer names a question of the document, at most once; a skipped question must be `optional` and carry no `note` (a note is only valid with an answer) |
| single `choice` | `value` is one option `value` (`Text`), or `other` is set; not both; optional `note` |
| multi `choice` | `value` is a list (`Many`) of distinct option values, optionally with `other`; optional `note` |
| `text` | `value` is `Text`; no `other`, no `note` |
| `number` | `value` is a `Number` within `min`/`max`, whole when `integer`; no `other`; optional `note` |

A submission that breaks a rule returns `Error::InvalidInput` naming the question; the question stays pending and no `QuestionResolved` is sent, so the host can show the error and let the user fix the answer.

**Result.** The model receives `User answers:` followed by one pretty-printed JSON object keyed by question `id` in document order; the tool result `details` carries the same object under `answers`, so a host can rebuild a settled card from the call and its result alone. A skipped question is `null`; otherwise the entry holds `value` (the option `value`, never a label; a list for multi-select, always present there; the text; or the number), plus `other` and `note` when given. On a single-select, `other` replaces the pick.

```json
{
  "store":    { "value": "sqlite", "note": "keep the schema portable to Postgres" },
  "features": { "value": ["ci"], "other": "terraform" },
  "ttl_days": { "value": 30 },
  "comment":  null
}
```

**Timeout.** With `ask.timeoutMs` set (see [configuration](configuration.md#question-tool-ask--axoasksettings)), an unanswered call settles with only what the model marked: the `recommended` option(s), a non-blank `prefill`, or the `default`, each as `{ "value": … }`, and `null` for any question without one, required or not. The first option is never guessed. The text says the user did not answer in time, and `details` also has `"timedOut": true`; the result is not an error.

`dismiss` makes the call an error result ("Ask tool was cancelled by the user") and stops the run; an empty submission is not a cancellation. A queued steer settles the call as described in [Queued input for hosts](#queued-input-for-hosts).

### Context usage

`ContextUsage { tokens: u64, window: u64, trigger: u64, reserve: u64 }` describes current context, not historical spend: `tokens` uses the compaction estimator over the current projection, `window` is the effective model context window, `reserve` is the configured compaction headroom, and `trigger` is `window - reserve`. Limits reflect the current model and compaction settings. `Session::context_usage()` and `LiveSnapshot::context` return `None` when those limits cannot be resolved.

`SessionEvent::Context` refreshes this state after assistant responses, context rewrites, and model or persona changes. Use the snapshot as the initial value and subsequent events as replacements. `LiveSnapshot::auto_paused` and `SessionEvent::AutoPaused { paused }` similarly expose whether automatic continuation is paused; this is separate from whether a run is currently active, and from whether jobs are running. A Stop during a run sets the pause when pressed, and a run that fails (error, interruption, persist or journal failure) sets it when it settles; user input, a host `Steer` / `FollowUp`, or `Session::resume_wakes` clears it, while incoming teammate mail stays queued unread without clearing it (see [Queued input for hosts](#queued-input-for-hosts) and [Background Job Durability](sessions-and-compaction.md#background-job-durability)).

### `SessionEvent`

Emitted by `attach().events` (`broadcast::Receiver<Arc<SessionEvent>>`; every receiver shares one allocation per event, so match through the `Arc`):

| Variant | Fields | Meaning |
| :--- | :--- | :--- |
| `Agent(AgentEvent)` | `AgentEvent` | Passthrough of stateful turn loop events (model streaming, tool call execution, message completions) |
| `Activity` | `Activity { running: bool, maintenance: Option<MaintenanceKind> }` | Authoritative session ownership state (run, maintenance, or navigation), synchronized with attach; not an idle reservation |
| `Entry(SeqEntry)` | `SeqEntry` | New entry persisted to the session store |
| `QueueChanged` | — | Refresh the live queued inputs; see [Queued input for hosts](#queued-input-for-hosts) |
| `QuestionPending` | `call_id: String`, `blocks: Vec<AskBlock>` | An `ask` tool call needs user input; see [Ask documents for hosts](#ask-documents-for-hosts) |
| `QuestionResolved` | `call_id: String` | A pending question was answered, dismissed, timed out, or settled by a queued steer |
| `MaintenanceStart` | `kind: MaintenanceKind` | Compaction, `/shake`, or branch summary started |
| `MaintenanceEnd` | `kind: MaintenanceKind`, `error: Option<String>`, `aborted: bool` | Maintenance task finished |
| `Retry` | `attempt: u32`, `delay_ms: u64`, `max_retries: u32`, `error: Option<String>`, `failure: Option<axo_ai::ProviderFailure>` | Turn or summary retry: retry number, selected delay, configured limit, and available provider error text/structured failure |
| `Job` | `JobInfo` | A listed job appeared or changed: a command or child row's state after a promotion, exit, admission, run start, or run message |
| `JobGone` | `id: String` | A job left the list |
| `Context` | `ContextUsage` | Current context estimate and model window, compaction trigger, and reserve |
| `AutoPaused` | `paused: bool` | Automatic-continuation pause state changed |
| `TreeUsage` | `usage: Usage` | Whole-tree spend (`Session::tree_usage`) changed: this session's own usage or a descendant's grew. Replaces the previous value |
| `Mode` | `mode: Option<String>` | Selected root mode changed; `None` means no mode. Instructions and limits apply at the next request boundary |
| `EffectiveMode` | `mode: Option<String>` | Mode now governing request-boundary instructions and tool limits, distinct from the selected `Mode` |
| `ModeResult` | `call_id: String`, `mode: String`, `result: Value`, `awaiting_review: bool` | Valid mode result; only `awaiting_review: true` requires a host decision and appears in the pending snapshot |
| `ModeResultResolved` | `call_id: String`, `outcome: ResultResolution` | Review removed after a decision (`Accepted`, `Revised`, `Dismissed`), superseding mode change (`Superseded`), queued steer (`Steered`), or cancellation (`Withdrawn`); remove its UI item. The tool's persisted result may follow later |

`SessionEvent::kind()` returns the `SessionEventKind` tag without serializing the payload, for filtering. The tag serializes in camelCase.

Events arrive in one total order per session. `attach` subscribes while holding the journal, live, mode, job-registry, and tree-usage locks, then captures pending questions and results. State-valued events such as `Job`, `Context`, `AutoPaused`, and `TreeUsage` may repeat a value the snapshot already holds. Pending-review events can overlap their snapshot: upsert `ModeResult` by call id and remove on `ModeResultResolved`; reattach after broadcast lag to rebuild live state.

For a promoted command, the stream reports the running row, then the finished/failed row with typed exit and message delivery `Writing`, then its persisted `Entry::JobEnd`, and then the row with delivery `Pending` (or its later state). Entries carry the durable result text; row updates carry state, not a second result transcript.

### Background jobs for hosts

`bash` commands that move to the background become jobs `bg_N` (see [Background Jobs](prompts-and-context.md#background-jobs-cratesaxosrcjobsmodrs-cratesaxosrcjobscommandrs)); spawned children have persistent rows `agent_N`. The model reaches both through `proc://`; hosts get the same state as data without parsing that text:

- **State:** `Session::jobs()` returns `Vec<JobInfo>`, and `attach().live` carries the same list as `LiveSnapshot::jobs`.
- **Spend and selection:** `LiveSnapshot::usage` is the session's own spend: `Entry::usage()` summed over its whole journal, across branches, including maintenance and failed responses, without counting `run_end` or `job_end` rollups again. `LiveSnapshot::tree_usage` (also `Session::tree_usage()`, kept current by `SessionEvent::TreeUsage { usage }`) adds every descendant's own spend, computed the same way from each child's journal, so it includes child compaction and branch summaries and grandchildren. Each child row's `JobKind::Child::tree_usage` is that child's subtree share. Do not total `Entry::JobEnd` usage for cost: it covers one child run's assistant messages only, excluding the child's maintenance and its descendants. The snapshot also carries the live `model`, `thinking`, chosen `service_tier`, and selected persona name in `agent`; getters expose the same current selection.
- **Changes:** each listed row that appears or changes emits `SessionEvent::Job(info)`, and a row leaving the list emits `SessionEvent::JobGone { id }`; upsert rows by `id` over the snapshot. Foreground commands that are never listed emit nothing.
- **Stop:** `Session::kill_job(id)` returns `KillReceipt { id, stopping }`; the job outcome is then journaled and delivered normally. Commands record `CommandExit::Killed { by: Killer::Host }` and read `Command was stopped by the user`; model calls to `kill` record `Killer::Model` instead. Model `kill` and `send` serialize typed [coordination receipts](#coordination-receipts) into result details. `proc://` is read-only.
- **Scheme text:** `session.schemes().read(&url).await` resolves file targets and reads them as UTF-8, while virtual targets use their handler. A missing file reports `{url} not found`; binary data reports `{url} is not UTF-8 text; resolve it and read the file`. Use `CommandOutput::read` for live command byte ranges, or resolve a file target and perform host file I/O.
- **Replay:** `jobs_from_entries(&header, &entries)` reconstructs listed rows without opening a live session. Supply all raw entries, as returned by `Axo::open_session_read`, not just a projected branch. An unfinished command remains `Running`; replay does not reconcile it or invent live-only facts.
- **Output:** `command_output(call_id)` opens the newest command the call started; `command_jobs(call_id)` lists all its command IDs, oldest first, and `job_output(job_id)` opens one by ID. These lookups include unlisted foreground commands and retained output after reopen; a missing or unopenable spool yields no handle. A `CommandOutput` provides `job()` (`bg_N`), `artifact()` (numeric artifact id), `path()` (the raw log file), `progress()` (a watch receiver of `OutputProgress { bytes, done, capture_failed }`), and async `read(range: Range<u64>)` / `read_chunks(range: Range<u64>)`. Both ranges use byte offsets into the raw merged spool and clamp to its flushed prefix. `read_chunks` returns `Vec<OutputChunk>`, each with `stream: OutputStream` (`Stdout`, `Stderr`, or `Merged` for bytes without stream metadata, including an unindexed tail or a live log without a readable index) and `bytes: Vec<u8>`. Cross-pipe order is capture order, not guaranteed process-write chronology. `done` means capture ended, including retained helpers; `capture_failed` distinguishes incomplete capture. A held handle stays readable after foreground files are unlinked on Unix. No stdin or detach API is exposed; background commands die with their session.
- **Retained output:** `CommandOutput::open(job: String, artifact: u64, path: impl AsRef<Path>) -> io::Result<Self>` opens a retained, closed, or copied raw log with its `<raw-log>.streams` sidecar. Raw `read` and model-facing artifacts remain merged bytes; the sidecar retains stream labels. Missing sidecars on older logs produce `Merged` chunks rather than invented labels. An indexed log without a clean terminal record matching the log length reports incomplete capture. Forks snapshot both files for running commands instead of hard-linking mutable output.

`axo::schemes::artifact::Artifacts::allocate` returns `io::Result<(u64, PathBuf)>`. It and `create` reject a tail of `streams` or ending in `.streams` with `io::ErrorKind::InvalidInput` before using an ID; extension calls receive `invalid_input`. `find` ignores sidecars and `live()` includes sidecars for its live artifacts.

`JobInfo` (`axo::JobInfo`) has `id: String`, `status: JobStatus`, `call_id: Option<String>` (`None` for host-spawned children), `started_at: i64` (epoch milliseconds: command spawn or child admission time), `kind: JobKind`, and `messages: Vec<JobMessageInfo>`. Messages appear in sequence order with `id`, `note`, `is_error`, `partial`, `run_id`, and `delivery`. `MessageDelivery` is `Held`, `Writing` (journal write in flight), `Pending`, `Reserved`, or `Delivered`. Text, typed child result value, and per-run child usage remain in the corresponding `job_end` entry rather than the row; the row carries only the cumulative subtree total in `JobKind::Child::tree_usage`.

| `JobKind` | Fields | Meaning |
| :--- | :--- | :--- |
| `Command` | `command: String`, `tool: String`, `cwd: PathBuf`, `artifact: u64`, `promotion: Option<Promotion>`, `promoted_at: Option<i64>`, `ended_at: Option<i64>`, `exit: Option<CommandExit>` | Tool-supplied display label, tool name, working directory, merged output spool, promotion cause and times, and root-process outcome. Times are epoch milliseconds; promotion is `Async`, `Timer`, `Steer`, or `Host`. Never-promoted retained helpers have no promotion |
| `Child` | `session_id: String`, `path: PathBuf`, `depth: u32`, `agent: Option<String>`, `model: String`, `team: Option<TeamRef>`, `resident: bool`, `progress: Option<Box<ChildProgress>>`, `unread_mail: u32`, `tree_usage: Usage` | Child identity, path, nesting depth (root children are depth 1), optional catalog agent, expanded provider/model, and team identity. `resident` is true while the parent's session table holds the child pinned (from creation, while queued or working, and after revival) and false once parked, even if a host still holds an `Arc<Session>` keeping it loaded. Progress describes the current or last observed run; it is absent before the first completed assistant message and after reopen until new progress arrives, and is boxed only to keep child rows near command-row size (fields read through it unchanged). `unread_mail` counts teammate mail queued in the child and not yet read; it lives on the row rather than in progress because an idle auto-paused member keeps its mail unread until it next runs. It updates whenever mail is posted or taken, and rows rebuilt from the journal start at 0. `tree_usage` is the child's whole-subtree spend (its own journal plus its descendants, maintenance included), equal to that child's `Session::tree_usage()`; it updates live as the subtree spends and is rebuilt from child journals on open (see [Tree usage](sessions-and-compaction.md#tree-usage)). `jobs_from_entries` cannot read child files and leaves it at zero |

`CommandExit` is `Exited { code }`, `Signaled { signal }`, `TimedOut { after_secs }`, `Killed { by: Killer::Model | Killer::Host }`, `Aborted` (foreground cancellation), or `Interrupted` (stopped by `Session::shutdown`, reading `Command was interrupted when the session closed`, or reconciled on reopen). A process-wait failure can leave `exit` absent. Promotion causes and killers serialize in camelCase; exits use a camelCase `type` tag and fields.

`JobInfo`, `JobStatus`, `ChildProgress`, `JobMessageInfo`, `MessageDelivery`, and `OutputProgress` serialize in camelCase. `JobKind` uses a `type` tag (`command` or `child`) and camelCase fields. `JobStatus` values such as `helpersRunning` are distinct from the human-readable `label()` (`helpers running`).

`ChildProgress` contains `turns: u32` (request count), `max_turns: u32` (configured budget), `wrapping_up: bool` (request count has reached the budget), `yield_failures: u32` (rejected output attempts, zero without an output schema), `usage: Usage` (persisted assistant spend for the run), and `tools: Vec<String>` (names of the child's tools executing now, in start order; parallel calls list each name, finished calls drop out, and the list is empty when the run ends). Child state changes, progress, tool starts and ends, unread-mail changes, and parking/revival publish `SessionEvent::Job` updates.

`TeamRef` (`axo::TeamRef`) has `id: String` (`team_N`, numbered per parent), `member: String` (roster name), and `name: Option<String>` (the catalog team name; `None` for ad-hoc teams). There is no separate team state type: hosts group `jobs()` rows by `team.id` and treat a member as waiting while its `progress.tools` contains `"wait"`. A team deadlock is never a lasting state, because mutual-wait detection fails the waits as soon as it finds one (see [`wait`](prompts-and-context.md#wait)).

`JobStatus` (`axo::JobStatus`) is `Queued` (child claimed but waiting for a run slot), `Running` (child owns its slot or command root is active), `HelpersRunning` (commands only: retained supervised processes), `Finished`, or `Failed`. Otherwise a child's latest non-note run message determines success/failure (no run message means failed). Mid-run notes never settle a child or change its completed-run status. `JobStatus::label` returns the `proc://` wording (`queued`, `running`, `helpers running`, `finished`, `failed`); `agent://` uses the same child labels, with no separate idle state. `Axo::schemes` has none of `proc://`, `agent://`, or `tool://`; only open sessions expose these live schemes. The [agent scheme](prompts-and-context.md#agent) documents status, results, and transcripts; messaging uses [`send`](prompts-and-context.md#send) and stopping uses [`kill`](prompts-and-context.md#kill). Parent `shutdown` also shuts down children, while dropping a parent need not stop a child held independently by the host.

### File changes for hosts

A tool result can report multiple file accesses in `details.files`, even if the call failed. Each record has a `ledger` (absolute path, SHA-256 hash, and `read` or `modify` access; see `FileLedger::restore`). A write or edit that changes a file also includes a `change: FileChange` in that record. Both sides' raw bytes are stored as content-addressed blobs under `<session>/snapshots/<sha256>`, named by `FileLedger::hash` (43-character base64url SHA-256) and written read-only. `bash` writes and virtual scheme writes record nothing. Blobs are never pruned: compaction and `/shake` leave `snapshots/` alone, so changes stay reviewable for every turn of the file.

`axo::file_changes(&ToolResultMessage) -> Vec<FileChange>` returns the mutations in record order. `axo::file_records` includes reads and unchanged writes too. Both return an empty list for missing or invalid file records. All types serialize in camelCase; `Option` fields are omitted when `None`.

| Type | Fields |
| :--- | :--- |
| `FileChange` | `path: String` (absolute target, the same value as `ledger.path`), `key: String` (canonical file identity: group changes by it), `url: Option<String>` (the scheme URL the call used), `created: bool`, `before: Option<SnapshotBlob>` (absent iff `created`), `after: SnapshotBlob`, `diff: Option<LineDiff>` (absent when either side is binary or over `MAX_SNAPSHOT_BYTES`) |
| `SnapshotBlob` | `sha256: String` (always present), `bytes: u64`, `binary: bool` (contains a NUL byte or is not valid UTF-8), `omitted: Option<Omitted>`: `TooLarge` (over `MAX_SNAPSHOT_BYTES`, 4 MiB) or `IoError` (the blob write failed); an omitted blob is not stored |
| `LineDiff` | Over BOM-stripped, LF-normalized text: `additions: usize`, `deletions: usize`, `first_changed_line: Option<usize>` (1-based, in the new text), `patch: Option<String>` (unified, 4 context lines, headed by the URL or else the absolute path; absent for created files, when no line changed, or over 64 KiB), `patch_truncated: bool` (serialized only when true: a patch existed but exceeded 64 KiB) |
| `SquashedChange` | `path`, `key`, `url` (from the last mutation), `created` and `before` (from the first), `after` (from the last), `mutations: usize`, `discontinuous: bool`, `diff: Option<LineDiff>` |

Hosts read blobs with `Session::read_snapshot(sha256)` on an open session, or `Axo::read_snapshot(session_path, sha256)` for any session file (open, closed, forked, or child) without taking its lock. Both verify the bytes against the hash: a malformed hash is `Error::InvalidInput`, and a missing or mismatching blob is `Error::Io` (`ErrorKind::InvalidData` for a mismatch).

To show a range of turns as one review, collect its `FileChange`s in transcript order and squash them. `axo::squash(changes)` is pure: it groups by `key` in first-appearance order, keeps the first `before` and last `after`, and sets `discontinuous` when a mutation's `before` is not the previous one's `after` (the file changed outside the file tools in between, so the net diff is not only the agent's work). A file that ends where it began is dropped unless it was created or is discontinuous. `diff` is the stored diff for a single mutation and `None` for several; `Session::squash_changes(changes)` and `Axo::squash_changes(session_path, changes)` also re-diff multi-mutation files whose first `before` (empty when created) and last `after` are stored text, leaving `diff: None` if a blob cannot be read.

For the changes on one branch, walk `Projection::path()` indices over the raw entries (from `Attach::missed` or `Axo::open_session_read`), not `LiveSnapshot::messages`, which drops compacted turns. A fork carries the blobs its copied path references (see [Forking](sessions-and-compaction.md#attaching-navigating--forking-session)); a child session keeps its own blobs under its own session directory.

### Coordination receipts

Model `kill`, `send`, `wait`, and `spawn` results serialize the same public serde receipt types used by host integrations:

| Type | Fields / variants |
| :--- | :--- |
| `KillReceipt` | `id: String`, `stopping: bool` |
| `SendReceipt` | `to: String`, `kind: SendKind`, `session_id: String` (resolved destination), `receipt: SendIdentity` |
| `SendKind` | `Note`, `Steer`, `FollowUp`, `Mail` (camelCase JSON) |
| `SendIdentity` | `Submission(String)` (submission UUID), `Message(String)` (parent job-message reference); JSON `{"type":"submission" \| "message","id":"…"}` |
| `WaitReceipt` | `reason: WaitReason` (`Results`, `Mail`, `Interrupted`, `Idle`, camelCase JSON), `jobs: Vec<String>` (message references, omitted when empty) |
| `SpawnReceipt` | Untagged `Child { job: String }` or `Team { team: String, jobs: IndexMap<String, String> }`; existing spawn receipt shape |

`Session::send` uses the model's routing rules, including parked-child revival, parent notes, teammate mail, and child-only steering. Child steer/follow-up and teammate submissions retain optional `SubmissionProvenance { session_id, route: SendKind, name: Option<String> }`: originating session, actual route, and optional roster name. Provenance is not a fabricated user sender. `Session::post` remains for host-defined custom messages to the current session.

### Commands

`axo` owns the command catalog, argument validation, the `/name args` text parser, and dispatch. Hosts decide which commands to offer and in what form: a slash menu, buttons, a model picker, or nothing. A built-in only runs when the host calls `run_command`, so a host never inherits a command it does not expose. App-only commands (opening settings, a new chat) stay in the app.

| Command | Text form | `CommandPolicy` | Effect |
| :--- | :--- | :--- | :--- |
| `compact` | `/compact [focus…]` | `WhenIdle` | `compact(focus)` |
| `shake` | `/shake <elide\|images\|thinking>` | `WhenIdle` | `shake(mode)` |
| `model` | `/model <selector>` | `Alongside` | Select a model, thinking level, or service tier using the [model selector grammar](configuration.md#model-selectors) |
| `thinking` | `/thinking <level>` | `Alongside` | `set_thinking(level)` |
| `Command::Tier` | `/tier` followed by the tier name | `Alongside` | `set_service_tier` |
| `navigate` | `/navigate <entry> [true\|false] [focus…]` | `WhenIdle` | `navigate(Some(entry), NavigateOptions { summarize, focus })`; focus requires `true` |
| each skill | `/<name> <free text>` | `Queued` | Prompt text: expanded, then sent through whichever of `prompt`, `steer`, or `follow_up` the app used |
| each mode | `/<name> [prompt…]` | `Alongside` | `Command::Mode { name, prompt }` selects the mode, then sends a nonblank optional prompt through `steer`; without a prompt it does not start a run |
| each registered command | `/<name> [args…]` | `Alongside` | `Command::Registered { name, args }` awaits the host's `CommandHandler::run(session, args)` |

- **Policies:** `Alongside` commands run during a turn or maintenance and take effect from the next request; `WhenIdle` commands return `Error::Busy` during a turn or maintenance.
- **Typed commands:** use `Session::parse_command(text)` before prompting to recognize built-ins, this session's modes and its registered commands. `Command::parse(text)` remains a built-in-only parser. Both return `None` for unrecognized input; skills and unknown `/word`s stay prompt text. Built-in positional words map to schema properties in order, honoring single and double quotes; a final property with `"format": "text"` (the `focus` of `compact` and `navigate`) takes the remaining text verbatim and trimmed. Invalid arguments return `Error::InvalidCommand { command, message }`. Mode and registered-command parsing take the text after the name verbatim and trimmed, without quote decoding; blank mode text becomes `None`, blank registered-command text an empty `args`. `run_command` selects the mode before steering that text, which is persisted as a normal steering submission. A built-in or registered command itself is never saved as a submission (though `compact` and summarizing `navigate` may request a model summary).
- **Registered commands:** hosts add `/name` commands through `SessionOptions::host.commands` or `Session::set_commands` as `RegisteredCommand { name, description, argument_hint, origin, handler: Arc<dyn CommandHandler> }`; `CommandHandler` has one method, `async fn run(&self, session: &Session, args: &str) -> Result<(), Error>`. Forks inherit the live list; children have none. A name that is empty or contains whitespace or `/` is ignored (`command name "<name>" is invalid; ignored`); `axo::valid_command_name` applies this rule. A built-in's or mode's name hides the command (`command "/<name>" from <origin> is hidden by the built-in /<name> command`, `… is hidden by the mode /<name>`); among duplicates the last registration wins at the first one's position (`command "/<name>" from <winner> replaces the one from <loser>`), where an unlabeled origin reads `the host`. A command hides a same-name skill with the skill's path diagnostic `skill name "<name>" is hidden by the command /<name>; the command catalog lists the command`. These diagnostics join `Session::diagnostics` at start and are returned by `set_commands` and `set_layer`, covering host and layer commands together.
- **Catalog JSON:** `CommandInfo` serializes as `{ "name", "description", "policy": "alongside" | "whenIdle" | "queued", "origin": null | "…", "kind": "builtin", "parameters" }`, `{ …, "kind": "mode", "argumentHint" }`, `{ …, "kind": "registered", "argumentHint" }`, or `{ …, "kind": "skill", "argumentHint" }`. The Rust representation is `CommandInfo { name, description, policy: CommandPolicy, origin: Option<String>, kind: CommandKind }`, with `CommandKind::Builtin { parameters: Value }`, `CommandKind::Mode { argument_hint: Option<String> }`, `CommandKind::Registered { argument_hint: Option<String> }`, or `CommandKind::Skill { argument_hint: Option<String> }` (flattened into JSON as `kind`). The origin comes from `RegisteredCommand::origin`, `ModeSpec::origin`, or `Skill::origin`; built-ins, definition files, and unlabeled registrations have `None`. Modes precede registered commands, which precede skills; each hides same-name skills with a diagnostic. Hosts can construct `Command::Mode { name, prompt }` directly for a picker; `Command::decode(name, args)` validates built-in JSON arguments only (`null` counts as `{}`). Built-in `parameters` is an inlined JSON Schema object (draft 2020-12, no `$ref`, `$defs`, `$schema`, or `title`); `"format": "model"` and `"format": "entry"` suggest model and session-tree pickers, while `"format": "text"` marks free-form guidance.

## 3. Rust Extension Traits

| Trait | Crate | Purpose |
| :--- | :--- | :--- |
| `ConfigSource` | `axo::ConfigSource` | Supplies configuration: `user()` returns the trusted `UserConfig`, `project(cwd)` the untrusted project `ConfigLayer` (called only at `Read` or `Trust`, and only when the user's `project.files.config` is `true`). axo validates, merges, and filters both through `EffectiveConfig::resolve`. Passed via `AxoBuilder::config`; without one, sessions use default settings and no project layer. See [Configuration source](configuration.md#configuration-source-axoconfigsource) |
| `ExecEnv` | `axo::exec::ExecEnv` | Resolves the environment (`EnvMap`, optional `prompt_note`, optional one-time `notice`) for `bash` tool calls. Built-in implementations: `DevShellEnv` and `StaticEnv`; `StaticEnv::host()` snapshots the host environment. An `Arc<EnvMap>` converts to `Resolved` with no notice or prompt note. Passed via `SessionOptions::exec_env`. `DevShellEnv::new(base, settings, cache_dir: Option<PathBuf>, supervisor: Arc<ProcessSupervisor>)` runs its direnv, git, nix and capture subprocesses through that supervisor (a `None` `cache_dir` makes Nix mode resolve to `base`); `ExecEnv` only resolves the environment, and every command still runs supervised |
| `Scheme` | `axo::Scheme` | A URL scheme (`name://path`) the file tools accept as a path. `fn name(&self) -> &str`, `async fn resolve(&self, url: &SchemeUrl, cx: &SchemeContext) -> Result<Target, ToolError>`, `async fn read(&self, url: &SchemeUrl, cx: &SchemeContext) -> Result<String, ToolError>`, `async fn write(&self, url: &SchemeUrl, content: &str, cx: &SchemeContext) -> Result<String, ToolError>`. Virtual writes store content so a later read returns it; the returned text is the tool result. Actions belong in tools, not scheme writes. `resolve` maps a URL to `Target::File { path, writable }` (the normal file pipeline runs on it; bash rewrites whole-word, `--flag=`/`NAME=`, and redirect-target URLs to that path) or `Target::Virtual` (the scheme's own `read`/`write` handle it; defaults return `{url} cannot be read` / `{url} is read-only`). `edit`/`write` refuse non-writable files; bash refuses virtual URLs and output redirects onto read-only files. `SchemeUrl { scheme, path }` carries lowercase scheme and verbatim path; `SchemeContext { session_id, session_dir, cwd }` carries per-session facts. `prompt_doc` (default empty) is the one-line bullet in the model's `schemes` prompt section. Host names must match `^[a-z][a-z0-9+.-]+$`; `artifact`, `file`, and `tool` are reserved (`Error::Scheme`); `axo::valid_scheme_name` applies both rules. `write_replay` defaults to `Replay::Never`. Built-ins: `artifact://`, `skill://`, `local://`, `axo://` (with `AxoBuilder::cache_dir`), and in open sessions the read-only `proc://`, `agent://`, and `tool://`. Use `kill` and `send` for job control and agent messaging, and `call` for deferred tools. Passed via `SessionOptions::host.schemes` and replaced live by `Session::set_schemes`; a non-reserved matching host scheme replaces the built-in. Hosts resolve displayed URLs through `Session::schemes()` or `Axo::schemes(path, project_access, host_schemes)`. `Axo::schemes` resolves `skill://` only for file-discovered skills; code-supplied skills need the open session. |
| `SessionStore` / `SessionWriter` | `axo::{SessionStore, SessionWriter}` | Pluggable persistence backend for creating, locking, reading, listing, and appending session entries. Besides reading the header or full journal, `read_page(path, after, limit)` returns the header, up to `limit` entries with `seq > after`, and whether more follow. It backs `host.readSession`. The default reads the whole journal and slices; override it to bound the work, as `JsonlStore` does by streaming. `SessionWriter::path` is known before the file exists; `append` must complete durable storage for `Entry::requires_sync` entries before returning, and `list(cwd)` must include child sessions with their `SessionInfo::spawn`, most recently modified first (`Axo::open_session` follows these to name a child's root). Bundled implementation: `JsonlStore`. Passed as the required `store` argument of `Axo::builder` |
| `Tool` | `axo_agent::Tool` | Defines a tool (`name`, `description`, JSON Schema `parameters`, `execution_mode`, `replay`, `prompt_guidelines`, optional dispatcher `target`, and `execute(cx: ToolContext)`). `execution_mode` defaults to `Parallel`, `replay` to `Never`, `prompt_guidelines` to empty. Tools hold their own session handles; `cx` carries per-call state. Supply `HostTool` registrations through `SessionOptions::host.tools`; each wraps a `ToolFactory` and an `Exposure`. axo calls each factory with the session's `Arc<SessionTools>` again for each fork or child. A direct tool named like a built-in replaces it in place |
| `ToolPolicy` | `axo::ToolPolicy` | Ordered host policy via `SessionOptions::host.tool_policies: Vec<Arc<dyn ToolPolicy>>` (empty by default; `Session::set_tool_policies` replaces the list for invocations that start afterwards; forks and children start with the session's current list). Both callbacks receive `&ToolScope`, the call context, and a cancellation token. `ToolScope { session_id, depth, agent, mode, bash, writes }` identifies the session (root depth is zero), root persona or child spec, effective mode as `Option<Arc<ModeSpec>>`, not a selection awaiting the next request boundary, the effective `BashAccess` (`ReadOnly` when the agent, the effective mode, or the child ceiling declares it), and the effective write scope as `Option<WriteScope>` (`None` when unrestricted). Internal budget and mode checks precede host policies. `before_tool_call` receives `BeforeToolCallContext` and returns `ToolCallDecision`; the first `Block { reason, terminate, origin }` or error blocks the call and skips later policies. `ToolPolicy::origin(&self) -> Option<&str>` defaults to `None` and supplies the label when a policy returns a block without an origin. `after_tool_call` receives `AfterToolCallContext` and returns optional `AfterToolCallResult` overrides; each policy sees prior overrides, and errors are logged and skipped |
| `SpawnHook` | `axo::SpawnHook` | Child-creation policy via `SessionOptions::host.spawn_hooks`, replaced live by `Session::set_spawn_hooks`; returns `SpawnDecision` from the resolved `SpawnContext`. See [Spawning child sessions](#spawning-child-sessions) |
| `LoopHooks` | `axo_agent::LoopHooks` | The loop driver's contract, implemented internally by each `Session` (a private hooks object), not supplied through session options. `prepare_request(&AgentContext, cancel)` returns `Result<Option<Vec<AgentMessage>>, HookError>` to optionally replace messages (`AgentContext { messages, tools, deferred }` exposes both declared and deferred tool lists); `convert_to_llm` lowers `Custom` messages (default drops them); `should_skip_tools` intercepts unusable responses; `finish_turn` returns `TurnFinish { continue_turn: bool, messages: Option<Vec<AgentMessage>> }`; `prepare_next_turn` returns `Vec<AgentMessage>` appended with normal events and `MessageOrigin::Run`; `steering` supplies queued host input (see [Queued inputs and message origins](#queued-inputs-and-message-origins)). For model selection and tool setters, see [Implementing `axo_agent::Tool`](#implementing-axo_agenttool) |
| `Persist` | `axo_agent::Persist` | The agent's only event sink: ordered, awaited `async fn persist(&self, event: &AgentEvent) -> Result<(), PersistError>`; an error stops the run. `Session` uses it to persist `Entry::Message` on `MessageEnd { message, origin }` before the next request, maintain live run state, and relay session events. The agent has no broadcast or snapshot API; clients attach to `Session` |
| `Streamer` | `axo_ai::Streamer` | Host streaming extension, injected via `AxoBuilder::streamer` for tests and custom transports. `Models` implements it with provider dispatch; `ScriptedApi` implements it behind the `testing` feature |
| `Api` | `axo_ai::Api` | Wire adapter contract that `Models` dispatches to: `AnthropicApi`, `CompletionsApi`, or `ResponsesApi` |
| `CredentialStore` | `axo_ai::CredentialStore` | Loads and saves `Credential`s under [opaque origin-based slots](configuration.md#models-axo_aimodelsconfig--credentials); `Models::credential_slot(provider)` returns the current slot (`FileCredentialStore` over a host-named file is bundled). Supports `get`, `set`, `remove`, and `update` (runs a transform under the store's cross-process file lock). Injected via `ModelsOptions::credentials` into the `Models` the host passes to `Axo::builder` |
| `ProviderLogin` | `axo_ai::ProviderLogin` | Host login and refresh callbacks; see [Runtime providers, login, and reload](configuration.md#runtime-providers-login-and-reload) |
| `LoginInteraction` | `axo_ai::LoginInteraction` | Host interface for login UI; see [Runtime providers, login, and reload](configuration.md#runtime-providers-login-and-reload) |
| `RequestRewrite` | `axo_ai::RequestRewrite` | Host request rewrite hook; see [Runtime providers, login, and reload](configuration.md#runtime-providers-login-and-reload) |
| `ProviderTransport` | `axo_ai::ProviderTransport` | A provider's own HTTP exchange: sends one attempt of the request axo built and returns a `ProviderResponse` axo decodes, so a provider can reach an endpoint or protocol variant without forking an adapter. Unlike `AxoBuilder::streamer`, which replaces the whole model call, axo keeps encoding, retries, decoding and usage. Set on `Provider::transport`; see [Runtime providers, login, and reload](configuration.md#runtime-providers-login-and-reload) |
| `ProviderModels` | `axo_ai::ProviderModels` | A provider's runtime model list (for example the models an account may use), listed and cached by axo. Set on `Provider::list_models`; see [Runtime providers, login, and reload](configuration.md#runtime-providers-login-and-reload) |

Every `ToolCallDecision::Block` produces an error result with `details = {"blocked":{"origin":null}}` or a string origin in place of `null`. Read the marker with `axo_agent::Blocked::of(details.as_ref())`; a tool's own error is not a block marker. Mode-gate, turn-budget, and unlabeled hook blocks carry `origin: None`. Blocked calls skip execution and after-call hooks.

For deterministic streaming tests, `axo_ai::testing::ScriptedApi` records requests and serves scripted turns. `ScriptedApi::lane(&self, model_id: &str, turns: Vec<ScriptedTurn>)` appends a dedicated FIFO for that exact model id (not provider/model). Requests without a lane use the shared FIFO; an exhausted lane returns “script exhausted” rather than falling back to that queue. Separate child model ids keep concurrently running parent/child scripts independent.

### Implementing `axo_agent::Tool`

When driving `axo_agent::Agent` directly (or adding tools to a custom loop): build an `AgentConfig::new(streamer, model)` (defaults to `NoHooks`, no persist, one-at-a-time queues, thinking off, standard service tier), construct with `Agent::new(config)`, and interact via `prompt`, `steer`, `follow_up`, `continue_run`, `cancel(CancelReason)`, and `shutdown`. `Agent::set_tools` supplies directly declared tools; `Agent::set_deferred` supplies executable tools not initially declared to the model. Both apply from the next turn. For selection on the next request, use `Agent::set_selection(model, thinking, tier)` to update all three atomically; pass `None` for `model` to keep the current model. `Agent::set_tool_hooks` replaces the hooks for passes that start afterwards. Implement `axo_agent::Tool`:

```rust
use std::borrow::Cow;

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

    fn prompt_guidelines(&self) -> &[Cow<'static, str>] {
        const GUIDELINES: &[Cow<'static, str>] =
            &[Cow::Borrowed("Use ping to check target reachability before opening a connection.")];
        GUIDELINES
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
- **Task lifecycle:** the loop never polls a tool future itself, so a tool may await anything, including state the host's `Persist` holds. Calls start only after the batch's preparation and `ToolHooks::before_tool_call` decisions complete. When the run is stopped or shut down, every unfinished call's `cancel` fires and the loop keeps waiting; 3 s later the loop signals a hard stop. An unfinished call drops `execute` and awaits any recovery registered with `ToolContext::on_hard_stop` (`impl Future<Output = Option<ToolOutput>> + Send + 'static`). Register it once on the native call before admitting a mutation. Recovery has no further timeout: `Some(output)` becomes the result, and no recovery or `None` becomes an `Operation aborted` error result. Direct calls and detached tasks cannot register recovery. A panicking call becomes an error result. Cleanup that must happen however the call ends belongs in a `Drop` guard. `ToolHooks::after_tool_call` runs on the loop after `execute` returns; hard-stopped calls skip it.
- **`ToolUpdates`:** `send(update)` never blocks; updates sent before `execute` returns are emitted before that call's `ToolExecutionEnd`, whose `result.is_error` reports failure. Clonable, so spawned helpers can stream too. `ToolUpdates::new(f)` streams to a closure and `ToolUpdates::sink()` discards updates, for calling `execute` outside an `Agent`.
- **`SteerSignal`:** `is_pending()` is true while steering input is queued and not yet drained into the transcript; `pending().await` resolves when it becomes so (and never if the agent is gone). The built-in `bash` uses it to move a foreground command to the background, `wait` to return early, and `ask` and reviewed mode `yield` to stop waiting for the user. `Agent::remove_queued` clears it when it removes the last queued steer. Results the host injects through `LoopHooks::steering` never set it. `SteerSignal::never()` gives a signal that never fires, for calling `execute` outside an `Agent`.
- **`ToolInvocationId(u64)`:** unique per execution within one `Agent`, assigned right before `execute`. It is the identity reported back as `MessageOrigin::Tool` for that call's persisted result.
- **`ToolOutput`:** `content` (model-facing), `details: Option<Value>` (host/log data, never sent to the model), `is_error` (a failure with output, e.g. a failing command; an `Err(ToolError)` also becomes an error result), and `terminate` (stop after this batch, honored only when every result in the batch sets it).
- **`ExecutionMode::Parallel` vs `ExecutionMode::Sequential`:** A call to an `ExecutionMode::Sequential` tool is a barrier in its batch: every earlier call finishes before it starts, and later calls start only after it ends. Consecutive `ExecutionMode::Parallel` calls between barriers run concurrently as one group. Within a group, preparation and `ToolHooks::before_tool_call` run sequentially in call order before any call starts, so a later group's hooks see earlier groups' effects; `ToolResult` messages are always emitted in call order. Built-in `write`, `edit`, and `wait` are sequential. Execution mode is controlled only by tools, not by agent configuration.
- **`Replay::Safe` vs `Replay::Never`:** Controls whether an unfinished tool call is re-executed (`Safe`) or marked interrupted (`Never`) when `Session` recovers a crashed run. `replay(&self, call: &ToolCall)` decides per call; `write`, for example, is `Safe` for plain paths and defers to the scheme's `write_replay` for scheme URLs.
- **Dispatch and identity:** `Tool::target` optionally returns a target name and arguments. `axo_agent::route` resolves direct and deferred tools into `Routed { tool, call, via }`, preserving the original call id. The loop validates both dispatcher and target arguments, then uses the target for execution mode, policy contexts, execution events, `ToolContext`, and result names. Session crash recovery uses the same route for `Replay` and interrupted-result identity, including command-output lookup and admission of children started by an interrupted `spawn`; an unresolvable call retains its original identity. The dispatcher does not execute a second nested call or create another invocation. For Completions models with `requiresToolResultName`, the wire result name instead matches the assistant's original call name by id (for example `call`); an orphan result falls back to its stored tool name.

### Session tools and supervised subprocesses

Factories receive `SessionTools` with effective bash and write restrictions already installed, including the persona or child spec, restored mode, and inherited child ceilings. This also holds on reopen, fork, and child revival. Read `bash_access()` and `write_scope()` at use time rather than caching them: later scope changes update the same tools handle.

`SessionOptions::host.tools` and `Session::set_tools` accept `Vec<HostTool>`. Each `HostTool { factory, exposure, origin }` wraps a `ToolFactory` (`Arc<dyn Fn(&Arc<SessionTools>) -> Arc<dyn Tool> + Send + Sync>`) with `Exposure::Direct` or `Exposure::Deferred`; `HostTool::direct(factory)` and `HostTool::deferred(factory)` are convenience constructors, and `HostTool::with_origin(label)` labels the registration for diagnostics and `SessionToolInfo::origin`. Direct host tools apply in order, a later one replacing an earlier one or a built-in of the same name; among deferred host tools of one name the later wins too. A replacement involving a labeled registration yields a diagnostic with the winner's origin: `tool "<name>" from <winner> replaces the built-in`, `tool "<name>" from <winner> replaces the one from <loser>`, and always for deferred duplicates `deferred tool "<name>" from <winner> replaces the one from <loser>` (an unlabeled origin reads `the host`). The factory receives the session's `SessionTools`, whose `session_id()` names the session it builds for:

| Field or method | Type | Meaning |
| :--- | :--- | :--- |
| `cwd` | `PathBuf` | Relative paths and commands resolve against it |
| `exec` | `Arc<dyn ExecEnv>` | The command environment (host or dev shell) |
| `ledger` | `FileLedger` | Content hashes of files this session has read or written (the stale-file guard) |
| `snapshots` | `SnapshotStore` | Before/after bytes of this session's file mutations (`SnapshotStore::read`, `SnapshotStore::squash`) |
| `context` | `NestedContext` | Nested project instructions attached once to file tool results |
| `schemes` | `Schemes` | The session's scheme registry; its host schemes follow `Session::set_schemes` |
| `supervisor` | `Arc<ProcessSupervisor>` | Runs local commands in supervised containers, process groups or cgroup jobs per the host's `Supervision` (shared by every session of one `Axo`) |
| `run_command` | `async fn run_command(&self, cx: ToolContext, request: CommandRequest) -> Result<ToolOutput, ToolError>` | Runs a supervised command with output capture, background promotion, retained helpers, and exactly-once result delivery; background jobs are listed by `Session::jobs` and `proc://` |
| `run_bash` | `async fn run_bash(&self, cx: ToolContext, request: BashRequest) -> Result<ToolOutput, ToolError>` | Runs exactly like built-in `bash`, including URL rewriting, execution environment, read-only sandbox, and prefixing the one-time environment notice |
| `locate` | `async fn locate(&self, path: &str) -> Result<Located, ToolError>` | Cleans and resolves a scheme URL or plain filesystem path into `Located::File { url, path, writable }` or `Located::Virtual { url }` |
| `resolve_path` | `fn resolve_path(&self, path: &str) -> PathBuf` | Normalizes a tool path argument against `cwd` (drops leading `@`, expands `~`/`~/`, converts `file://`, and folds `.`/`..`) |
| `rewrite_urls` | `async fn rewrite_urls<'a>(&self, command: &'a str) -> Result<Cow<'a, str>, ToolError>` | Rewrites whole-word, `--flag=`/`NAME=`, and redirect-target scheme URLs in a shell command to file paths, matching `bash` |
| `bash_access` | `fn bash_access(&self) -> BashAccess` | The session's current effective bash access, updated whenever its scope changes (persona, effective mode). Host tools that run commands can honor it by setting `ExecSpec::sandbox` the way `bash` does |
| `write_scope` | `fn write_scope(&self) -> Option<WriteScope>` | The session's current effective [write scope](prompts-and-context.md#write-scope), updated whenever its scope changes; `None`: unrestricted |
| `check_write` | `async fn check_write(&self, target: &Located) -> Result<(), ToolError>` | Refuses a located target not permitted by every write-scope layer; `write_file` checks this before accessing the target's contents |
| `read_file` | `async fn read_file(&self, cancel: &CancellationToken, path: &str, limit: Option<u64>) -> Result<FileRead, ToolError>` | Returns bytes and a read ledger record, or virtual text. File reads take the per-file lock and are cancellable. Non-regular files are refused; exceeding `limit` bytes fails for both files and virtual text |
| `write_file` | `async fn write_file(&self, cancel: &CancellationToken, path: &str, write: FileWrite<'_>) -> Result<(Written, String), ToolError>` | Replaces bytes or applies an edit under the file lock; returns the file record or virtual scheme's response, and the path shown to the caller |

| Type | Shape |
| :--- | :--- |
| `FileRead` | `File { path, label, bytes, record }` or `Virtual { url, text }` |
| `FileWrite<'_>` | `Replace(Vec<u8>)` creates or replaces; `Edit(Box<dyn FnOnce(&[u8], &str) -> Result<Vec<u8>, ToolError> + Send + '_>)` rewrites an existing file using its current bytes and shown path. Edits of virtual targets are refused: `<url> is not a file; use read or write` |
| `Written` | `File(FileRecord)` or `Virtual(String)`; serializes as `{ type, value }` |

`axo::tools::CommandRequest` has `spec: ExecSpec`, `label: String` (the command shown in job listings), `timeout: Option<Duration>` (no default deadline), and `run_async: bool` (promote immediately). The runner applies the non-interactive environment overlay and, when `bash.ci` is enabled, `CI=true` after the environment supplied in `spec`. Output is spooled as `<n>.<tool>.<escaped call id>.log`, using `ToolContext::call` for the tool name and call id.

`axo::BashRequest` has `command: String`, `timeout: Option<f64>` in seconds, and `background: bool`. Its JSON form uses camelCase and rejects unknown fields.

Host tools should use `SessionTools::read_file` and `SessionTools::write_file` for file access. The write helper resolves the target, checks the write scope and freshness, and records the result while holding the mutation lock. Collect the returned file records in access order and use `axo::file_details(&records)` for `ToolOutput::details`, so `FileLedger::restore` can rebuild the ledger on reopen and navigation and hosts can review the [file changes](#file-changes-for-hosts). Virtual targets do not produce file records.

Host tools that run commands should use `SessionTools::run_command` to share `bash`'s job lifecycle. All subprocesses must go through the session's `ProcessSupervisor`, directly or via this runner, never `std::process::Command`/`tokio::process::Command` directly, so descendants stay contained to the extent the selected supervision mode tracks them:

```rust
use std::sync::Arc;
use std::time::Duration;
use axo::exec::host::ExecSpec;
use axo::tools::CommandRequest;
use axo::{HostTool, Registrations, SessionOptions, SessionTools, ToolFactory};
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
            sandbox: None, // or Some(Sandbox { writable }) to restrict writes
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
let options = SessionOptions {
    host: Registrations {
        tools: vec![HostTool::deferred(head)],
        ..Default::default()
    },
    ..SessionOptions::default()
};
// Use HostTool::direct(head) instead to declare git_head from the first request.
```

Direct tools enter the model's declared tool list immediately; deferred tools stay out of the initial declarations and `tools` section, are announced via reserved `tool_catalog` custom messages (which hosts cannot post), documented at `tool://` (promoting per [`Model::tool_support()`](configuration.md#tool-list-capabilities)), and invoked through reserved `call`. Persona allowlists and host `ToolPolicy` checks apply to both surfaces; see [Deferred tools](prompts-and-context.md#deferred-tools) for collision, reservation, catalog reconciliation, and promotion rules.

The supervision API (`axo::exec::host`, with `OutputError`, `ProcessSupervisor`, `Sandbox` and `Supervision` also re-exported at the crate root):

| Item | Signature | Behavior |
| :--- | :--- | :--- |
| `Supervision` | `enum { ProcessGroup, Cgroup { program: PathBuf, args: Vec<OsString>, cgroup_root: Option<PathBuf> } }` | Host-selected command supervision mode; see [Command supervision](configuration.md#command-supervision-axosupervision) |
| `ExecSpec` | `{ program: PathBuf, args: Vec<OsString>, cwd: PathBuf, env: EnvMap, sandbox: Option<Sandbox> }` | One command. `env` is the complete environment (`env_clear` semantics); a bare `program` resolves on its `PATH`. `sandbox: None` runs unrestricted; `Some` restricts the command's filesystem writes |
| `Sandbox` | `{ writable: Vec<PathBuf> }` | Landlock write boundary for one command and every process it starts: they may read and execute anywhere but write only beneath `writable` (paths that do not exist are skipped; a file path grants rights on that file only). It applies on a short-lived thread per spawn, so neither the host process nor its runtime threads are restricted, and neither are the supervisor's own guard and guardian processes; under cgroup supervision the job's cgroup directory is added so the root can join it. It requires Landlock ABI 3 (Linux 6.2+) fully enforced and fails closed otherwise. It restricts filesystem access only, not network, signals, or IPC. The built-in `bash` sets it for [read-only bash](prompts-and-context.md#read-only-bash); dev-shell capture and other harness commands use `None` |
| `ProcessSupervisor::new` | `fn new(supervision: Supervision) -> Arc<Self>` | Checks nothing yet, so hosts that never run commands need no delegated cgroup even in `Cgroup` mode. `AxoBuilder::build` creates one per `Axo` with the builder's `Supervision` (default `ProcessGroup`) |
| `ProcessSupervisor::spawn` | `async fn spawn(&self, spec: ExecSpec) -> Result<Process, String>` | Starts `spec` in a fresh container (process group or cgroup job) with stdin closed and stdout/stderr piped. In process-group mode an unstartable program returns `Err`; in cgroup mode `spawn` succeeds and the root exits 127 (program not found) or 126 (other `exec` failure) with `axo supervisor: cannot run …` on stderr (fails closed when supervision is unavailable; see [Command supervision](configuration.md#command-supervision-axosupervision)). In both modes, a `sandbox` that cannot be enforced returns `Err("read-only bash needs Linux Landlock ABI 3 (Linux 6.2+): …")` before anything starts. Cancelling leaves nothing behind |
| `ProcessSupervisor::output` | `async fn output(&self, spec: ExecSpec, deadline: Option<tokio::time::Instant>) -> Result<std::process::Output, OutputError>` | Like `Command::output`: reads stdout and stderr while waiting for the root, then kills anything left in the container and closes it. Reaching `deadline` kills the job and returns `OutputError::TimedOut`; a spawn or wait failure returns `OutputError::Failed(message)`. `OutputError` implements `Display` (`timed out`, or the message) and `std::error::Error` |
| `ProcessSupervisor::output_stderr_tail` | `async fn output_stderr_tail(&self, spec: ExecSpec, deadline: Option<tokio::time::Instant>, limit: usize) -> Result<std::process::Output, OutputError>` | Like `output`, but discards stdout and keeps only the last `limit` bytes of stderr |
| `Process` | `pub stdout: Option<ChildStdout>`, `pub stderr: Option<ChildStderr>` | A running job. `capture_output()` returns an `mpsc::Receiver<io::Result<PipeChunk>>` plus the reader `JoinSet<()>`. Each `PipeChunk` has raw `bytes: Vec<u8>` and a `pipe: Pipe` (`Pipe::Stdout` or `Pipe::Stderr`); pipe read errors are surfaced, and cross-pipe order is capture order only. `wait()` waits for the root (cancel-safe; in cgroup mode a guardian that dies first takes the job with it); `guardian_lost().await` resolves once the cgroup guardian has died, after killing the job (cancel-safe, at most once, never in process-group mode); `container()` returns a cloneable `Container`; `close().await` waits until the container is empty, removes the cgroup directory in cgroup mode, and stands the watcher down (call `container().kill()` first to end leftovers). Dropping an unclosed `Process` kills the whole container |
| `Container` | `kill(&self)`, `populated(&self) -> bool`, `async terminate(&self)`, `memory_batch(containers: &[Container]) -> Vec<Option<u64>>` | Handle to one job's container: `kill` sends SIGKILL to the process group or writes `cgroup.kill` (errors ignored); `populated` reports whether any process remains (a cgroup that is unreadable counts as empty; a group member that refuses a signal still counts as present); `terminate` sends SIGTERM (to the group, or to each cgroup member through a pidfd), waits up to `KILL_GRACE` (1 s) for the container to empty, then kills it. `memory_batch` is a blocking read of each container’s proportional set size (bytes), in input order (`None` if unavailable) |
| `KILL_GRACE` | `const KILL_GRACE: Duration` | Fixed 1 s grace period between SIGTERM and SIGKILL used by `Container::terminate` |
| `find_on_path` | `fn find_on_path(name: &str, env: &EnvMap, cwd: &Path) -> Option<PathBuf>` | Path search helper resolving a command against `env["PATH"]` and `cwd` |

Commands run through `SessionTools::run_command` have the same background behavior as `bash`: immediate promotion with `run_async`, timed promotion controlled by `bash.promoteAfterMs`, promotion when a steer is pending, or host promotion via `Session::background(call_id)`; retained helpers remain supervised. Their jobs appear in `Session::jobs` and `proc://`, and results are delivered exactly once through `wait` or a later message. Hosts needing a strictly call-scoped subprocess can use `ProcessSupervisor::output` directly; those subprocesses are not registered as session jobs or promoted.

### Queued inputs and message origins

`axo_agent` knows nothing about jobs; it offers two generic primitives that `Session` builds job delivery on.

- **`QueuedMessage { message: AgentMessage, id: Option<String> }`:** an input waiting to enter the transcript, with the host's id for it (`From<AgentMessage>` gives `id: None`). `Agent::prompt(Vec<QueuedMessage>)`, `Agent::steer(QueuedMessage)`, and `Agent::follow_up(QueuedMessage)` take it; `agent.prompt(vec![message.into()])` keeps the id-less form short. `Agent::steer` also sets the steer signal until the steering queue is drained.
- **Queue rollback:** `Agent::remove_queued(id)` removes a queued steer or follow-up by host id and returns a `RemovedQueued` token. Inspect the input with `message()` or pass the token to `Agent::restore_queued` to restore it to the same queue before its surviving suffix. Serialize producers and other removals on that queue between removal and restoration; consumers may still drain the front.
- **`LoopHooks::steering`:** `async fn steering(&self) -> Vec<QueuedMessage>` (default empty) supplies host messages drained after the agent's own steering queue, after the current tool batch and before the next model request. It never sets the steer signal. `Session` uses it for pending command/child row messages and teammate mail. Follow-up input comes from the agent's follow-up queue, drained when the run would otherwise end. `Agent::queued()` returns the queued messages as `(Vec<AgentMessage>, Vec<AgentMessage>)` in steering, follow-up order, and `Agent::queued_ids()` returns the host ids of queued steering, then follow-up, messages that carry one; teammate mail is held separately by the session's registry.
- **`AgentEvent::MessageEnd { message, origin: MessageOrigin }`:** every finished message carries its provenance:

| `MessageOrigin` | Assigned to |
| :--- | :--- |
| `Input(Option<String>)` | A prompted or queued input (including hook-supplied ones), with the id it was queued under |
| `Tool(ToolInvocationId)` | The result of a tool call that started executing with that invocation: what it returned (`Ok` or `Err`), even if cancellation arrived meanwhile, or the aborted result when Stop ended its task |
| `Run` | Everything else: assistant replies, tool-change declarations, messages returned by `prepare_next_turn`, and synthetic tool results (blocked, failed-preparation, unanswered, or aborted before they started) |

An executed result keeps its `Tool` origin exactly when it is the result persisted. `Session`'s `Persist` maps `Input(Some(id))` to the submission id (or, for a job delivery message, to the jobs it delivers) and `Tool(invocation)` to the jobs that call delivered or started, so receipts never depend on comparing message text (see [Background Job Durability](sessions-and-compaction.md#background-job-durability)).

## 4. Bun extensions (`axo-extensions`)

Trusted TypeScript/JavaScript packages can contribute providers, tools, policies, spawn hooks, URL schemes, prompt sections, definitions, and commands through supervised Bun runners. Declarations come from `extension.json`; the default export binds handlers. The bridge uses the same Rust session and provider APIs a host can call. Inference HTTP and SSE belong to the Rust adapters, even when an extension rewrites the request, unless a provider declares its own `transport`: then each attempt's HTTP exchange runs in the extension while Rust still builds the request, retries and decodes the SSE. axo installs a package's npm dependencies with the host's Bun and manages a store of installed packages, but never downloads a package itself (see [Packaging and install](#packaging-and-install)).

### Host setup and lifecycle

`Extensions::new(axo, options)` takes the host's `Arc<Axo>` and `ExtensionOptions`. Runners use that host's model registry and process supervisor. Construction rejects relative paths with `BridgeError::InvalidOptions` and does no I/O. This example uses the same `ConfigSource` pattern as the [Rust host example](#building-axo-and-running-a-session).

```rust,no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
# use axo::{ConfigLayer, ConfigSource, UserConfig};
# struct HostConfig;
# impl ConfigSource for HostConfig {
#     fn user(&self) -> Result<UserConfig, axo::Error> {
#         Ok(UserConfig::default())
#     }
#     fn project(&self, _: &std::path::Path) -> Result<ConfigLayer, axo::Error> {
#         Ok(ConfigLayer::default())
#     }
# }
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use axo::{Axo, EffectiveConfig, EnvMap, JsonlStore, ProjectAccess, SessionOptions};
use axo_ai::{FileCredentialStore, Models, ModelsConfig, ModelsOptions};
use axo_extensions::{ExtensionEvent, ExtensionOptions, Extensions};
use tokio::sync::broadcast::error::RecvError;

let state = PathBuf::from("/var/lib/my-host");
let models = Arc::new(
    Models::load(ModelsOptions {
        http_client: None,
        cache_dir: Some(state.join("cache")),
        extended_context: false,
        config: ModelsConfig::default(),
        credentials: Arc::new(FileCredentialStore::new(state.join("credentials.json"))),
    })
    .await?,
);
let axo = Arc::new(Axo::builder(models, Arc::new(JsonlStore::new(state.join("sessions"))))
    .config(Arc::new(HostConfig))
    .build());

let extensions = Extensions::new(
    Arc::clone(&axo),
    ExtensionOptions {
        // Absolute paths only; `None` leaves every extension `Unavailable`.
        bun: Some(PathBuf::from("/opt/bun/bin/bun")),
        roots: vec![state.join("extensions")],
        // Installed (`publisher/name`) extensions live here; `None` disables installs.
        store: Some(state.join("extension-store")),
        // Authored packages run `bun install` on their own when their dependencies are stale.
        auto_install: true,
        data_root: state.join("extension-data"),
        // Keep it short: `<runtime_dir>/<private dir>/rpc` must fit a Unix socket path.
        runtime_dir: PathBuf::from("/run/my-host"),
        // The runner's whole environment; nothing is inherited.
        env: Arc::new(EnvMap::from([
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
            (OsString::from("HOME"), state.join("extension-home").into_os_string()),
        ])),
    },
)?;

// Subscribe first so the initial reconciliation's events are seen.
let mut events = extensions.subscribe();
// `extensions` is user-only, so resolving at `Ignore` reads no project.
let settings = EffectiveConfig::resolve(&HostConfig, &state, Some(ProjectAccess::Ignore))?.settings;
extensions.configure(&settings.extensions).await?;
// Use the composed options on create or reopen, then attach the resulting session.
let options = extensions.compose(SessionOptions {
    cwd: state.clone(),
    ..SessionOptions::default()
});
let session = Arc::new(axo.create_session(options).await?);
extensions.attach(&session).await;
// The host's own registrations stay the host's to change while attached.
session.set_sections(vec![("host_context".into(), Some("Running inside my-host.".into()))]).await?;

let snapshot = (extensions.status(), extensions.collisions(), extensions.diagnostics());
tokio::spawn(async move {
    loop {
        match events.recv().await {
            Ok(ExtensionEvent::StateChanged(status)) => { /* id, group, generation, phase, problem */ }
            Ok(ExtensionEvent::Shadowed(collision)) => {}
            Ok(ExtensionEvent::Diagnostic(diagnostic)) => {}
            Ok(ExtensionEvent::SessionDiagnostic { session, message }) => {}
            // Missed events: reread the snapshots, including session_diagnostics(id).
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
});

// A fork keeps the host registrations and the extension layer apart: attach it too.
// let fork = Arc::new(session.fork(&entry_id).await?.session); extensions.attach(&fork).await;
// Reopen uses open_session with freshly composed options; attach it again afterward.

extensions.shutdown().await;
# Ok(())
# }
```
Both `Extensions` and `ExtensionScope` dereference to the same `ExtensionSet` API. Construct and manage scopes, Bun and installed packages through the root `Extensions` handle.


| Item | Signature | Behavior |
| :--- | :--- | :--- |
| `ExtensionOptions` | `bun: Option<PathBuf>`, `roots: Vec<PathBuf>`, `data_root: PathBuf`, `runtime_dir: PathBuf`, `env: Arc<EnvMap>`, `store: Option<PathBuf>`, `auto_install: bool` | Host choices, all absolute paths. Package roots are searched in order for authored extensions; `store` holds installed ones (`None` leaves every `publisher/name` extension `Unavailable` and fails every install call); `ExtensionOptions::data_dir(id)` names each extension's data directory: `<data_root>/<id>` for an authored extension, `<data_root>/@installed/<publisher>/<name>` for an installed one (`@` cannot start an ID, so neither nests inside the other); each runner generation gets a private directory under `runtime_dir`; `env` is the runner's complete environment (plus the bridge's own variables), and also the environment of every `bun install`. `auto_install` lets axo install authored dependencies on its own. Nothing is discovered from the working directory, `PATH`, project layers, or home directories. `bun` is the initial executable; change it with `set_bun`. To change other options, build a new manager and `shutdown` the old one |
| `Extensions::new` | `fn new(axo: Arc<Axo>, options: ExtensionOptions) -> Result<Extensions, BridgeError>` | Validates absolute paths without performing I/O. An absolute but unusable Bun remains a recoverable `ProblemKind::Bun` at runtime |
| `ExtensionSet::configure` | `async fn configure(&self, enabled: &[EnabledExtension]) -> Result<(), BridgeError>` | Validates the enabled list, rescans packages, replaces the provider layer, and reapplies the session layer to attached root sessions when the declared contributions changed. Does not eagerly start runners. Package failures go to `status`; invalid host inputs return a [`BridgeError`](#bridgeerror) without changing the configuration. Call it after list or manifest changes; use `reload` for code-only changes. Serialized with `retry`, `reload` and `shutdown`, and completed even if the caller's future is dropped |
| `ExtensionSet::status` | `fn status(&self) -> Vec<ExtensionStatus>` | One `ExtensionStatus { id, group, generation, phase, problem }` per enabled extension, in enabled order, including shadowed ones. `problem: Option<ExtensionProblem>` is set for `Disabled`, `Unavailable`, and `Restarting`; branch on its `kind`, display its human-readable `message` |
| `ExtensionSet::collisions` | `fn collisions(&self) -> Vec<ProviderCollision>` | Every `ProviderCollision { provider, winner, hidden }`: `winner` is an extension ID; `hidden` is `Hidden::Extension(id)` for a shadowed extension or `Hidden::Host(ProviderOrigin)` for a host contributor: `Layer` (an older provider layer), `Registered` (a `Models::register` provider), or `Loaded` (the loaded catalog). A snapshot from the last `configure`, `retry` or `reload`; later registry changes and newer provider layers are not reflected until the next one |
| `ExtensionSet::diagnostics` | `fn diagnostics(&self) -> Vec<ExtensionDiagnostic>` | Non-fatal discovery findings and scheme or section collisions between extensions, each with the extension ID and message |
| `ExtensionSet::providers` | `fn providers(&self) -> Vec<ExtensionProvider>` | Published providers, sorted by provider ID, as `ExtensionProvider { provider, extension }` naming each winning extension; see [Winners](#winners-collisions-and-unavailability) |
| `ExtensionSet::inspect` | `async fn inspect(&self, id: &str) -> Result<ExtensionPackage, ExtensionProblem>` | Reads the package on disk now, enabled or not: authored IDs from roots, installed IDs from the store. Not the last accepted declarations retained after a failed rescan |
| `ExtensionSet::authored` | `async fn authored(&self) -> Vec<String>` | Sorted, deduplicated authored IDs under the roots, including invalid packages whose manifest path is present; skips missing or unreadable roots. Does not validate manifests or enable anything |
| `inspect_package` (axo-extensions) | `async fn inspect_package(source: &Path) -> Result<ExtensionPackage, ExtensionProblem>` | Reads an absolute package folder without running it, before installation. Success does not promise install success: lockfiles, copied file types, and dependency installation are not checked |
| `ExtensionSet::subscribe` | `fn subscribe(&self) -> broadcast::Receiver<ExtensionEvent>` | Committed state changes, provider collisions, discovery and session diagnostics. After `RecvError::Lagged`, reread `status`, `collisions`, `diagnostics`, and any affected session's `session_diagnostics` |
| `ExtensionSet::retry` | `async fn retry(&self, id: &str) -> Result<(), BridgeError>` | Rescans, then re-arms `id` if it is load-disabled or its placement is `Disabled` or `Unavailable`: a load-disabled member is reloaded alone (its group peers keep their own load failures, and the shared process is recycled), and a placement disabled by its crash budget restarts cold with a fresh budget. A failed dependency install is attempted again. `BridgeError::UnknownExtension` for an ID that is not enabled; `ShutDown` after shutdown |
| `ExtensionSet::reload` | `async fn reload(&self, id: &str) -> Result<(), BridgeError>` | Rescans, then replaces `id`'s placement with a cold one (fresh crash budget) so the next callback imports the package's current code, whatever its phase. Group peers share the runner and are recycled with it; their own load failures are kept, `id`'s is cleared, and a failed dependency install is attempted again. Same errors as `retry` |
| `ExtensionSet::install_dependencies` | `async fn install_dependencies(&self, id: &str) -> Result<(), BridgeError>` | Runs `bun install` for an enabled authored extension now, even after a failed install of the same files, then reloads it. Use this when `auto_install` is off; see [`BridgeError`](#bridgeerror) |
| `Extensions::set_bun` | `async fn set_bun(&self, bun: Option<PathBuf>) -> Result<(), BridgeError>` | Replaces Bun for the manager and every scope, rescans them, and re-arms extensions with `ProblemKind::Bun`. Live runners keep running; the next generation uses the new path. A relative path returns `InvalidOptions` without changing anything; a closed manager returns `ShutDown` |
| `Extensions::install` | `async fn install(&self, source: &Path, origin: &str, revision: Option<&str>, replace_origin: bool) -> Result<InstalledExtension, InstallError>` | Installs or updates the package folder `source` into the store; see [Installed extensions](#installed-extensions). Records the host's optional revision label without comparing it. Does not enable it |
| `Extensions::rollback` | `async fn rollback(&self, id: &str) -> Result<InstalledExtension, InstallError>` | Swaps installed `id`'s whole current and previous copies, including revision and install time (a second rollback goes forward again), and reloads it if enabled. `NotInstalled`, `NoPrevious` |
| `Extensions::uninstall` | `async fn uninstall(&self, id: &str, delete_data: bool) -> Result<(), InstallError>` | Deletes installed `id`'s package and previous version, even with an unreadable install record; refuses while it is enabled (`Enabled`). `delete_data` also deletes its data directory `<data_root>/@installed/<publisher>/<name>`; files the extension wrote anywhere else are untouched, since that directory is a convention, not a sandbox |
| `Extensions::installed` | `async fn installed(&self) -> Result<Vec<InstalledExtension>, InstallError>` | Every `InstalledExtension { id, origin, current, previous }` in the store, sorted by ID; unreadable records are skipped. `current: InstalledCopy` and `previous: Option<InstalledCopy>` expose `revision: Option<String>` and `installed_at: i64` (Unix milliseconds); the folder field is crate-private |
| `ExtensionSet::memory` | `async fn memory(&self) -> Vec<RunnerMemory>` | One `RunnerMemory { members, group, pss_bytes }` per live runner, in the enabled order of its first member: the proportional set size summed over the runner's process group or cgroup. Never split among the members of a shared runner; a runner whose memory cannot be read is omitted. Read on demand; there is no memory cap |
| `ExtensionSet::shutdown` | `async fn shutdown(&self)` | Closes admission, withdraws this set’s providers, and kills and reaps runners. Shutting down a scope keeps host and root extension registrations; shutting down the root waits for admitted store mutations, closes its live scopes and restores host registrations. An in-flight install prevented from publishing returns `InstallError::ShutDown` without changing the record; new store mutations are refused, but `installed()` can still read the store. Idempotent. Dropping the manager fences retained callbacks immediately and runs the same teardown without waiting; call `shutdown` to wait for it |

`ExtensionPackage` contains `id`, declared `sdk: Option<String>`, the `package: PathBuf`, `providers: Vec<ProviderDecl>`, `session: SessionDecl`, ordered `(name, body)` pairs in `sections`, loaded `definitions: Definitions`, `entry_error: Option<String>`, and `diagnostics: Vec<String>` (ignored duplicate manifests and definition-loader findings). An invalid entry can leave declarations valid: inspect `entry_error` even on success. The declaration types `ProviderDecl`, `SessionDecl`, `ToolDecl`, `SchemeDecl`, `CommandDecl`, and `PolicyHook` are public in axo-extensions. Inspection does not run Bun, install dependencies, or stamp extension origins on the file-loaded definitions.

| `ProblemKind` | Meaning |
| :--- | :--- |
| `Bun` | No Bun configured, or the configured path is not an executable file |
| `NotFound` | No manifest in the roots, or the package is not installed |
| `Invalid` | Rejected manifest, SDK, sections, definitions, entry, install record, or dependency metadata |
| `DependenciesMissing` | Dependencies are missing or stale and `auto_install` is off |
| `DependencyInstall` | Bun install failed or timed out; the message includes the end of stderr on failure |
| `Load` | The extension's own activation was rejected |
| `Crash` | The runner died, hung, or failed its handshake |
| `Setup` | Host inputs prevent running: store, runtime directory, socket path, process spawn, or data directory |

Await the first `configure` before composing session options or selecting extension models. Listing providers, composing options, and creating or reopening a session do not require a running Bun process. The first callback starts it, including the first subscribed journal entry after attachment.

Extensions can use `ctx.host` to list or read saved sessions, read snapshots, resolve a catalog, and run parsing and projection helpers. For session creation, opening, forking and closing, the host installs a `Lifecycle` using `Extensions::set_lifecycle(Option<Arc<dyn Lifecycle>>)` (`None` clears it). The hook composes options on create/open, keeps returned sessions alive, updates the host UI, and shuts down and releases closed sessions. The bridge attaches returned sessions before replying and detaches them after successful close. See the [generated reference](extension-api.md#served-by-the-extension-bridge) for the wire methods.

The async `Lifecycle` methods take `&self` and `caller: &axo::ext::Caller`:

| Method (remaining arguments) | Result |
| :--- | :--- |
| `create(cwd: PathBuf, request: SessionRequest)` | `Result<Arc<Session>, Error>` |
| `open(path: PathBuf, request: SessionRequest)` | `Result<Arc<Session>, Error>` |
| `fork(session: Arc<Session>, entry: EntryId)` | `Result<Forked, Error>` |
| `close(session_id: String)` | `Result<(), Error>` |

`SessionRequest` carries optional `model` and `agent` strings, optional `thinking: ThinkingLevel` and `project_access: ProjectAccess`, and `sections: Vec<(String, Option<String>)>`. `Forked` carries `session: Arc<Session>` and `editable: Option<Vec<PromptPart>>`; the wire `ForkReceipt` has the same editable parts but a session ID string. The manager owns the hook until shutdown, so a hook that needs the manager must hold `Weak<Extensions>`. Each admitted call runs to completion in its own task even if the caller cancels or its runner goes away.

Use `ctx.models` to find models, inspect credential sources without retrieving secrets, estimate tokens, or run [one-off completions](#one-off-completions). `await ctx.models.complete(request)` returns the final assistant message; a terminal error or missing final message rejects with code `completion`. The SDK types rejections as `HostCallError`; a terminal stream error carries the corresponding `CompletionDetails` in `error.details` (`stopReason` in JSON). `await ctx.models.stream(request, listener)` delivers assistant events followed by `closed` on normal completion; call `await subscription.close()` to stop early. See the [generated reference](extension-api.md) for the method list.

### Session composition

- `ExtensionSet::compose(options) -> SessionOptions` replaces `SessionOptions::layer` with the enabled extensions’ registrations, including definitions. It leaves the host’s own registrations unchanged, does no I/O and starts no runner.
- `Extensions::scope(ScopeOptions { key, roots, data_root, auto_install }) -> Result<ExtensionScope, BridgeError>` creates an isolated extension scope (for example, one trusted workspace folder) backed by the same `Extensions` manager and `Axo` engine. Each `ExtensionScope` owns its own `Models::scoped()` view (`ExtensionScope::models()`), `ProviderLayer`, credential slots, data directory, and runners, so two scopes can load different packages under the same extension ID or provider ID simultaneously without leaking tools, hooks, events, or providers to each other or to the base registry. `ExtensionSet::compose(options)` layers the parent manager's host-wide extensions followed by the scope's extensions and sets `SessionOptions::models` to `Some(scope.models())`; `ExtensionSet::attach(&session)` tracks the session in that scope (host reconfigurations propagate to all live scopes; scope reconfigurations update only that scope's sessions). Scoped extensions calling `models.*` see their scope's `Models`, and `session.*` calls (plus `host.closeSession` and `session.subscribe`) are restricted to sessions attached to that scope and to spawned children whose nearest attached spawn ancestor is attached there. The ownership check walks `Axo::spawn_parent` and never revives a parked child; only an owned child is revived to serve the call. `Lifecycle::create` and `Lifecycle::open` return `LifecycleSession { session, scope }` so newly created or opened sessions attach to the target folder's scope rather than the calling extension's scope; `Lifecycle::fork` inherits the source session's scope automatically.
- An extension set inspects and lists packages from its own roots and reports its own published provider winners. Scopes have no installed store: inspecting a `publisher/name` ID returns `ProblemKind::Setup`. Bun is shared with the root manager; only `Extensions::set_bun` changes it.
- Extension tools (`SessionToolInfo::origin`), registered commands, skills, modes, and policy-blocked calls carry one `ExtensionOrigin` label: `extension <id>` for the root manager, `extension <id> (scope <key>)` for a scope. `ExtensionOrigin::parse` recovers `id` and optional `scope` from a bridge label and returns `None` for other labels; `Display` formats it. Skills and modes loaded from ordinary definition roots have `origin: None`; the bridge stamps them when composing extension contributions. Spawn-hook blocks have no such attribution.
- `ExtensionSet::attach(&session)` tracks a live `Arc<Session>` weakly for actions and events. For a root it applies the current contributions once through `Session::set_layer`, even if unchanged, since a `configure` may have run since `compose`; later reconciliation (`configure`, `retry`, `reload`) reapplies only when the declared contributions differ, and `shutdown` always does. The bridge never touches the host lists, so the host's own setters (`set_tools`, `set_sections`, `set_tool_policies`, …) stay usable while attached, and the layer keeps following whatever the host sets. A fork starts with the parent's host lists and layer kept apart; attach it to keep its layer current. An attached spawned child gets actions and events only: it starts with its parent's host and layer entries joined as its own, keeps them for life, and has no command catalog. `ExtensionSet::detach` stops tracking by ID and leaves the registrations as they are.
- Host tools, schemes, sections, and registered commands are followed by extension entries in enabled order. Later entries win within the applicable registration class; direct tools take precedence over deferred tools of the same name. Built-in commands and modes still outrank registered commands, which outrank skills. Policies and spawn hooks run in order, host entries first, rather than replacing each other. Diagnostics produced by a live application, including `cannot apply extensions: <error>` when `set_layer` rejects the layer and the session keeps its previous one, are available through `ExtensionSet::session_diagnostics` and `ExtensionEvent::SessionDiagnostic`.
- Enabling, disabling or changing an extension’s definitions updates attached roots on reconciliation. Within each kind, the last-enabled extension comes first in the layer. Agents and teams share a namespace, so an extension agent wins over any same-name extension team regardless of enabled order (see [Live definitions](#live-definitions)). At startup, definition collisions and invalid teams appear in `Session::diagnostics`; live findings are delivered through `ExtensionEvent::SessionDiagnostic`. Running children keep their snapshots, but extension handlers resolve the current runner on each invocation.

### Packages and manifests

During reconciliation the manager reads the package's `extension.json` for each enabled ID, and only those; hosts can separately inspect disabled packages through `inspect`. An authored (plain) ID is read from `<root>/<id>/extension.json` in the first root in `roots` order where that path exists; a later copy is ignored with an `ExtensionDiagnostic`, and a first copy that exists but cannot be read still claims the slot rather than falling through to a later root. An installed (`publisher/name`) ID is read from the store's current version folder; without a `store` or an install record it is `Unavailable`. The manifest is UTF-8 JSON, unknown fields are rejected, and nothing is executed to read it:

```json
{
  "id": "lab-provider",
  "entry": "index.ts",
  "providers": [
    {
      "id": "lab",
      "models": [
        {
          "id": "m",
          "name": "Lab model",
          "api": "openai-completions",
          "provider": "lab",
          "baseUrl": "http://127.0.0.1:8080/v1",
          "reasoning": false,
          "input": ["text"],
          "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
          "contextWindow": 128000,
          "maxTokens": 8192
        }
      ],
      "env": [],
      "requiresKey": true,
      "login": true,
      "rewrite": true
    }
  ]
}
```

- **`id`** must equal the extension ID it is enabled under: the directory name for an authored package, `publisher/name` for an installed one. **`entry`** is a package-relative regular file that must resolve inside the package; the package directory itself may be a host-made symlink, but the entry may not escape it. This is discovery hygiene, not a sandbox.
- **`sdk`** names the extension SDK version the package was built against (`SDK_VERSION`, currently `0.0.0`, the version of `@axo/extension-sdk`). It must be caret-compatible: the same major above 0, else the same minor above 0, else the same version. A mismatch is not admitted, naming both versions. Optional for authored packages (absent means no check), required for installed ones.
- **Providers:** `models` use the `axo_ai::Model` JSON shape (`thinkingLevelMap`, `serviceTiers`, and `compat` optional; `standard` is not a valid `serviceTiers` key), each naming its enclosing provider, and are checked by the same validation as `Models::register`. `env` defaults to `[]`, `requiresKey` to `true`, and `login`, `rewrite`, `transport` and `listModels` to `false`. Provider IDs follow the registry rules and may not repeat within a manifest. `login: true` makes `Models::login` available and routes OAuth refreshes to the extension; `rewrite: true` routes every inference request's `ProviderRequest` through it. `transport: true` sends every attempt through its `send` handler, which returns a fetch `Response` whose body axo decodes as the model's wire (after `rewrite`, when both are declared). `listModels: true` replaces the manifest `models` with what its `listModels` handler returns, as [runtime model lists](configuration.md#runtime-providers-login-and-reload): axo lists when the provider joins without a cached list, after login, set key and logout, and on each refresh, never for an unavailable provider, so a provider declaring it starts its runner then; plain lookups still never start Bun. The manifest `models` stand in until the first successful listing. Credentials are stored and selected by `Models` exactly as for Rust providers.
- **OAuth-only providers:** `oauthOnly` defaults to `false`. Set it to `true` with `requiresKey: true`, `env: []`, and `login: true` to accept only stored OAuth credentials. Login and refresh must return an OAuth credential. This policy does not change the provider's wire adapter.
- **Session declarations** are optional, as is `providers`. See the session manifest example below.
- **Not admitted:** a missing, unreadable, or invalid manifest, incompatible `sdk`, or an invalid package path (see below) leaves a new extension `Unavailable`, with no provider or session surface.
- **Admitted but not runnable:** an unusable Bun or entry leaves the static declarations composed. Providers and remote tools, schemes, and commands fail naming the extension; its tool policy blocks every tool call and its spawn hook blocks every spawn instead of silently bypassing either. Bun is checked each time the manager reconciles.
- **Retained callbacks:** spawned children and detached or never-attached sessions can keep registrations that the manager no longer applies. Only `Failure::Removed` fails open: a retained tool policy allows calls and leaves results unchanged, and a retained spawn hook allows spawns. Retained tools, schemes, and commands still fail. While the extension remains enabled but cannot run, or after manager shutdown or drop (`Failure::ManagerGone`), policies and spawn hooks fail closed; tools, schemes, and commands fail too. See [Callbacks and failure contracts](#callbacks-and-failure-contracts) for the failure variants.
- **Failed rescans:** a previously admitted package retains its last accepted declarations in memory, unavailable. A broken refresh does not reveal what the package hid; a fresh host has no cached declarations.

```json
{
  "id": "lab-session",
  "entry": "index.ts",
  "tools": [
    {
      "name": "echo",
      "description": "Echo arguments",
      "parameters": { "type": "object" },
      "exposure": "direct",
      "replay": "never",
      "executionMode": "parallel",
      "guidelines": []
    }
  ],
  "schemes": [{ "name": "lab", "promptDoc": "Lab resources", "writeReplay": "never" }],
  "commands": [{ "name": "hello", "description": "Queue a greeting", "argumentHint": "text" }],
  "sections": [{ "name": "lab_rules", "file": "prompts/rules.md" }],
  "definitions": { "agents": "agents", "modes": "modes", "keywords": "keywords", "skills": "skills" },
  "toolPolicy": [],
  "spawnHook": false,
  "events": false
}
```

- Tool names are 1–64 ASCII letters, digits, underscores, or hyphens. A manifest can declare `call` or `yield`, but session shaping ignores `call` in every session and `yield` in roots with modes, with a diagnostic. Tool `parameters` must be a JSON object with type `"object"`. The defaults shown above also allow `deferred`, `safe`, and `sequential`, respectively. Replay is a static promise read during crash recovery; it does not retry a lost runner call automatically.
- Scheme and section names follow `valid_scheme_name` and `valid_section_name`. Scheme docs default to empty and `writeReplay` can also be `safe`. Command names must be nonempty and contain neither whitespace nor `/`. A manifest cannot repeat a tool, scheme, command, or section name.
- `toolPolicy` lists the policy hooks the extension implements, a subset of `["before", "after"]` without repeats; empty (the default) registers no policy. Only declared hooks are called: a `["before"]` policy never sees results.
- Section files are read as UTF-8; definition directories use the Rust loaders. A section file must be readable and inside the package; a definition path must also be a directory. Violations reject the package, or keep its last accepted declarations unavailable on rescan. An invalid entry instead leaves the package admitted but unrunnable. The agents directory supplies both agents and teams; the skills directory contains skill folders with a `SKILL.md` and any reference files.

### Packaging and install

Location decides the kind of extension. An **authored** extension, written by the user or a repo, lives under a host root and has a plain ID (`[a-z0-9][a-z0-9._-]*`). An **installed** extension is placed by `Extensions::install` in the managed `store` and has a `publisher/name` ID, each part following the plain-ID rule. An ID with a `/` is looked up only in the store, one without only in the roots, so the two never collide. Settings enable both kinds the same way.

#### Authored extensions

An enabled authored package whose `package.json` has a nonempty `dependencies`, `devDependencies`, `optionalDependencies`, or `peerDependencies` table installs them itself. The stamp `node_modules/.axo-install` records a SHA-256 of `package.json` and `bun.lock`; when it is missing or differs, `configure` (or `retry`/`reload`) starts `bun install` in the package folder in the background, under the host's `ProcessSupervisor` with `ExtensionOptions::env`, and returns without waiting. A package without dependencies never installs, and only enabled packages are looked at.

- **Calls wait.** A callback on a cold or starting placement waits for the install before the runner starts; status shows `Installing`. An already live runner keeps serving at `Ready`; it is reloaded when the install finishes, so a call still in flight on it fails once.
- **No lockfile required.** It is a plain `bun install`, exactly as run by hand: it creates `bun.lock` when absent (which the stamp then covers) and runs lifecycle scripts.
- **Failure.** The extension shows `Disabled` with `extension <id>: bun install failed (<exit status>): <end of stderr>` or `extension <id>: bun install timed out after 600 s`; its callbacks fail with that reason. The same files are not reinstalled on later rescans or calls; `retry`, `reload`, `install_dependencies`, or a change to `package.json`/`bun.lock` tries again.
- **Retry reset:** `retry`, `reload`, and `install_dependencies` serialize failure resets with that authored extension's install. Remembered and published dependency failures are cleared together; an old preparation cannot restore the cleared failure, and completion from a replaced package admission cannot change the current status. An explicit install keeps an already warm runner serving until the successful install triggers its reload.
- **Manual mode.** With `auto_install: false`, a stale package is `Unavailable` ("dependencies are not installed") until the host calls `ExtensionSet::install_dependencies`.

#### Installed extensions

The host acquires a package into any local folder (axo never downloads one) and passes it with an origin string and optional revision label to `Extensions::install(source, origin, revision, replace_origin)`, which covers first install and update:

1. Reads `id` and `sdk` from `<source>/extension.json`: the ID must be `publisher/name` and the SDK declared and compatible.
2. Refuses an update whose origin differs from the recorded one unless `replace_origin` is set; the record then takes the new origin. The revision label (for example a Git commit and ref) is stored but never compared.
3. Copies `source` into a fresh version folder, skipping a top-level `node_modules` and `.git`, recreating symlinks as symlinks and refusing other special files, then validates the manifest and entry as discovery would.
4. When `package.json` declares [any of the dependency kinds above](#authored-extensions), requires `bun.lock` and runs `bun install --frozen-lockfile` there. Lifecycle scripts run.
5. Only then rewrites the record with the new copy's revision and installation time (Unix milliseconds), keeps the replaced copy with its own metadata as the one previous copy (deleting the older one), and reloads the extension if it is enabled: its in-flight calls, and those of group peers sharing its runner, fail and are not replayed.

A failure before publishing leaves the record and running version untouched and cleans up the new folder. One install, rollback or uninstall runs per extension at a time, using advisory locks shared by managers on the same store. The store layout is:

```text
<store>/<publisher>/<name>/install.json   the atomic current/previous record
<store>/<publisher>/<name>/v-XXXXXX/      one folder per version
<store>/.locks/<publisher>/<name>        advisory lock
```

The record stores whole copies:

```json
{
  "origin": "https://example.com/publisher/name",
  "current": { "folder": "v-XXXXXX", "revision": "rev-b", "installedAt": 1795400000000 },
  "previous": { "folder": "v-YYYYYY", "revision": "rev-a", "installedAt": 1795300000000 }
}
```

`revision` may be `null`, and `previous` is `null` on first install. Rollback swaps both copies, preserving their revision and installation time; it does not create a new timestamp. Old records with string-valued copies are invalid, with no migration. Disable and uninstall such an extension (uninstall accepts unreadable records), then install it again.

Renaming a new `install.json` into place is the only switch, so it is atomic, and a running runner keeps reading its own version folder until it is reloaded.

If reloading fails after publication, the new version is already current. The error is `InstallError::Io` (`installed, but reloading failed`), or `ShutDown` if the manager closed.

An interrupted uninstall can leave a `.remove-<name>` or `.purge-<name>` tombstone. While cleanup is pending, install refuses with `InstallError::Io` (`uninstall cleanup is pending; retry uninstall`), and rollback reports `NotInstalled`. Retrying uninstall finishes pending cleanup, including any pending purge regardless of the new `delete_data` value.

#### `BridgeError`

| Variant | When |
| :--- | :--- |
| `InvalidConfig` | The enabled list is invalid |
| `InvalidOptions` | Construction receives a relative `bun`, root, `store`, `data_root`, or `runtime_dir` path (`invalid extension options: <name> <path> is not an absolute path`); `scope` rejects invalid or duplicate keys, and `set_bun` rejects a relative path |
| `ShutDown` | The manager is closed |
| `UnknownExtension` | The ID is not enabled |
| `InstalledDependencies` | `install_dependencies` was called for an enabled installed (`publisher/name`) extension; `install` manages its dependencies |
| `DependencyInstall` | `install_dependencies` could not locate the package or Bun install failed; carries the locate error or install failure |
| `Registry` | The model registry rejected the provider layer |

#### `InstallError`

| `InstallError` | When |
| :--- | :--- |
| `NoStore` | `ExtensionOptions::store` is `None` |
| `Bun` | Bun is missing or not executable. Every install needs a usable Bun, even without dependencies; Bun is checked before the manifest |
| `Busy` | Another install, rollback or uninstall of that extension is running |
| `InvalidPackage` | Unreadable or invalid manifest, an ID that is not `publisher/name`, no `sdk`, an invalid entry, an unsupported file type, a relative `source`, or a `source` overlapping the store destination |
| `SdkMismatch` | `sdk` is incompatible with `SDK_VERSION`; names both versions |
| `MissingLockfile` | Dependencies declared but no `bun.lock` |
| `OriginChanged` | An update from another origin without `replace_origin` |
| `InstallFailed` | `bun install --frozen-lockfile` failed; carries its exit status and the end of its stderr |
| `Enabled`, `NotInstalled`, `NoPrevious` | `uninstall` of an enabled extension; no record; `rollback` with no previous version |
| `Io`, `ShutDown` | A filesystem error; the manager is shut down |

#### Pre-bundled packages

A single file built with `bun build --target=bun --outfile=<package>/index.js <source entry>` has no external imports left and loads as an `entry` with no `package.json`, `node_modules` or install. Native `.node` and `.wasm` assets must ship beside the bundle.

### Winners, collisions and unavailability

For each provider ID the **last enabled extension declaring it wins**. The manager owns one `ProviderLayer`, newer than any created before it, so a winning extension provider hides a `Models::register` provider and the loaded catalog provider of the same ID (and older layers). Hidden providers are untouched and reappear exactly when the winner goes away: removing the extension from the list, a changed valid declaration, a reorder, or `shutdown`. A failing or crashed winner stays the winner; the hidden provider is never used as a fallback.

When a collision is newly reported, `ExtensionEvent::Shadowed` carries the same `ProviderCollision` record as the collision snapshot.

`ProviderInfo::origin` identifies the winning contributor as `ProviderOrigin::Layer`, `Registered`, or `Loaded`. For a `Layer` provider in a scoped model view, look in the scope’s `providers()` first, then the root manager’s; scope layers win over base layers. A layered provider in neither list belongs to a host-owned `ProviderLayer`.

While an extension is `Disabled` or `Unavailable`, its providers stay in `Models::find`, `Models::all`, and `Models::providers` (with their declared login capability), but `Models::login`, `Models::auth`, and requests fail with `Error::ProviderUnavailable` before credential lookup, naming the extension and group; see [Unavailable providers](configuration.md#runtime-providers-login-and-reload). Callbacks also fail during restart or shutdown; during first startup they wait for activation.

`configure` keeps a placement, its live runner, and its crash budget when its members and declarations are unchanged; a priority-only reorder changes only the activation order for later generations. A placement whose setup failed (`Unavailable`) is rebuilt cold instead, so corrected host inputs are re-checked; its members' load failures carry over. Changing a placement's members, an entry, or a declaration replaces it: the old runner's pending callbacks fail before the new providers are published, and an old provider handle that a request still holds fails at once instead of reaching the new winner. A provider that loses its winner is retired the same way even when its runner keeps serving other providers.

### Trust boundary

- **Who enables code:** only the user-only `extensions` setting (or a typed list from the host) enables an extension, and only packages under host-supplied `roots` or in the host-supplied `store` can load. Dependencies install only for enabled extensions, so a repo's extension runs `bun install` only once the user enables it. Project layers cannot enable, reorder, or group extensions at any access level (see [Extensions](configuration.md#extensions-extensions--axoenabledextension)). A host that wants project-provided extensions must authorize those roots itself; a session's discovered project files are never published as global providers.
- **What runs:** extensions are trusted code. The runner is an ordinary OS process with the host user's rights and the `ExtensionOptions::env` environment, started without a sandbox (`ExecSpec::sandbox` is `None`); it may read files and use the network like any program. The data directory (`ExtensionOptions::data_dir`) is created (mode `0700` when new) before activation and passed to the extension as `dataDir`; it is a convenience, not isolation.
- **No acquisition:** axo never downloads Bun, the SDK, or packages: the host hands `Extensions::install` a local folder. Dependency installs run the host's Bun with `ExtensionOptions::env`, so they reach whatever registry that environment configures, and run lifecycle scripts. Each runner directory carries a `bunfig.toml` with `install.auto = "disable"`, so importing a package that is not already installed fails activation instead of fetching it. An extension may still make its own network calls.
- **Secrets:** the bridge writes its socket path and a random per-generation nonce into the runner environment (`AXO_EXTENSION_SOCKET`, `AXO_EXTENSION_NONCE`); the runner deletes them from its environment before importing extension code. Credentials never appear on a command line. Runner stdout and stderr are drained and discarded. Error messages name the extension, provider, and method; they never include tokens, headers, request bodies, prompt answers, or author exception text. Provider decode errors describe the failing check without echoing author-supplied field or header names. The one exception is a failed dependency install, whose reason carries the end of Bun's stderr, including any output of the package's lifecycle scripts.

### Runners and supervision

Extensions are placed per **placement**: an ungrouped extension gets its own runner; all members of a `group` share one. One lifecycle owner per placement starts, watches, restarts, and disables its runner.

| `ExtensionPhase` | Meaning |
| :--- | :--- |
| `Cold` | Declarations published; no runner yet, or none needed since every member of the placement failed to load (those members show `Disabled`) |
| `Installing` | An authored package's dependencies are being installed; callbacks wait for the install |
| `Starting` | The first callback is starting a runner; callers arriving meanwhile share the same startup outcome |
| `Ready` | The runner answered its handshake and the member activated |
| `Restarting` | The runner failed and a new generation is starting immediately; callbacks arriving meanwhile fail |
| `Disabled` | The member's own import or activation failed, its dependency install failed, or the placement spent its crash budget. Stays so until `retry` or `reload`, or until the member is removed and re-enabled (a failed install also retries when `package.json` or `bun.lock` changes) |
| `Unavailable` | Cannot run: not admitted, no or unusable Bun, an entry that failed discovery, stale dependencies with `auto_install: false`, an invalid or unreadable `package.json` or unreadable `bun.lock`, or a host/filesystem setup failure such as an overlong socket path. No automatic retry; the next `configure`, `retry` or `reload` re-checks |

- **Start:** each generation gets a fresh private directory below `runtime_dir` (mode `0700`) holding the runner source embedded in the crate, `bunfig.toml`, and the listening socket `rpc`, bound before spawn. The supervisor runs `<bun> run <dir>/runner.ts` with that directory as working directory and no shell. Connection acceptance and the nonce-bearing `hello` share one 15 s handshake deadline, not separate timeouts; the listener is then closed and the socket path removed. The directory is removed when the generation ends. The socket path is limited to 107 bytes.
- **Activation:** members activate one at a time in enabled order, each with its own 15 s deadline. The runner imports the entry by file URL and calls its default export with `{ id, dataDir, session, host, models }`; the returned handlers must match the manifest exactly. Each generation rechecks the entry before activation. An unreadable or non-regular entry, failure to create the data directory, a non-UTF-8 entry or data path, or a clean import or activation failure disables **that member only**, without spending the placement’s crash budget. A runner that dies or hangs during activation counts as a placement failure.
- **Heartbeat:** the bridge pings every 2 s and answers come from the JS event loop, so a synchronous loop that blocks it is detected: 10 s without a pong kills the runner.
- **Crash budget:** runner exit, socket EOF, protocol violation, failed handshake, or a missed heartbeat is one failure for the whole placement, counted once per generation. Failures 1 and 2 within a rolling 300 s window restart immediately with a new generation; the 3rd disables every member not already load-disabled, with a reason naming the group and its members. Older failures age out. Shutdown, reconfiguration, a caller cancelling, a callback deadline, and an author's method error do not count.
- **Teardown:** a failed generation fails all its pending callbacks with their own extension's name, kills the runner's container, and waits for it to close before starting a replacement. No callback is replayed on the new generation.
- **Supervision mode:** runners are `ProcessSupervisor` containers of the host's `Supervision`. With `Supervision::ProcessGroup` killing the runner kills its process group, including children it spawned normally, but a descendant that leaves the group (`setsid`, double fork) survives the runner, shutdown, and the host, as with any process-group command. With `Supervision::Cgroup` every descendant, detached or not, stays in the runner's job cgroup and dies with it, and the job directory is removed. Runners are not session jobs: they are never listed under `proc://` and survive session shutdown; they belong to the `Extensions` manager.
- **Limits:** one runner per placement serves every session; there are no per-session processes, Workers, memory or CPU limits. A group shares fate: one member's crash or hang restarts, and eventually disables, the whole group.

### Callbacks and failure contracts

| Callback | Deadline | Interrupted (runner death, restart, retirement, deadline, no live runner) | Author error or invalid result |
| :--- | :--- | :--- | :--- |
| `login` | None: waits on the human. Dropping the `Models::login` future cancels it, aborting the author's signal and withdrawing outstanding questions | `Error::ProviderUnavailable`; nothing stored | `Error::Auth` naming extension, provider, and method; nothing stored |
| `refresh` | 60 s, including a cold start. It runs inside the credential store's locked update and continues when only a waiting stream is cancelled, so its rotated credential is still saved | `Error::ProviderUnavailable`; the lock is released and nothing is saved | `Error::Auth`; nothing saved |
| `rewrite` | 60 s, including a cold start | The stream ends with one `AssistantEvent::Error` carrying the `provider_unavailable`, `should_retry: false` failure; no HTTP request is sent and neither axo nor the session retries it | The stream ends with an `AssistantEvent::Error` naming the extension; nothing is sent |
| `send` | 60 s to the response head, including a cold start; none for the body | Before the head: as for `rewrite`. Mid-body: the stream ends with an error (`provider unavailable: ...`); nothing is replayed | `Error::Auth` naming the extension; an invalid head fails the attempt |
| `listModels` | 60 s, including a cold start | The listing fails and the previous list stays | The listing fails (an invalid list is rejected whole) and the previous list stays |

**Interruption versus an accepted request.** For a provider without `transport`, the boundary is the rewrite's reply. If Bun dies, hangs, or is replaced **before** the reply is accepted, the request fails as above and is never resent; the next independent request uses the restarted runner once it is `Ready`. **After** the reply is accepted, the request belongs to Rust: transport retries, the HTTP response, and the whole SSE stream continue normally even if Bun is killed mid-stream, and nothing is re-sent or aborted on its account. A provider that declares `transport` keeps no such guarantee: its HTTP exchange runs in Bun, so a crash mid-body fails that reply and is not replayed. A reply that arrives after its provider was retired is discarded. A callback whose deadline passes is failed and cancelled without restarting a runner that still answers heartbeats.

Replies are validated before use. A credential must have the `Credential` JSON shape: `key` and `access` must be nonempty, `api_key` allows no extra fields, OAuth `baseUrl` must be HTTP(S), and credential header names and values must be valid with no case-insensitive duplicate names. A provider field cannot differ from a reserved credential field only in case, `_`, or `-`. Requests reject unknown fields and require an HTTP(S) URL and valid header names and values. Both requests and credentials allow at most 24,576 headers and must fit the frame limit. Request headers travel as ordered `[name, value]` pairs, preserving duplicates; non-UTF-8 values use `{ "base64": "..." }`.

### Author SDK

The type-only author SDK lives in `packages/axo-extension-sdk` (package `@axo/extension-sdk`); an extension never needs it at runtime and may be plain JS. Its default export returns a `Handlers` record, possibly asynchronously. Each of `providers`, `tools`, `schemes`, and `commands` maps declarations to handlers. Provider login requires both login and refresh, rewrite requires the rewrite handler, transport requires `send`, list models requires `listModels`, and a declared tool policy defines exactly the hooks its `toolPolicy` lists. Missing or undeclared handlers fail activation, including the spawn hook and journal entry handler.

For a manifest that declares only an echo tool and a hello command:

```ts
import type { Extension } from "@axo/extension-sdk";

export default (() => ({
  tools: {
    echo: async ({ call, update }) => {
      update({ content: [{ type: "text", text: "Working" }] });
      return { content: [{ type: "text", text: JSON.stringify(call.arguments) }] };
    },
  },
  commands: {
    hello: (args, session) =>
      session.post({ customType: "greeting", text: args, display: true, delivery: "quiet" }),
  },
})) satisfies Extension;
```

- **Login interaction:** `ui.openUrl(url, instructions?)`, `ui.deviceCode({ userCode, verificationUrl, expiresInSeconds? })`, and `ui.progress(message)` resolve once the host's `LoginInteraction` has shown them; `ui.prompt({ message, placeholder?, secret }, signal?)` and `ui.select({ message, options: [{ id, label }] }, signal?)` resolve with the answer. A per-question signal withdraws just that question (for example when a browser callback wins); the login's own signal withdraws all of them. A refused display call fails the login (`login display not acknowledged`, surfaced as `Error::Auth`). `prompt` and `select` reject with `interaction` if the host UI fails. `select` requires a nonempty array of option objects and rejects with `invalid` if the host returns an ID not among them.
- **Credentials:** `{ type: "api_key", key }` or `{ type: "oauth", access, refresh, expires, headers?, baseUrl?, ...providerFields }` with `expires` in milliseconds since the epoch. axo stores what `login` returns, refreshes it within 5 minutes of expiry, and sends its access token, headers, and base URL as for any OAuth provider.
- **Rewrite:** receives the read-only model and the `ProviderRequest` axo's adapter built (`url`, ordered `headers`, JSON `body`) and returns the complete replacement. It runs once per inference request, before transport retries; model discovery is never rewritten.
- **Transport:** `send(model, request, context, signal)` receives the request after `rewrite` and `{ sessionId?, attempt }`, and returns a fetch `Response`. axo reads its status and headers, then streams its body as text (SSE or a JSON error body; binary bodies are not supported); a retryable status is retried as a new attempt. `signal` aborts when axo drops the response.
- **Model lists:** `listModels({ auth?, catalog }, signal)` receives the provider's resolved `ModelAuth` (absent without a credential) and `catalog(provider)`, which resolves to that models.dev provider's mapped rows. It returns `Model[]`, each naming this provider; a rejection keeps the previous list.
- **Signals:** each callback's signal aborts when the caller cancels, the call's deadline passes, or the runner's generation is retired.
- **Commands and tools:** A command gets `(args, session, signal)` with the trimmed arguments and a session handle. A tool gets a `ToolRun`: `run.call` is the validated call, `run.invocation` its execution ID, and `run.tools` the tools available during that call. Use `run.tools` for guarded file access (`readFile` requires UTF-8; use `readFileBytes` for binary data; both cap reads at 32 MiB) and supervised commands such as `run.tools.runBash({ command: "echo hi" })`, which runs bash in the session's execution environment. Call `ctx.session(run.sessionId)` if the tool needs a session handle. The tool also receives `run.signal` and `run.update`; acknowledged updates precede its final result.

  The bridge replaces any author-supplied `details.files` with the file records (or removes it when there are none); if there are records and details are non-null and not an object, the original details move under `details.value`. A failed call that touched files still carries its records. If the handler does not catch a `run.tools` rejection with code `tool`, the runner returns a model-facing `isError` result containing the error message.
- **Tool policies:** `before(scope, cx, signal)` can block a call; `after(scope, cx, signal)` can override result fields. Both receive the session's tool scope, including write restrictions, and the assistant message and tool call in `cx`. The after hook also receives the full result in `cx.result`. Return `null` to leave the call or result unchanged.
- **Spawn hooks:** `spawnHook(spawn, signal)` receives the parent session ID, task parts, spec and origin. A tool-originated spawn includes the originating call ID. Return `null` to allow, `{ block: reason }` to refuse, or `{ replace: spec }` to substitute an `AgentSpec`. A spawn context that cannot be serialized blocks the spawn.
- **Deadlines:** Tool and command calls have no bridge deadline. Policy, spawn, scheme and event calls have a 60 s deadline. A scheme can resolve to a virtual resource or an absolute file path, with optional handlers for virtual reads and writes. Scheme calls send the session directory and working directory as UTF-8 strings; if either is not valid UTF-8 the call fails naming the extension and scheme, without reaching the runner.
- **Command failures:** a failed extension command returns `Error::Host` boxing the axo-extensions crate’s `Failure`; hosts can downcast the source. `Removed` means the extension is no longer enabled in a live manager; `ManagerGone` means the manager was shut down or dropped. `Unavailable` means no runner could take the call, it stopped mid-call, or the deadline passed. `Method` means the handler failed; `Invalid` means the call was rejected locally without sending it.
- **Manifest entry handler:** With `"events": true`, `onEntry(entry, session)` gets a sequence-numbered journal entry and a session handle. The first argument has the SDK type `SeqEntry`: read the cursor from `entry.seq` and the payload from `entry.entry`; an entry tag these bindings do not know arrives as a `RawEntry` payload. The bridge offers new entries after attach in append order per session, calling extensions in enabled order. A slow handler delays later ones; lag, runner unavailability or callback failure can skip entries. This is independent of explicit session subscriptions.

### Extension session handles

`ctx.session(id)` returns a `SessionHandle` for an attached session or a known child, even outside a callback. Addressing a child can revive it; it does not attach the child or send its entries to the manifest entry handler. Use an [explicit subscription](#subscribing-to-live-events) while the child is resident. Read the ID as `session.id`. Ordinary methods return promises: the runner forwards the camelCase method name and positional arguments to `Session::invoke`; Rust decodes, executes and returns the result. The [generated reference](extension-api.md#session) lists the methods; the SDK's `bindings.ts` gives their wire types. The handle forwards only the invokable names the bridge sends at activation; any other property, such as `toJSON` or `then`, is `undefined`, so `JSON.stringify(session)` gives `{"id": ...}`. There is no per-call authority check.

Ordinary generated methods on `session`, `tools`, `host`, `models`, and `HostLifecycle` accept an optional trailing `AbortSignal`. Aborting withdraws a queued call or sends `cancel` for a sent call, and rejects the promise with the signal’s reason. Subscribe and unsubscribe are not canceled once sent.

For example, a command can enter a host-loaded mode with a task, or use the same rendered skill as a follow-up or a compaction focus:

```ts
await session.runCommand({ type: "mode", name: "plan", prompt: args });
// Or, in a separate command:
const focus = await session.renderSkill("review", args);
await session.compact(focus); // use session.followUp(focus) to submit it instead
```

The handle can answer pending questions, accept or revise mode results, and navigate. Fork and shutdown belong to the host. Extensions register through their manifest; they cannot change the host's registrations, sections or layer. The live schemes registry is not a session method on the handle.

| Wire method | Difference from the Rust host API |
| :--- | :--- |
| `post`, `appendCustom`, `custom` | Pass a bare, non-empty `customType`. Rust prefixes it with `<extension id>:` for message delivery and branch-scoped state; Rust hosts reading it directly use the prefixed key. An empty type rejects with `invalid_input`. This is namespacing, not journal access control. |
| `spawn`, `spawnTeam` | Return a `SpawnReceipt`, not a live child or team handle. |
| `parseCommand(text)` | Resolves to a parsed command, `null` for ordinary text, or rejects for invalid command arguments. |
| `readSnapshot(sha256)` | Snapshot bytes are base64. |
| `commandOutput(callId, offset, limit)`, `commandChunks(callId, offset, limit)` | Read the call's newest command. Output is `null` when unavailable or an `OutputPage` with job, artifact, progress and up to 1 MiB of raw bytes encoded as base64, starting at the byte offset. Chunks label the bytes by stream. |
| `commandJobs(callId)`, `jobOutput(jobId, offset, limit)`, `jobChunks(jobId, offset, limit)` | List every command the call started, oldest first, including foreground commands. Read a specific job's output or chunks by id. |
| `entries(after?, limit?)`, `live()` | Instead of attaching a live Rust receiver, page the journal after an exclusive sequence number (omit it to start at the beginning) and read the current `LiveSnapshot` separately. An `EntryPage` contains ordered `entries` and `more`; use the last `seq` as the next cursor. The limit defaults to 256 and is clamped to 1..=1024, so every page advances; lower it if the reply exceeds the frame limit. |

#### Subscribing to live events

`await session.subscribe(listener, kinds?)` returns a subscription; call `await subscription.close()` when finished. Omit `kinds` for all session events, or pass `SessionEventKind` values such as `["entry", "questionPending"]`. Each subscription is best effort, with no replay. Its `SessionStreamEvent` listener receives a `SessionEvent`, `{ type: "lagged", value: { skipped } }` when the receiver fell behind (an event too large to frame or not serializable also counts as one skipped), or `{ type: "closed", value: null }` when the stream closes. Use the journal cursor and live snapshot to catch up after lag. Each event waits for the listener's previous one, with no delivery deadline.

A subscription belongs to the runner generation that made it. Subscriptions only observe attached sessions or resident children; they never revive a child. A parked or unknown ID rejects with `no_session` (`session <id> is not attached or resident`). Detach or loss of child residency sends `closed` and ends the subscription; `closed` can arrive while the listener is still handling the previous event. A retired or crashed generation ends its subscriptions without notice, and they never reach or start a replacement runner. `close()` is idempotent: the listener stays registered until the host confirms, and a failed close rejects and can be retried. Closing a subscription the host already ended resolves.

**Handle errors by code.** The SDK's `HostErrorCode` includes snake_case Rust `ErrorCode` values such as `busy` and `skill_not_found`. Dispatch adds `unknown_method`, `invalid_arguments` (positional arguments did not decode) and `encode` (the result could not be encoded). The bridge rejects arguments it cannot decode with `invalid`, a reply that would exceed the frame bound with `too_large`, an unsupported object or operation with `unsupported`, a session ID it cannot resolve with `no_session`, and an unknown or foreign subscription with `no_subscription`. A child session that cannot be revived rejects with the underlying code (for example `session_busy` or `model_not_found`) rather than `no_session`. A tools call that fails with a tool error rejects with `tool`; a login question can reject with `interaction` (see [Author SDK](#author-sdk)); a lifecycle call without a host hook rejects with `no_lifecycle`; a shut-down manager rejects with `unavailable`. A completion stream whose event cannot be framed ends with `closed`.

A registered command runs at most once per session at a time: while `/<name>` runs, another `runCommand` of it in that session, whether nested in its own call or reached through a turn it started, is refused with `command /<name>: already running in this session`. Different extensions' commands never block each other. Each runner accepts up to 128 host calls in flight and queues 128 more; further calls reject immediately with `busy` (`host call queue is full`). A `run.update` resolves without delivery while the queue is full or after the tool’s signal aborts. If the queue makes no progress for 30 seconds, every **queued** call rejects with `stalled` (`too many nested host calls`). This detects a stall; it is not a deadline on calls already in flight.

### Wire protocol

`runner.ts` and the Rust peer use one private Unix socket per generation. A frame is a 4-byte big-endian length followed by UTF-8 JSON, bounded by `MAX_FRAME` (64 MiB). The runner authenticates the connection with `hello` and its nonce; later frames need no separate generation tag. The embedded runner and host ship together; there is no independent wire compatibility promise. The runner trusts arguments supplied by its parent, but still validates extension handler replies before serializing them. A `call` carries `id`, `ext`, `method` and `args`; its terminal reply is `ok` or `error`. Runner errors carry only a `code`; the host owns their wording. Host errors also carry a `message` and optional `details`. There are also `cancel`, `ping`, and `pong` frames.

Neither `hello` nor later frames carry a protocol version or generation identifier: the private socket and nonce establish the generation. Generation numbers remain host lifecycle/status metadata, not wire identity.

Login UI calls, tool updates and invocations on the `tools` object carry their live host call's `parent`. Other `invoke` calls are unparented: all specify `object`, `method` and positional `args`; only the session object takes a `target` session ID. `activate` carries `methods`, the map of names each object forwards.

`subscribe` takes `{ session, kinds }` or `{ completion }` and returns `{ sub }`; `unsubscribe` takes `{ sub }`. The host delivers events with `stream` on the subscribing connection, reporting closure with `{ sub, closed: true }` and session lag with `{ sub, lagged }`. Manifest entry handlers on host-attached sessions receive `entry` calls with `{ session, entry }`, in journal order. The host awaits each call before forwarding the next entry and continues after a handler rejects. Other calls may run concurrently so interaction does not block heartbeats. Malformed framing, duplicate live IDs or invalid parentage end the generation; an ordinary method refusal does not.
