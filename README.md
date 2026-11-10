# axo

**An embeddable, headless coding-agent harness core in Rust.**

`axo` is built to be linked directly into Rust hosts such as [Axome](https://github.com/tophc7/Axome), running durable agent sessions in a single process (~92 MiB PSS for one to three live sessions; see [plan/research/README.md](plan/research/README.md#memory-measurements)) without a sidecar runtime or terminal UI.

It ports [pi](https://github.com/earendil-works/pi) v1.3.0's core architecture—a sectioned system prompt stored in the transcript, delta-only streaming events, a tool-calling agent loop, and an append-only JSONL session tree—while adding crash recovery, recoverable `/shake` and cache-preserving compaction, cached Nix flake dev shells, and granular project access controls.

---

## Crates & Capabilities

The workspace is split into three layered crates so hosts only recompile what changes and non-coding agents can use the loop and provider layers independently:

| Crate | Purpose | Highlights |
| :--- | :--- | :--- |
| **[`axo-ai`](crates/axo-ai)** | LLM message model, streaming adapters, model catalog, and auth | Anthropic Messages, OpenAI Chat Completions, and OpenAI Codex Responses (`auth.json` OAuth + refresh locking); `models.dev` catalog with price-tier caps (lifted by `ModelsOptions::extended_context`); delta `Accumulator`; transient retry and context-overflow classification |
| **[`axo-agent`](crates/axo-agent)** | Generic tool-calling agent loop | Parallel and sequential `Tool` execution with a per-call `ToolContext` (cancellation, updates, pending-steer signal, invocation id), `Replay` crash-safety contracts (`Safe` vs `Never`), host policy via `ToolHooks`, the `LoopHooks` driver contract, steering and follow-up queues of identified `QueuedMessage`s, `MessageOrigin` provenance on persisted messages, `CancelReason` (`User` vs `Shutdown`), and one ordered, immutable event sink (`Persist`) |
| **[`axo`](crates/axo)** | Sessions, compaction, context assembly, config, and built-in tools | Durable `JsonlStore` with incremental `Projection` and `context_edit`s, single-writer OS locks, crash recovery, `attach` catch-up snapshots, recoverable `/shake` and live/separate compaction summaries, `DevShellEnv` Nix/direnv capture, host-selected command supervision (`Supervision`: process groups by default, opt-in cgroup v2 through an embedded supervisor entry point), per-session URL schemes (`artifact://`, `skill://`, `local://`, `axo://`, `proc://`, `agent://`, `tool://`, host `Scheme`s), background commands and subagents with `wait`, `kill`, and `send`, asynchronous `spawn` and schema-validated child `yield`, deferred tools through `call`, scoped `ToolPolicy`, and mode definition catalogs |

### Built-in tools (`crates/axo/src/tools`)

| Tool | Replay | Description |
| :--- | :--- | :--- |
| `read` | `Safe` | Reads UTF-8 text files (with `offset`/`limit` pagination) and images (`png`, `jpeg`, `gif`, `webp`, `bmp`), recording read hashes in the session's file ledger and loading nested directory `AGENTS.md` files on demand |
| `write` | `Safe` (plain paths / `local://`); host virtual writes choose their replay contract | Writes new files or complete rewrites, guarded by the read ledger when overwriting existing files. Scheme writes store content readable back at the same URL; actions are tools |
| `edit` | `Never` | Performs exact or whitespace/quote-fuzzy multi-hunk replacements against files recorded in the read ledger, rejecting stale external modifications |
| `bash` | `Never` | Executes commands under the session's `ExecEnv` (host, `direnv`, or cached Nix flake dev shell) in a dedicated container (its own process group, or a cgroup v2 job when the host selects `Supervision::Cgroup`), with head+tail inline truncation and full output spooled under the session directory; long commands (configurable `bash.promoteAfterMs`, default 60 s; a user steer; or `async: true`) continue as background jobs `bg_N`. Host tools share this command/job support through `SessionTools::run_command` |
| `grep` | `Safe` | In-process regex search built on `ignore` and `grep-searcher` |
| `find` | `Safe` | In-process gitignore-aware file discovery built on `ignore` and `globset` |
| `wait` | `Safe` | Returns pending command/subagent results or wakes for mail; a steer interrupts it. Outside teams, returns immediately without running jobs; in teams, detects mutual waits |
| `kill` | `Never` | Stops a background job or a subagent's run with `{"id":"bg_N"}` or `{"id":"agent_N"}`; available when `bash` or `spawn` remains |
| `spawn` | `Safe` | Starts a named, inline, or plain-delegate child, or a named/ad-hoc team, asynchronously; offered below `subagents.maxDepth`. Teams use `agent` or `members`, shared `task` or per-member `tasks`, optional `context`, and an ad-hoc `charter`; call-level `model` / `output` are single-agent only |
| `send` | `Never` | Messages a child, teammate, or parent with required `to` and `message`; optional `steer: true` interrupts only one's own child at its next steering boundary. Available with `spawn` or parent/team peers |
| `ask` | `Safe` | Emits `SessionEvent::QuestionPending` with structured single- or multi-select questions and waits for any attached client to answer via `Session::answer` (registered when `SessionOptions::ask_enabled` is `true`) |
| `yield` | Children: `Safe`; modes: `Safe` with review, otherwise `Never` | Children with an output schema receive a declared tool that returns the validated result and ends the run. A mode with a result schema instead mounts deferred `yield`, reached through `call` and `tool://yield`, optionally awaiting host review. Schemas with `"type": "object"` take the result directly; other schemas use required `result`. Allowlists and mode limits never filter mode `yield` |
| `call` | Routed tool's contract | Runs a deferred tool with required `tool` and optional object `args` (default `{}`). Read `tool://<name>` for its schema first; declared when deferred tools or a root mode catalog exist |
| `enter_mode` | `Safe` | Deferred tool for root sessions with model-invocable modes; required `mode` is an enum of those names. `disable-model-invocation` excludes a definition; persona allowlists can filter the tool. It selects the mode for the next step |

Persona allowlists do not filter `kill` or `send`; their presence rules still apply. The `proc://` and `agent://` schemes are read-only inspection surfaces, not action endpoints.

---

## Key Features

### Durable sessions & crash recovery

Every session is an append-only JSONL entry tree protected by an OS file lock with automatic crash recovery, live snapshot attachment, branch navigation, and forking. See [Session Storage](crates/axo/docs/sessions-and-compaction.md#session-storage-jsonlstore--sessionstore) and [Crash Recovery](crates/axo/docs/sessions-and-compaction.md#crash-recovery-cratesaxosrcrecoveryrs).

### Child sessions

Hosts spawn asynchronous children with `Session::spawn` and teams with `Session::spawn_team`; models use `spawn` and schema-validated `yield`. Each child has its own journal, persisted resolved spec, and one `agent_N` parent-registry row across all its runs, sharing concurrent run slots and team-boundary mail. See [Spawning child sessions](crates/axo/docs/extending.md#spawning-child-sessions) and [Child sessions](crates/axo/docs/sessions-and-compaction.md#child-sessions).

### Commands & prompt parts

`axo` owns the built-in commands (`/compact`, `/shake`, `/model`, `/thinking`, `/navigate`) and the prompt-part input shape; hosts expose the catalog via `session.commands()`. `Session::parse_command` parses built-ins first, then `/name [prompt]` for a loaded mode; `Session::run_command` enters that mode and delivers any prompt as a steer (starting a run when idle). The catalog also lists non-shadowed skills. See [Commands](crates/axo/docs/extending.md#commands) and [Prompt parts](crates/axo/docs/prompts-and-context.md#prompt-parts-cratesaxosrcinputrs).

### Supervised commands & background jobs

Every local command runs in a host-selected container (`Supervision::ProcessGroup` or `Supervision::Cgroup`); long commands continue as `bg_N` background jobs delivered exactly once. See [Command supervision](crates/axo/docs/configuration.md#command-supervision-axosupervision) and [Background Job Durability](crates/axo/docs/sessions-and-compaction.md#background-job-durability).

### Cache-aware compaction & `/shake`

`axo` manages context pressure through append-only context edits, manual `/shake` elision, and one-action-per-pass automatic clearing or prompt-cached summarization. See [Automatic Compaction](crates/axo/docs/sessions-and-compaction.md#automatic-compaction-cratesaxosrccompaction).

### Sectioned system prompt, context files & skills

The system prompt is assembled into named `<section>` blocks (`delegation` for children, `system`, `docs`, `tools`, `schemes`, `work`, `prose`, `project_context`, `skills`, `team` for team members, `cwd`, `dev_shell`, `date`, plus host sections). Startup and tool execution discover instructions, skills, and personas across user and project roots. See [System prompt sections](crates/axo/docs/prompts-and-context.md#system-prompt-sections), [Context files](crates/axo/docs/prompts-and-context.md#context-files-cratesaxosrccontextfilesrs), and [Skills](crates/axo/docs/prompts-and-context.md#skills-cratesaxosrccontextskillsrs).

Hosts can switch a root persona and replace live host sections and tools; see [Session control methods](crates/axo/docs/extending.md#session-control-methods).

### Deferred tools, policies & session modes

Hosts supply `Vec<HostTool>` through `SessionOptions::tools` or `Session::set_tools`, explicitly choosing `HostTool::direct` or `HostTool::deferred` exposure. Deferred tools are documented at `tool://` and announced by durable `tool_catalog` notices, keeping their guidelines out of the system prompt. With the dispatcher already present and no native promotion, mounting or calling them leaves the declared tool list unchanged. Reading a tool's docs can promote it on models whose `Model::tool_support()` permits tool additions or changes.

Calls routed through `call` have one identity: hooks, policies, execution events, and results use the inner tool's name and arguments; only the assistant's call block retains `call`. Hosts pair them by call id. `SessionOptions::tool_policies` supplies ordered `ToolPolicy` values with a `ToolScope` containing session, depth, persona, and the effective mode. See [Extension traits](crates/axo/docs/extending.md) and [Deferred tools](crates/axo/docs/prompts-and-context.md#deferred-tools).

Root sessions load `ModeSpec` definitions from explicit `ConfigLayer::mode_roots` and `SessionOptions::modes`, expose them through `Session::modes()` and the command catalog, and validate names and result schemas. axo bundles no modes. Select one with `Session::set_mode(Some(name))` or a mode command; leave with `Session::set_mode(None)`. `Session::mode()`, `SessionEvent::Mode`, and `LiveSnapshot::mode` expose the selected state.

Mode selection is journaled immediately; its instructions and call-time tool limits become effective together at the next run start or turn boundary. Durable `mode_notice` messages reconcile the effective mode with the current context before requests and restore its instructions after compaction. Limits do not rewrite the system prompt or declared tool list, preserving the cached prefix. Modes follow the journal branch on navigation, forks, and reopen. A mode's optional model rule restores only its own selection change on exit; an explicit intervening model or thinking change is kept. Children inherit the effective tool limits in their persisted spawn spec, not the mode itself. See [Mode definitions](crates/axo/docs/prompts-and-context.md#mode-definitions) and [Session storage](crates/axo/docs/sessions-and-compaction.md#session-storage-jsonlstore--sessionstore).

Modes with `result` mount deferred `yield`; reading `tool://yield` promotes it only on models that support removal as well as addition. A yield is admitted only while its mode is both selected and effective. Invalid arguments leave the mode active. Without review, a valid result emits `SessionEvent::ModeResult` with `awaiting_review: false` and ends that mode if it is still selected. With review, the event and `LiveSnapshot::pending_results` expose `PendingResult { call_id, mode, result }` until the host decides:

- `Session::accept_result(call_id, message)` ends the mode when the waiting tool resumes, provided it is still selected; a racing replacement selection is never cleared. The optional message overrides `result.then`; either provides continuation text for the model. With neither, a successful yield terminates the run.
- `Session::revise_result(call_id, feedback)` keeps the mode active and returns the feedback to the model to revise and call `yield` again. Blank feedback is rejected.
- `Session::dismiss(call_id)` settles the review and aborts the run. Changing modes supersedes outstanding reviews; selecting the already-selected mode is a no-op. `SessionEvent::ModeResultResolved` removes a settled or withdrawn review from the host UI.

The host-tool name `call` is always reserved for deferred dispatch; sessions with modes also reserve `yield`. Such host registrations are ignored with diagnostics. See [Host integration](crates/axo/docs/extending.md) for the decision methods, errors, and review lifecycle.

### Nix flake & direnv dev shells (`DevShellEnv`)

When `bash.devShell` is set to `"nix"` or `"direnv"` in a trusted project, `axo` captures and caches the project's environment and runs subsequent commands directly against it (~3 ms overhead per command instead of ~460 ms for `nix develop -c`). See [Execution environment](crates/axo/docs/configuration.md#execution-environment-bash--axobashsettings).
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

axo owns no configuration directory and reads no configuration files of its own. The host supplies models and a session store, and optionally a `ConfigSource` (user and project settings layers, prompt overrides, skill roots, context files) and a cache directory. `Axo::builder(models, store)...build()` does no configuration I/O; axo samples the source on each `create_session`, `open_session`, and `Axo::schemes` call, and calls `ConfigSource::project` only when the session's project access is `read` or `trust` and `project.files.config` is on (the default).

See [Embedding `axo` in a Rust Host](crates/axo/docs/extending.md#2-embedding-axo-in-a-rust-host) for the complete builder, session, and structured-input example.

Without `.config(..)`, sessions run on default settings with no project layer. Without `.cache_dir(..)`, the `docs` prompt section and the `axo://` scheme are absent, and Nix dev shells fall back to the host environment with a warning.

`AxoBuilder::name("axome")?` renames the docs scheme and section and the bundled `SYSTEM`/`SUMMARY` self-references; the default is `axo`. Prompt overrides remain verbatim, and crate names do not change.

Durable server hosts can opt into cgroup supervision (`Supervision::Cgroup`), where commands run in cgroup v2 jobs and the host routes arguments in `main` to `axo::exec::supervisor::run`. See [Command supervision](crates/axo/docs/configuration.md#command-supervision-axosupervision) for the synchronous routing `main` example and failure details.

---

## Layout

```text
axo/
├── crates/
│   ├── axo-ai/                     # Message model, streaming adapters, model catalog, OAuth & credentials
│   ├── axo-agent/                  # Tool-calling loop, hooks, queues, cancellation, split event delivery
│   └── axo/                        # Axo & Session APIs, JsonlStore, compaction, context, DevShellEnv, supervision, jobs, tools
│       ├── src/exec/supervisor.rs  # Embedded guard/enter entry point hosts route to for cgroup supervision
│       ├── prompts/                # Bundled SYSTEM.md, PROSE.md, DELEGATION.md, and summarizer markdown prompts
│       ├── examples/               # `drive` (CLI session driver) and `memory` (PSS benchmark); their own config layout under `--dir`
│       └── docs/                   # Bundled implementation reference for the `<docs>` system-prompt section
│           ├── README.md           # Documentation index
│           ├── configuration.md    # ConfigSource and EffectiveConfig, settings, models, project access, dev shells, supervision
│           ├── prompts-and-context.md     # Prompt sections, overrides, context files, skills, agents/teams, background jobs, prompt parts
│           ├── sessions-and-compaction.md # JsonlStore, projection, recovery, attach, /shake, and compaction
│           └── extending.md        # Declarative files, Rust embedding (Session, commands), and extension traits
└── plan/                           # Architecture design, pi port studies, and background research
    ├── design.md                   # Goals, non-goals, architecture, and settled core decisions
    ├── plan.md                     # Active planning, next tracks, and open design discussions
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
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Drive a live or resumed session from the command line with the `drive` example:

```sh
cargo run -p axo --example drive -- --dir <dir> --cwd . --model zebes/qwen3.8-27b "Inspect @crates/axo/src/lib.rs"
cargo run -p axo --example drive -- --dir <dir> --cwd . --attach /tmp/shot.png "What is in this image?"
cargo run -p axo --example drive -- --dir <dir> --cwd . --commands
cargo run -p axo --example drive -- --dir <dir> --resume <dir>/sessions/--<encoded-cwd>--/<timestamp>_<uuidv7>.jsonl "/model openai-codex/gpt-6-luna"
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

- **Implementation Reference (`crates/axo/docs/`):** Start at [`crates/axo/docs/README.md`](crates/axo/docs/README.md) for [`configuration.md`](crates/axo/docs/configuration.md), [`prompts-and-context.md`](crates/axo/docs/prompts-and-context.md), [`sessions-and-compaction.md`](crates/axo/docs/sessions-and-compaction.md), and [`extending.md`](crates/axo/docs/extending.md).
- **Planning & Research (`plan/`):** See [`plan/design.md`](plan/design.md), [`plan/plan.md`](plan/plan.md), [`plan/pi/`](plan/pi/README.md), and [`plan/research/`](plan/research/README.md).

## License

[MIT](./LICENSE). `axo` ports ideas and prompt text from [pi](https://github.com/earendil-works/pi) and [oh-my-pi](https://github.com/can1357/oh-my-pi), both MIT-licensed; code and prompts adapted from them retain their copyright notices.
