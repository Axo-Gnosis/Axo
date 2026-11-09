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
| **[`axo`](crates/axo)** | Sessions, compaction, context assembly, config, and built-in tools | Durable `JsonlStore` with incremental `Projection` and `context_edit`s, single-writer OS locks, crash recovery, `attach` catch-up snapshots, recoverable `/shake` and live/separate compaction summaries, `DevShellEnv` Nix/direnv capture, host-selected command supervision (`Supervision`: process groups by default, opt-in cgroup v2 through an embedded supervisor entry point), per-session URL schemes (`artifact://`, `skill://`, `local://`, `axo://`, `proc://`, `agent://`, host `Scheme`s), background commands and subagents with `wait`, asynchronous `spawn` and schema-validated `yield`, and the built-in tool set |

### Built-in tools (`crates/axo/src/tools`)

| Tool | Replay | Description |
| :--- | :--- | :--- |
| `read` | `Safe` | Reads UTF-8 text files (with `offset`/`limit` pagination) and images (`png`, `jpeg`, `gif`, `webp`, `bmp`), recording read hashes in the session's file ledger and loading nested directory `AGENTS.md` files on demand |
| `write` | `Safe` (plain paths / `local://`) / `Never` (action schemes) | Writes new files or complete rewrites, guarded by the read ledger when overwriting existing files |
| `edit` | `Never` | Performs exact or whitespace/quote-fuzzy multi-hunk replacements against files recorded in the read ledger, rejecting stale external modifications |
| `bash` | `Never` | Executes commands under the session's `ExecEnv` (host, `direnv`, or cached Nix flake dev shell) in a dedicated container (its own process group, or a cgroup v2 job when the host selects `Supervision::Cgroup`), with head+tail inline truncation and full output spooled under the session directory; long commands (configurable `bash.promoteAfterMs`, default 60 s; a user steer; or `async: true`) continue as background jobs `bg_N`. Host tools share this command/job support through `SessionTools::run_command` |
| `grep` | `Safe` | In-process regex search built on `ignore` and `grep-searcher` |
| `find` | `Safe` | In-process gitignore-aware file discovery built on `ignore` and `globset` |
| `wait` | `Safe` | Returns pending command/subagent results or wakes for mail; a steer interrupts it. Outside teams, returns immediately without running jobs; in teams, detects mutual waits |
| `spawn` | `Safe` | Starts a named, inline, or plain-delegate child, or a named/ad-hoc team, asynchronously; offered below `subagents.maxDepth`. Teams use `agent` or `members`, shared `task` or per-member `tasks`, optional `context`, and an ad-hoc `charter`; call-level `model` / `output` are single-agent only |
| `ask` | `Safe` | Emits `SessionEvent::QuestionPending` with structured single- or multi-select questions and waits for any attached client to answer via `Session::answer` (registered when `SessionOptions::ask_enabled` is `true`) |
| `yield` | `Safe` | Added only to children with an output schema, after allowlist filtering; returns the validated result and ends the run. Non-object schemas use a required `result` field; the tool is exempt from turn-budget blocking |

---

## Key Features

### Durable sessions & crash recovery

Every session is an append-only JSONL entry tree protected by an OS file lock with automatic crash recovery, live snapshot attachment, branch navigation, and forking. See [Session Storage](crates/axo/docs/sessions-and-compaction.md#session-storage-jsonlstore--sessionstore) and [Crash Recovery](crates/axo/docs/sessions-and-compaction.md#crash-recovery-cratesaxosrcrecoveryrs).

### Child sessions

Hosts spawn asynchronous children with `Session::spawn` and teams with `Session::spawn_team`; models use `spawn` and schema-validated `yield`. Each child has its own journal, persisted resolved spec, and one `agent_N` parent-registry row across all its runs, sharing concurrent run slots and team-boundary mail. See [Spawning child sessions](crates/axo/docs/extending.md#spawning-child-sessions) and [Child sessions](crates/axo/docs/sessions-and-compaction.md#child-sessions).

### Commands & prompt parts

`axo` owns the command catalog (`/compact`, `/shake`, `/model`, `/thinking`, `/navigate`) and the prompt-part input shape; hosts expose commands via `session.commands()` and run them with `session.run_command`. See [Commands](crates/axo/docs/extending.md#commands) and [Prompt parts](crates/axo/docs/prompts-and-context.md#prompt-parts-cratesaxosrcinputrs).

### Supervised commands & background jobs

Every local command runs in a host-selected container (`Supervision::ProcessGroup` or `Supervision::Cgroup`); long commands continue as `bg_N` background jobs delivered exactly once. See [Command supervision](crates/axo/docs/configuration.md#command-supervision-axosupervision) and [Background Job Durability](crates/axo/docs/sessions-and-compaction.md#background-job-durability).

### Cache-aware compaction & `/shake`

`axo` manages context pressure through append-only context edits, manual `/shake` elision, and one-action-per-pass automatic clearing or prompt-cached summarization. See [Automatic Compaction](crates/axo/docs/sessions-and-compaction.md#automatic-compaction-cratesaxosrccompaction).

### Sectioned system prompt, context files & skills

The system prompt is assembled into named `<section>` blocks (`delegation` for children, `system`, `docs`, `tools`, `schemes`, `work`, `prose`, `project_context`, `skills`, `team` for team members, `cwd`, `dev_shell`, `date`, plus host sections). Startup and tool execution discover instructions, skills, and personas across user and project roots. See [System prompt sections](crates/axo/docs/prompts-and-context.md#system-prompt-sections), [Context files](crates/axo/docs/prompts-and-context.md#context-files-cratesaxosrccontextfilesrs), and [Skills](crates/axo/docs/prompts-and-context.md#skills-cratesaxosrccontextskillsrs).

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
