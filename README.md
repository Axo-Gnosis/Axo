# axo

**An embeddable, headless coding-agent harness core in Rust.**

`axo` is built to be linked directly into Rust hosts such as [Axome](https://github.com/tophc7/Axome), running durable agent sessions in a single process (~92 MiB PSS idle for one to three live sessions; see [plan/research/README.md](plan/research/README.md#memory-measurements)) without a sidecar runtime or terminal UI.

It ports [pi](https://github.com/earendil-works/pi) v1.3.0's core architecture—a sectioned system prompt stored in the transcript, delta-only streaming events, a tool-calling agent loop, and an append-only JSONL session tree—while adding crash recovery, recoverable `/shake` and cache-preserving compaction, cached Nix flake dev shells, and granular project access controls.

---

## Crates & Capabilities

The workspace is split into three layered core crates so hosts only recompile what changes and non-coding agents can use the loop and provider layers independently, plus an optional bridge crate and a procedural-macro crate:

| Crate | Purpose | Highlights |
| :--- | :--- | :--- |
| **[`axo-ai`](crates/axo-ai)** | LLM message model, streaming adapters, model catalog, and auth | Anthropic Messages, OpenAI Chat Completions, OpenAI Responses, and Google Gemini (`auth.json` OAuth + refresh locking); runtime provider registration with typed login (`ProviderLogin`, `LoginInteraction`), request rewrite (`RequestRewrite`), provider-owned transport (`ProviderTransport`), runtime model lists (`ProviderModels`) and account usage limits (`ProviderUsage`); `models.dev` catalog, refreshed in place (`Models::refresh`), with price-tier caps (lifted by `ModelsOptions::extended_context`) and cache-preserving tool-change capability (`Model::tool_support`); delta `Accumulator`; transient retry and context-overflow classification |
| **[`axo-agent`](crates/axo-agent)** | Generic tool-calling agent loop | Parallel and sequential `Tool` execution with a per-call `ToolContext` (cancellation, updates, pending-steer signal, invocation id), deferred tools and dispatcher routing (`Tool::target`, `route`), `Replay` crash-safety contracts (`Safe` vs `Never`), host policy via `ToolHooks`, the `LoopHooks` driver contract, steering and follow-up queues of identified `QueuedMessage`s, `MessageOrigin` provenance on persisted messages, `CancelReason` (`User` vs `Shutdown`), and one ordered, immutable event sink (`Persist`) |
| **[`axo`](crates/axo)** | Sessions, compaction, context assembly, config, and built-in tools | Durable `JsonlStore` with incremental `Projection` and `context_edit`s, single-writer OS locks, crash recovery, `attach` catch-up snapshots, recoverable `/shake` and live/separate compaction summaries, `DevShellEnv` Nix/direnv capture, host-selected command supervision (`Supervision`: process groups by default, opt-in cgroup v2 through an embedded supervisor entry point), per-session URL schemes (`artifact://`, `skill://`, `local://`, `axo://`, `proc://`, `agent://`, `tool://`, host `Scheme`s), background commands and subagents with `wait`, `kill`, and `send`, asynchronous `spawn` and schema-validated child `yield`, deferred tools through `call`, scoped `ToolPolicy`, and [live definitions](crates/axo/docs/extending.md#live-definitions) for agents, teams, modes, keywords and skills |
| **[`axo-extensions`](crates/axo-extensions)** | Optional Bun bridge for providers and session capabilities | Static `extension.json` declarations for providers, tools, policies, spawn hooks, schemes, sections, definitions and commands. `Extensions` composes host and user-enabled contributions, exposes diagnostics, and starts supervised runners on demand. Type-only SDK in [`packages/axo-extension-sdk`](packages/axo-extension-sdk) |
| **[`axo-macros`](crates/axo-macros)** | Procedural macros for the extension surface | `#[expose]` validates public-method exposure decisions; named blocks generate JSON dispatch and type metadata for `axo` and `axo-extensions`, while the bare form validates withheld-only impls |

### Built-in tools

| Tool | Replay | Description |
| :--- | :--- | :--- |
| `read` | `Safe` | Reads text as numbered lines with line selectors, several paths per call, one-level directory listings and outlines of large code files, and images (`png`, `jpeg`, `gif`, `webp`, `bmp`), recording read hashes in the session's file ledger and loading nested directory `AGENTS.md` files on demand ([output format](crates/axo/docs/prompts-and-context.md)). Local content targets must be regular files or directories |
| `write` | `Safe` for plain paths; scheme URLs use `Scheme::write_replay` (default `Never`; `local://` is `Safe`) | Writes new files or complete rewrites, guarded by the read ledger when overwriting existing files. Scheme writes store content readable back at the same URL; actions are tools |
| `edit` | `Never` | Performs exact or whitespace/quote-fuzzy multi-hunk replacements against UTF-8 files; every `oldText` must match the current file exactly once, so no earlier read is needed. A pasted `N⇥` line-number prefix is stripped |
| `bash` | `Never` | Executes commands under the session's `ExecEnv` (host, `direnv`, or cached Nix flake dev shell) in a dedicated container (its own process group, or a cgroup v2 job when the host selects `Supervision::Cgroup`), with head+tail inline truncation and output spooled under the session directory; long commands (configurable `bash.promoteAfterMs`, default 60 s; a user steer; or `async: true`) continue as background jobs `bg_N`. Host tools share this command/job support through `SessionTools::run_command`. Successful file views load nested `AGENTS.md` files like `read`, and `bash.blockToolViews` (off by default) refuses a command that is one view or search a built-in tool does, naming the call (`axo::classify_bash`) |
| `grep` | `Safe` | In-process regex search built on `ignore` and `grep-searcher`: results grouped per file with per-file caps, path lists with line selectors and `!glob` exclusions, context lines, and `files` and `count` modes ([output format](crates/axo/docs/prompts-and-context.md)) |
| `find` | `Safe` | In-process gitignore-aware file discovery built on `ignore` and `globset` |
| `wait` | `Safe` | Returns pending command/subagent results or wakes for mail; a steer interrupts it. Outside teams, returns immediately without running jobs; in teams, detects mutual waits |
| `kill` | `Never` | Stops a background job or a subagent's run with `{"id":"bg_N"}` or `{"id":"agent_N"}`; available when `bash` or `spawn` remains |
| `spawn` | `Safe` | Starts a named, inline, or plain-delegate child, or a named/ad-hoc team, asynchronously; offered below `subagents.maxDepth`. Teams use `agent` or `members`, shared `task` or per-member `tasks`, optional `context`, and an ad-hoc `charter`; call-level `model` / `output` are single-agent only |
| `send` | `Never` | Messages a child, teammate, or parent with required `to` and `message`; optional `steer: true` interrupts only one's own child at its next steering boundary. Available with `spawn` or parent/team peers |
| `ask` | `Safe` | Sends one document of Markdown and typed questions (`choice`, `text`, `number`) as `SessionEvent::QuestionPending` and waits for any attached client to answer via `Session::answer`, which validates the submission against the document; answers return keyed by question id (registered when `SessionOptions::ask_enabled` is `true`) |
| `yield` | Children: `Safe`; modes: `Safe` with review, otherwise `Never` | Children with an output schema receive a declared tool that returns the validated result and ends the run. A mode with a result schema instead mounts deferred `yield`, reached through `call` and `tool://yield`, optionally awaiting host review. Schemas with `"type": "object"` take the result directly; other schemas use required `result`. Allowlists and mode limits never filter mode `yield` |
| `call` | Routed tool's contract | Runs a deferred tool with required `tool` and optional object `args` (default `{}`). Read `tool://<name>` for its schema first; declared when deferred tools remain or a root session has loaded modes or a selected or effective mode |
| `enter_mode` | `Safe` | Deferred tool for root sessions with model-invocable modes; required `mode` is an enum of those names. `disable-model-invocation` excludes a definition; persona allowlists can filter the tool. It selects the mode for the next step |

Persona allowlists do not filter `kill` or `send`; their presence rules still apply. The `proc://` and `agent://` schemes are read-only inspection surfaces, not action endpoints.

---

## Key Features

### Durable sessions & crash recovery

Every session is an append-only JSONL entry tree protected by an OS file lock with automatic crash recovery, live snapshot attachment, branch navigation, and forking. See [Session Storage](crates/axo/docs/sessions-and-compaction.md#session-storage-jsonlstore--sessionstore) and [Crash Recovery](crates/axo/docs/sessions-and-compaction.md#crash-recovery-cratesaxosrcrecoveryrs).

### Child sessions

Hosts spawn asynchronous children with `Session::spawn` and teams with `Session::spawn_team`; models use `spawn` and schema-validated `yield`. Each child has its own journal, persisted resolved spec, and one `agent_N` parent-registry row across all its runs, sharing concurrent run slots and team-boundary mail. See [Spawning child sessions](crates/axo/docs/extending.md#spawning-child-sessions) and [Child sessions](crates/axo/docs/sessions-and-compaction.md#child-sessions).

### Commands & prompt parts

`axo` owns the built-in commands (`/compact`, `/shake`, `/model`, `/thinking`, `/tier`, `/navigate`) and the prompt-part input shape; hosts expose the catalog via `session.commands()`. `Session::parse_command` parses built-ins first, then `/name [prompt]` for a loaded mode, then `/name [args]` for a host-registered command (`SessionOptions::host.commands`, `Session::set_commands`); `Session::run_command` enters that mode and delivers any prompt as a steer (starting a run when idle), or awaits the registered `CommandHandler`. The catalog also lists non-shadowed skills. See [Commands](crates/axo/docs/extending.md#commands) and [Prompt parts](crates/axo/docs/prompts-and-context.md#prompt-parts-cratesaxosrcinputrs).

### Supervised commands & background jobs

Every local command runs in a host-selected container (`Supervision::ProcessGroup` or `Supervision::Cgroup`); long commands continue as `bg_N` background jobs delivered exactly once. See [Command supervision](crates/axo/docs/configuration.md#command-supervision-axosupervision) and [Background Job Durability](crates/axo/docs/sessions-and-compaction.md#background-job-durability).

Hosts can read live or retained output as raw merged bytes or stdout/stderr-tagged chunks through `CommandOutput`, with durable `.streams` sidecars. Ordering is capture order, not guaranteed cross-stream write chronology. See [Background jobs for hosts](crates/axo/docs/extending.md#background-jobs-for-hosts).

### Cache-aware compaction & `/shake`

`axo` manages context pressure through append-only context edits, manual `/shake` elision, and one-action-per-pass automatic clearing or prompt-cached summarization. See [Automatic Compaction](crates/axo/docs/sessions-and-compaction.md#automatic-compaction-cratesaxosrccompaction).

Anthropic prompt caches default to 1 hour on subscription root sessions and 5 minutes elsewhere (`cacheTtl`); a paused session's 5-minute cache is kept warm by `max_tokens: 0` refresh requests when the expected saving pays for them (`cacheWarming`). See [Prompt caching](crates/axo/docs/configuration.md#prompt-caching-cachettl--cachewarming).

### Sectioned system prompt, context files & skills

The system prompt is assembled into named `<section>` blocks (`delegation` for children, `system`, `agent` for a persona or child spec body, `docs`, `tools`, `schemes`, `work`, `prose`, `project_context`, `skills`, `team` for team members, `cwd`, `dev_shell`, `date`, plus host sections). Startup and tool execution discover instructions, skills, and personas across user and project roots. See [System prompt sections](crates/axo/docs/prompts-and-context.md#system-prompt-sections), [Context files](crates/axo/docs/prompts-and-context.md#context-files-cratesaxosrccontextfilesrs), and [Skills](crates/axo/docs/prompts-and-context.md#skills-cratesaxosrccontextskillsrs).

Hosts can switch a root persona, replace live host sections and tools, and inspect built versus recorded prompt sections, context-file inventories, and the file ledger. Skill roots belong to user and project `ConfigLayer`s; project roots require the skills gate at `Read` or `Trust`. See [Session control methods](crates/axo/docs/extending.md#session-control-methods).

### Deferred tools, policies & session modes

Hosts supply `Vec<HostTool>` with explicit `HostTool::direct` or `HostTool::deferred` exposure, documented at `tool://`, invoked through `call` with inner-tool identity, and promoted when [`Model::tool_support()`](crates/axo/docs/configuration.md#tool-list-capabilities) allows; `SessionOptions::host.tool_policies` runs ordered `ToolPolicy` checks over `ToolScope`. Root sessions load `ModeSpec` definitions for branch-scoped, boundary-effective instruction overlays, call-time tool limits and `writes` scopes (both persisted onto spawned descendants as ceilings), model restoration, and optional host-reviewed `yield` results. See [Deferred tools](crates/axo/docs/prompts-and-context.md#deferred-tools), [Mode definitions](crates/axo/docs/prompts-and-context.md#mode-definitions), [Write scope](crates/axo/docs/prompts-and-context.md#write-scope), and [Root-session modes](crates/axo/docs/extending.md#root-session-modes).

Typed host surfaces expose session activity (an idle observation is not a reservation), selected versus effective modes, schema-validated edited result acceptance, retry failure details, and shared coordination receipts through `Session::send` and `Session::kill_job`. See [Session events](crates/axo/docs/extending.md#sessionevent) and [Coordination receipts](crates/axo/docs/extending.md#coordination-receipts).

### Nix flake & direnv dev shells (`DevShellEnv`)

When `bash.devShell` is set to `"nix"` or `"direnv"` in a trusted project, `axo` captures and caches the project's environment and runs subsequent commands directly against it. In [Nix flake measurements](plan/research/nix-shell.md), cached execution added about 3 ms per command, versus 120–464 ms for `nix develop -c`. See [Execution environment](crates/axo/docs/configuration.md#execution-environment-bash--axobashsettings).

### Project access levels (`ignore` | `read` | `trust`)

Configured exclusively in the user layer the host supplies through `ConfigSource::user` (a project cannot elevate its own access):

```jsonc
{
  "defaultModel": "anthropic/claude-sonnet-4-5",
  "bash": {
    "devShell": "nix"
  },
  "project": {
    "access": "read",
    "paths": {
      "/repo/**": "trust",
      "~/scratch/**": "ignore"
    }
  }
}
```

- `"ignore"` loads nothing from the repository.
- `"read"` (default) loads markdown context files, skills, prompt overrides, and settings. `bash.devShell` resolves to `off`, even if enabled in user settings.
- `"trust"` permits all project settings and is required for dev shells regardless of which layer enables `bash.devShell`. Config values starting with `!` are rejected at every access level.

---

## Embedding in Rust

axo owns no configuration directory and reads no configuration files of its own. The host supplies models and a session store, and optionally a `ConfigSource` (user and project settings layers, prompt overrides, definition roots, context files) and a cache directory. `Axo::builder(models, store)...build()` does no configuration I/O; see [Configuration source](crates/axo/docs/configuration.md#configuration-source-axoconfigsource) for when axo samples the user and project layers.

See [Embedding `axo` in a Rust Host](crates/axo/docs/extending.md#2-embedding-axo-in-a-rust-host) for the complete builder, session, and structured-input example.

Without `.config(..)`, sessions run on default settings with no project layer. Without `.cache_dir(..)`, the `docs` prompt section and the `axo://` scheme are absent, and Nix dev shells fall back to the host environment with a warning.

`AxoBuilder::name("axome")?` renames the docs scheme and changes the text of the `docs` section (its key stays `docs`) and the bundled `SYSTEM`/`SUMMARY` self-references; the default is `axo`. Prompt overrides remain verbatim, and crate names do not change.

Hosts keep one `Models` for their lifetime: `Models::reload` applies configuration changes in place, `Models::refresh` (also run on a timer) applies fresh catalog data and re-lists runtime providers, open sessions pick up a changed model definition at their next request, `Models::register` and `Models::unregister` add and remove runtime providers (with their own `ProviderLogin`, `RequestRewrite`, `ProviderTransport`, `ProviderModels` and `ProviderUsage`), and `Models::login` runs a provider's login through the host's `LoginInteraction`. Hosts can list provider metadata with `Models::providers()` and inspect credential provenance with `Models::credential_source()` without exposing secrets, validating keys, or refreshing OAuth tokens. See [Models & Credentials](crates/axo/docs/configuration.md#models-axo_aimodelsconfig--credentials).

axo speaks wire protocols only; subscription providers such as ChatGPT Codex and Claude subscription live outside axo (for example in Axo-Extras) and are registered by the host through the same provider seam. See [Runtime providers, login, and reload](crates/axo/docs/configuration.md#runtime-providers-login-and-reload).

Durable server hosts can opt into cgroup supervision (`Supervision::Cgroup`), where commands run in cgroup v2 jobs and the host routes arguments in `main` to `axo::exec::supervisor::run`. See [Command supervision](crates/axo/docs/configuration.md#command-supervision-axosupervision) for the synchronous routing `main` example and failure details.

For TypeScript/JavaScript extensions, pass the host's `Arc<Axo>` and `ExtensionOptions` to `Extensions::new` (which rejects relative paths without doing I/O), then call `configure(&settings.extensions)`. Compose session options before create/open and attach the resulting session (and any host-created forks) for live reconfiguration and journal events. Extensions can also use guarded tool operations and the host's model registry; session creation, open, fork and close require a host-provided `Lifecycle` hook. Shut the manager down on exit. The host supplies the Bun binary and package, store, data and runtime locations; project settings cannot enable extensions. With `ExtensionOptions::auto_install` enabled, axo installs missing or stale dependencies for enabled authored extensions; otherwise the host calls `ExtensionSet::install_dependencies`. `Extensions::install` places host-acquired packages in the managed store with update, rollback and uninstall. See [Bun extensions](crates/axo/docs/extending.md#4-bun-extensions-axo-extensions) and the [extension API reference](crates/axo/docs/extension-api.md).

---

## Layout

```text
axo/
├── crates/
│   ├── axo-ai/                     # Message model, streaming adapters, model catalog, OAuth & credentials
│   ├── axo-agent/                  # Tool-calling loop, hooks, queues, cancellation, split event delivery
│   ├── axo/                        # Axo & Session APIs, JsonlStore, compaction, context, DevShellEnv, supervision, jobs, tools
│   │   ├── src/exec/supervisor.rs  # Embedded guard/enter entry point hosts route to for cgroup supervision
│   │   ├── prompts/                # Bundled SYSTEM.md, PROSE.md, DELEGATION.md, and summarizer markdown prompts
│   │   ├── examples/               # `drive` (CLI session driver) and `memory` (PSS benchmark); their own config layout under `--dir`
│   │   └── docs/                   # Bundled implementation reference for the `<docs>` system-prompt section
│   │       ├── README.md           # Documentation index
│   │       ├── configuration.md    # ConfigSource and EffectiveConfig, settings, extensions list, models, provider layers, project access, dev shells, supervision
│   │       ├── prompts-and-context.md     # Prompt sections, overrides, context files, skills, agents/teams, background jobs, prompt parts
│   │       ├── sessions-and-compaction.md # JsonlStore, projection, recovery, attach, /shake, and compaction
│   │       ├── extending.md        # Declarative files, Rust embedding, and Bun extensions
│   │       └── extension-api.md    # Generated extension API reference
│   ├── axo-macros/                 # #[expose] procedural macro for the extension surface
│   └── axo-extensions/             # Manifests, provider layers, session bindings, supervised Bun runners
│       ├── src/runner.ts           # Bun-side runner, embedded and materialized per generation
│       └── tests/                  # `bridge`, `bun_provider` (opt-in), `bindings`, `runner-activation.test.ts` (Bun)
├── packages/
│   └── axo-extension-sdk/          # Type-only `@axo/extension-sdk` author contract
└── plan/                           # Architecture design, pi port studies, and background research
    ├── design.md                   # Goals, non-goals, architecture, and settled core decisions
    ├── plan.md                     # Active planning, next tracks, and open design discussions
    ├── handoff/                    # Handoff briefs
    ├── pi/                         # Source studies of pi v1.3.0
    └── research/                   # Background studies and memory/compaction measurements
```

---

## Development

Enter the Nix development shell for the Rust toolchain (`cargo`, `rustc`, `clippy`, `rustfmt`, `rust-analyzer`):

```sh
nix develop
cargo check --workspace --all-targets
cargo test --workspace  # process-group mode: no cgroup delegation or helper binary needed
systemd-run --user --scope -p Delegate=yes -- env AXO_TEST_CGROUP=1 cargo test -p axo --test supervisor  # opt-in cgroup cases
cargo test -p axo-extensions --test bridge  # manager, lifecycle, session and install checks with the test binary as a scripted Rust runner; no Bun needed
env AXO_TEST_BUN=/absolute/path/to/bun cargo test -p axo-extensions --test bun_provider  # real Bun runner; skips when unset, fails when set but unusable
systemd-run --user --scope -p Delegate=yes -- env AXO_TEST_CGROUP=1 AXO_TEST_BUN=/absolute/path/to/bun cargo test -p axo-extensions --test bun_provider  # same, under cgroup supervision
bun test crates/axo-extensions/tests/runner-activation.test.ts  # embedded runner activation and rollback checks; needs Bun
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Drive a live or resumed session from the command line with the `drive` example:

```sh
cargo run -p axo --example drive -- --dir <dir> --cwd . --model zebes/qwen3.8-27b "Inspect @crates/axo/src/lib.rs"
cargo run -p axo --example drive -- --dir <dir> --cwd . --attach /tmp/shot.png "What is in this image?"
cargo run -p axo --example drive -- --dir <dir> --cwd . --commands
cargo run -p axo --example drive -- --dir <dir> --resume <dir>/sessions/--<encoded-cwd>--/<timestamp>_<uuidv7>.jsonl "/model anthropic/claude-sonnet-4-5"
cargo run -p axo --example drive -- --dir <dir> --resume <dir>/sessions/--<encoded-cwd>--/<timestamp>_<uuidv7>.jsonl "/shake elide"
# cgroup supervision with drive itself as the supervisor; needs a delegated cgroup
systemd-run --user --scope -p Delegate=yes -- cargo run -p axo --example drive -- --dir <dir> --cgroup --cwd . "Run bash: cat /proc/self/cgroup"
```

Measure idle and mid-turn proportional set size (PSS) across N concurrent sessions in one process:

```sh
cargo run --release -p axo --example memory -- --sessions 3
```

---

## Documentation

- **Implementation Reference (`crates/axo/docs/`):** Start at [`crates/axo/docs/README.md`](crates/axo/docs/README.md) for configuration, prompts and context, sessions and compaction, embedding and extensions, and the generated [extension API reference](crates/axo/docs/extension-api.md).
- **Planning & Research (`plan/`):** See [`plan/design.md`](plan/design.md), [`plan/plan.md`](plan/plan.md), [`plan/pi/`](plan/pi/README.md), and [`plan/research/`](plan/research/README.md).

## License

[MIT](./LICENSE). `axo` ports ideas and prompt text from [pi](https://github.com/earendil-works/pi) and [oh-my-pi](https://github.com/can1357/oh-my-pi), both MIT-licensed; code and prompts adapted from them retain their copyright notices.
