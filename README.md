# axo

**An embeddable, headless coding-agent harness core in Rust.**

`axo` is built to be linked directly into Rust hosts such as [Sworm](https://github.com/tophc7/Sworm), running many durable agent sessions in a single process (~92 MiB PSS live) without a sidecar runtime or terminal UI.

It ports [pi](https://github.com/earendil-works/pi) v1.3.0's core architecture—a sectioned system prompt stored in the transcript, delta-only streaming events, a tool-calling agent loop, and an append-only JSONL session tree—while adding crash recovery, recoverable `/shake` and cache-preserving compaction, cached Nix flake dev shells, and granular project access controls.

---

## Crates & Capabilities

The workspace is split into three layered crates so hosts only recompile what changes and non-coding agents can use the loop and provider layers independently:

| Crate | Purpose | Highlights |
| :--- | :--- | :--- |
| **[`axo-ai`](crates/axo-ai)** | LLM message model, streaming adapters, model catalog, and auth | Anthropic Messages, OpenAI Chat Completions, OpenAI Responses, and OpenAI Codex Responses (`auth.json` OAuth + refresh locking); `models.dev` catalog with `extendedContext` price-tier caps; delta `Accumulator`; transient retry and context-overflow classification |
| **[`axo-agent`](crates/axo-agent)** | Generic tool-calling agent loop | Parallel and sequential `Tool` execution, `Replay` crash-safety contracts (`Safe` vs `Never`), `LoopHooks`, steering and follow-up queues, `CancelReason` (`User` vs `Shutdown`), and split event delivery (ordered `Persist` subscriber + non-blocking broadcast) |
| **[`axo`](crates/axo)** | Sessions, compaction, context assembly, config, and built-in tools | Durable `JsonlStore` with incremental `Projection` and `context_edit`s, single-writer OS locks, crash recovery, `attach` catch-up snapshots, recoverable `/shake` and live/separate compaction summaries, `DevShellEnv` Nix/direnv capture, and the built-in tool set |

### Built-in tools (`crates/axo/src/tools`)

| Tool | Replay | Description |
| :--- | :--- | :--- |
| `read` | `Safe` | Reads UTF-8 text files (with `offset`/`limit` pagination) and images (`png`, `jpeg`, `gif`, `webp`, `bmp`), recording read hashes in the session's file ledger and loading nested directory `AGENTS.md` files on demand |
| `write` | `Never` | Writes new files or complete rewrites, guarded by the read ledger when overwriting existing files |
| `edit` | `Never` | Performs exact or whitespace/quote-fuzzy multi-hunk replacements against files recorded in the read ledger, rejecting stale external modifications |
| `bash` | `Never` | Executes commands under the session's `ExecEnv` (host, `direnv`, or cached Nix flake dev shell) in an isolated process group with a pipe-backed crash guard, head+tail inline truncation, and full output spooled under the session directory |
| `grep` | `Safe` | In-process regex search built on `ignore` and `grep-searcher` |
| `find` | `Safe` | In-process gitignore-aware file discovery built on `ignore` and `globset` |
| `ask` | `Safe` | Emits `SessionEvent::QuestionPending` with structured single- or multi-select questions and waits for any attached client to answer via `Session::answer` (registered when `SessionOptions::ask_enabled` is `true`) |

---

## Key Features

### Durable sessions & crash recovery

Every session is an append-only JSONL entry tree (`~/.axo/sessions/--<encoded-cwd>--/<timestamp>_<uuidv7>.jsonl`) protected by an OS file lock (`File::try_lock`) so a second writer gets `Error::SessionBusy` and a crashed process releases the lock automatically.

- **Durable intent:** Submissions (`prompt`, `steer`, `follow_up`, with optional `request_id` deduplication), `run_start` / `run_end` markers, and finished assistant messages are persisted before tool calls execute.
- **Reopen recovery:** When a session with an unfinished run reopens under `RecoveryPolicy::Resume`, `Replay::Safe` tools (`read`, `grep`, `find`, `ask`) re-run automatically and `Replay::Never` tools (`bash`, `write`, `edit`) receive an interrupted result pointing to any spooled output. Five consecutive resumes without progress park the run as `Parked`.
- **Live attach:** `session.attach(from_seq)` returns missed durable entries since `from_seq`, a `LiveSnapshot` of any run or maintenance in progress (partial reply, running tools, pending `ask` questions, queued submissions), and a live broadcast receiver.
- **Branching & forking:** `session.navigate(target, options)` moves the persisted `leaf` pointer inside the tree (optionally summarizing the abandoned branch into a `branch_summary` entry); `session.fork(entry_id)` clones the path into a new session file.

### Cache-aware compaction & `/shake`

`axo` manages context pressure through append-only `context_edit` and `compaction` entries without mutating earlier history in place:

- **Manual `/shake` (`session.shake(mode)`):** Shrinks context without a model call. `ShakeMode::Elide` replaces older tool results and large fenced/XML blocks outside the newest ~4k tokens with `[shaken ~N tokens; full text: <path>]` pointing to recoverable files in the session's spool directory; `Images` and `Thinking` strip older media or reasoning blocks. Undo is an ordinary branch navigation to the entry before the shake.
- **One action per automatic pass:** When context tokens cross `W - reserve`, `axo` chooses either **recoverable clearing** (eliding tool results older than the newest 40k tokens of tool output when that frees at least 20k tokens below the trigger) or **summarization**, never both in the same pass.
- **Live then separate summaries:** By default (`compaction.summaryRequest: "live"`), `axo` requests the checkpoint summary inside the live conversation with `ToolChoice::None` so the entire history hits the prompt cache, falling back to a standalone tool-free summarizer request (`"separate"`) if the response is rejected or triggered by context overflow. Recent user messages (within `keepRecentTokens`), tracked read/modified file lists, and the session file path carry over automatically.

### Sectioned system prompt, context files & skills

The system prompt lives in the transcript as named `<section>` blocks (`system`, `docs`, `tools`, `work`, `prose`, `project_context`, `skills`, `cwd`, `dev_shell`, `date`, plus host sections). Mid-session updates emit a diff `SystemMessage` containing only changed sections.

- **Replaceable prompt files:** `SYSTEM.md`, `WORK.md`, `PROSE.md`, and the five summarizer prompts can be overridden in `~/.axo/prompts/` or `<cwd>/.axo/prompts/`. Writing an empty `SYSTEM.md`, `WORK.md`, or `PROSE.md` disables that section.
- **Ordered `contextFiles` & nested loading:** Startup loads `~/.axo/AGENTS.md` (and `~/.agents/AGENTS.md` when `homeAgentsDir: true`), then walks from the git root to `cwd` loading enabled names from `settings.contextFiles` (`AGENTS.md`, `AGENTS.local.md`, `CLAUDE.md`, `CLAUDE.local.md`, `.agents/AGENTS.md`, `.axo/AGENTS.md`). Touching a file in a subdirectory with `read`, `write`, or `edit` attaches any unseen nested context files onto that tool result without invalidating the system prompt cache.
- **Skills (`SKILL.md`):** Discovered from project `.agents/skills/` and user `~/.axo/skills/`. Listed in `<available_skills>` for on-demand model reads, or expanded inline when the user types `/name args`.

### Nix flake & direnv dev shells (`DevShellEnv`)

When `bash.devShell` is set to `"nix"` or `"direnv"`, `axo` captures the project's environment once (via `nix print-dev-env` or an approved `.envrc`), caches Nix captures under `~/.axo/cache/devshell/` with a GC root, and runs every subsequent `bash` command directly against the captured environment (~3 ms overhead per command instead of ~460 ms for `nix develop -c`). Captures invalidate automatically when `flake.nix`, `flake.lock`, tracked `.nix` files, or `bash.devShellWatch` globs change.

### Project access levels (`ignore` | `read` | `trust`)

Configured exclusively in the user's `~/.axo/settings.jsonc` (a project cannot elevate its own access):

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
- `"read"` (default) loads markdown context files, skills, prompt overrides, and safe settings, while dropping code-running project settings (`bash.devShell` and `!command` `defaultModel`).
- `"trust"` permits all project settings, including `bash.devShell` and `!command` `defaultModel`.

---

## Embedding in Rust

```rust
use std::path::PathBuf;
use axo::{Axo, RecoveryPolicy, SessionOptions};

let axo = Axo::builder().build().await?;

let session = axo
    .create_session(SessionOptions {
        cwd: PathBuf::from("/repo/rust/axo"),
        model: Some("anthropic/claude-sonnet-4-5".into()),
        ask_enabled: true,
        recovery: RecoveryPolicy::Resume,
        ..SessionOptions::default()
    })
    .await?;

let mut events = session.attach(None).await.events;
let outcome = session
    .prompt("Run the test suite and summarize any failures.")
    .await?;
```

---

## Layout

```
axo/
├── crates/
│   ├── axo-ai/                     # Message model, streaming adapters, model catalog, OAuth & credentials
│   ├── axo-agent/                  # Tool-calling loop, hooks, queues, cancellation, split event delivery
│   └── axo/                        # Axo & Session APIs, JsonlStore, compaction, context, DevShellEnv, tools
│       ├── prompts/                # Bundled SYSTEM.md, PROSE.md, and summarizer markdown prompts
│       └── examples/               # `drive` (CLI session driver) and `memory` (PSS benchmark)
├── docs/                           # Implementation reference (linked in the `<docs>` system-prompt section)
│   ├── README.md                   # Documentation index
│   ├── configuration.md            # ~/.axo/ and .axo/ layout, settings.jsonc, models.jsonc, project access
│   ├── prompts-and-context.md      # System prompt sections, prompt overrides, contextFiles, and skills
│   ├── sessions-and-compaction.md  # JsonlStore, projection, recovery, attach, /shake, and compaction
│   └── extending.md                # Declarative files and Rust embedding traits
└── plan/                           # Architecture design, pi port studies, and background research
    ├── design.md                   # High-level design and future extension plans
    ├── core.md                     # Core port specification and milestone plan
    ├── pi/                         # Source studies of pi v1.3.0
    └── research/                   # Background studies and memory/compaction measurements
```

---

## Development

Enter the Nix development shell for the Rust toolchain (`cargo`, `rustc`, `clippy`, `rustfmt`, `rust-analyzer`):

```sh
nix develop
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Drive a live or resumed session from the command line with the `drive` example:

```sh
cargo run -p axo --example drive -- --cwd . --model zebes/qwen3.8-27b "Inspect crates/axo/src/lib.rs"
cargo run -p axo --example drive -- --resume ~/.axo/sessions/.../session.jsonl "/shake elide"
```

Measure idle and mid-turn proportional set size (PSS) across N concurrent sessions in one process:

```sh
cargo run --release -p axo --example memory -- --sessions 3
```

---

## Documentation

- **Implementation Reference (`docs/`):** Start at [`docs/README.md`](docs/README.md) for [`configuration.md`](docs/configuration.md), [`prompts-and-context.md`](docs/prompts-and-context.md), [`sessions-and-compaction.md`](docs/sessions-and-compaction.md), and [`extending.md`](docs/extending.md).
- **Planning & Research (`plan/`):** See [`plan/design.md`](plan/design.md), [`plan/core.md`](plan/core.md), [`plan/pi/`](plan/pi/README.md), and [`plan/research/`](plan/research/README.md).

## License

[MIT](./LICENSE). `axo` ports ideas and prompt text from [pi](https://github.com/earendil-works/pi) and [oh-my-pi](https://github.com/can1357/oh-my-pi), both MIT-licensed; code and prompts adapted from them retain their copyright notices.
