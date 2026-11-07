# axo

**An embeddable, headless coding-agent harness core in Rust.**

`axo` is built to be linked directly into Rust hosts such as [Axome](https://github.com/tophc7/Axome), running many durable agent sessions in a single process (~92 MiB PSS live) without a sidecar runtime or terminal UI.

It ports [pi](https://github.com/earendil-works/pi) v1.3.0's core architecture—a sectioned system prompt stored in the transcript, delta-only streaming events, a tool-calling agent loop, and an append-only JSONL session tree—while adding crash recovery, recoverable `/shake` and cache-preserving compaction, cached Nix flake dev shells, and granular project access controls.

---

## Crates & Capabilities

The workspace is split into three layered crates so hosts only recompile what changes and non-coding agents can use the loop and provider layers independently:

| Crate | Purpose | Highlights |
| :--- | :--- | :--- |
| **[`axo-ai`](crates/axo-ai)** | LLM message model, streaming adapters, model catalog, and auth | Anthropic Messages, OpenAI Chat Completions, and OpenAI Codex Responses (`auth.json` OAuth + refresh locking); `models.dev` catalog with price-tier caps (lifted by `ModelsOptions::extended_context`); delta `Accumulator`; transient retry and context-overflow classification |
| **[`axo-agent`](crates/axo-agent)** | Generic tool-calling agent loop | Parallel and sequential `Tool` execution with a per-call `ToolContext` (cancellation, updates, pending-steer signal, invocation id), `Replay` crash-safety contracts (`Safe` vs `Never`), `LoopHooks`, steering and follow-up queues of identified `QueuedMessage`s, `MessageOrigin` provenance on persisted messages, `CancelReason` (`User` vs `Shutdown`), and split event delivery (ordered `Persist` subscriber + non-blocking broadcast) |
| **[`axo`](crates/axo)** | Sessions, compaction, context assembly, config, and built-in tools | Durable `JsonlStore` with incremental `Projection` and `context_edit`s, single-writer OS locks, crash recovery, `attach` catch-up snapshots, recoverable `/shake` and live/separate compaction summaries, `DevShellEnv` Nix/direnv capture, host-selected command supervision (`Supervision`: process groups by default, opt-in cgroup v2 through an embedded supervisor entry point), per-session URL schemes (`artifact://`, `skill://`, `local://`, `axo://`, `proc://`, host `Scheme`s), background jobs with `proc://` and `wait`, and the built-in tool set |

### Built-in tools (`crates/axo/src/tools`)

| Tool | Replay | Description |
| :--- | :--- | :--- |
| `read` | `Safe` | Reads UTF-8 text files (with `offset`/`limit` pagination) and images (`png`, `jpeg`, `gif`, `webp`, `bmp`), recording read hashes in the session's file ledger and loading nested directory `AGENTS.md` files on demand |
| `write` | `Safe` (plain paths / `local://`) / `Never` (action schemes) | Writes new files or complete rewrites, guarded by the read ledger when overwriting existing files |
| `edit` | `Never` | Performs exact or whitespace/quote-fuzzy multi-hunk replacements against files recorded in the read ledger, rejecting stale external modifications |
| `bash` | `Never` | Executes commands under the session's `ExecEnv` (host, `direnv`, or cached Nix flake dev shell) in a dedicated container (its own process group, or a cgroup v2 job when the host selects `Supervision::Cgroup`), with head+tail inline truncation and full output spooled under the session directory; long commands (60 s, a user steer, or `async: true`) continue as background jobs `bg_N` |
| `wait` | `Safe` | Blocks until a background job's result is available (or a user message arrives) and returns it; returns immediately when no job is running |
| `grep` | `Safe` | In-process regex search built on `ignore` and `grep-searcher` |
| `find` | `Safe` | In-process gitignore-aware file discovery built on `ignore` and `globset` |
| `ask` | `Safe` | Emits `SessionEvent::QuestionPending` with structured single- or multi-select questions and waits for any attached client to answer via `Session::answer` (registered when `SessionOptions::ask_enabled` is `true`) |

---

## Key Features

### Durable sessions & crash recovery

Every session is an append-only JSONL entry tree (with the bundled `JsonlStore`: `<root>/--<encoded-cwd>--/<timestamp>_<uuidv7>.jsonl` under the root the host chooses) protected by an OS file lock (`File::try_lock`) so a second writer gets `Error::SessionBusy` and a crashed process releases the lock automatically.

- **Durable intent:** Submissions (`prompt`, `steer`, `follow_up`, with optional `request_id` deduplication) store their prompt parts, so apps can redraw file and paste chips after reload; `run_start` / `run_end` markers and finished assistant messages are persisted before tool calls execute.
- **Reopen recovery:** When a session with an unfinished run reopens under `RecoveryPolicy::Resume`, `Replay::Safe` tools (`read`, `grep`, `find`, `ask`, `wait`, and `write` to plain paths or `local://`) re-run automatically and `Replay::Never` tools (`bash`, `edit`, and `write` to action schemes) receive an interrupted result pointing to any spooled output. Background jobs that were still running are marked interrupted once, with their partial-output reference. Five consecutive resumes without progress end the run as `Interrupted` with reason `restart_limit`.
- **Live attach:** `session.attach(from_seq)` returns missed durable entries since `from_seq`, a `LiveSnapshot` of any run or maintenance in progress (partial reply, running tools, pending `ask` questions, queued submissions) plus the session's background jobs, and a live broadcast receiver.
- **Branching & forking:** `session.navigate(target, options)` moves the persisted `leaf` pointer inside the tree (optionally summarizing the abandoned branch into a `branch_summary` entry); `session.fork(entry_id)` clones the path into a new session file and returns the forked user message's parts as `Fork::editable` for the client to edit and resubmit.

### Commands & prompt parts

`axo` owns the user command catalog and the shape of a user message; hosts decide which commands to expose and how:

- **Built-in commands:** `/compact [focus…]`, `/shake <mode>`, `/model <provider/id>`, `/thinking <level>`, and `/navigate <entry> [true|false] [focus…]`. A focus is free text telling the summary what to prioritize; it is recorded on the compaction or branch-summary entry. `session.commands()` lists them (each with a JSON Schema for its arguments and a mid-run `CommandPolicy`) followed by the session's skills, for hosts to expose however they like: a slash menu, buttons, their own pickers, or not at all. A built-in runs only through `session.run_command(command)`, given a `Command` from JSON arguments (`Command::decode`), typed text (`Command::parse("/shake images")`), or a value; `prompt`, `steer`, and `follow_up` never run one. A command is never saved as a submission or sent as a user prompt. `/model` and `/thinking` run alongside a turn or maintenance (compaction, shake, branch summary); `/compact`, `/shake`, and `/navigate` return `Error::Busy` during either.
- **Prompt parts:** `prompt`, `steer`, and `follow_up` take `impl Into<PromptInput>`: a plain string, or a list of `PromptPart`s (typed text, pastes, `@` file references that give the model only the path, and handed-over content that axo saves under the session's `local://` folder; saved images also reach the model as pixels).

### Supervised commands & background jobs

Every local command (`bash` and dev-shell capture) runs in its own container, chosen by the embedding host through `AxoBuilder::supervision(Supervision)`:

- **`Supervision::ProcessGroup` (default):** the command leads its own process group; stopping or dropping it signals the group, and a best-effort `sh` guard kills the group if the host dies. It needs no extra executable and no cgroup delegation. Descendants that leave the group (`setsid`, double fork) are not tracked.
- **`Supervision::Cgroup { program, args, cgroup_root }`:** the command runs in its own cgroup v2 job directory, entered before any user code runs and watched by a guardian process. It provides bounded lifecycle containment for detached descendants. The host supplies the supervisor by routing one of its own command lines in `main` to `axo::exec::supervisor::run(args)` and runs inside a delegated cgroup v2 (systemd: `Delegate=yes`); see [Command supervision](crates/axo/docs/configuration.md#command-supervision-axosupervision).

- **Retained helpers:** A command's result returns when its root shell exits (plus a 100 ms drain); processes it left running in its container stay supervised as `bg_N`, keep spooling output, and can be inspected or killed.
- **Promotion:** `bash` moves to the background after 60 s, when the user steers, or immediately with `async: true`, returning `bg_N` and its `artifact://` output. The optional `timeout` keeps no default and is never reset.
- **Exactly-once results:** A finished job's result reaches the model once, through `wait` or an automatic `job_result` message injected at the next turn boundary (an idle session starts a run for it). Promotion, results and delivery receipts are journaled, so results survive crashes and reopen; reopening never starts a run by itself.
- **`proc://`:** `read proc://` lists jobs, `read proc://bg_N` shows one, and `write proc://bg_N/kill` stops it.
- **Stop:** `session.abort()` stops the foreground turn only; background jobs keep running, and automatic delivery waits for the next user message.

### Cache-aware compaction & `/shake`

`axo` manages context pressure through append-only `context_edit` and `compaction` entries without mutating earlier history in place:

- **Manual `/shake` (`session.shake(mode)` or the `/shake <mode>` command):** Shrinks context without a model call. `ShakeMode::Elide` replaces older tool results and large fenced/XML blocks outside the newest ~4k tokens with `[shaken ~N tokens; full text: artifact://<n>]` pointing to recoverable files in the session's `artifacts/` directory; `Images` and `Thinking` strip older media or reasoning blocks. Undo is an ordinary branch navigation to the entry before the shake.
- **One action per automatic pass:** When context tokens cross `W - reserve`, `axo` chooses either **recoverable clearing** (eliding tool results older than the newest 40k tokens of tool output when that frees at least 20k tokens below the trigger) or **summarization**, never both in the same pass.
- **Live then separate summaries:** By default (`compaction.summaryRequest: "live"`), `axo` requests the checkpoint summary inside the live conversation with `ToolChoice::None` so the entire history hits the prompt cache, falling back to a standalone tool-free summarizer request (`"separate"`) if the response is rejected or triggered by context overflow. Recent user messages (within `keepRecentTokens`), tracked read/modified file lists, and the session file path carry over automatically.

### Sectioned system prompt, context files & skills

The system prompt lives in the transcript as named `<section>` blocks (`system`, `docs`, `tools`, `schemes`, `work`, `prose`, `project_context`, `skills`, `cwd`, `dev_shell`, `date`, plus host sections). Mid-session updates emit a diff `SystemMessage` containing only changed sections.

- **Replaceable prompts:** `SYSTEM`, `WORK`, `PROSE`, and the five summarizer prompts can be overridden by the host's user layer or, at `read` and `trust` access with `project.files.config` on (the default), the project layer it supplies (the project wins per name). Overrides are sampled once when a session is created or opened. An empty `SYSTEM`, `WORK`, or `PROSE` override disables that section.
- **Ordered `contextFiles` & nested loading:** Startup loads `~/.agents/AGENTS.md` (only when `homeAgentsDir: true`), then the host's `UserConfig::context_files` in order, then walks from the git root to `cwd` loading enabled names from `settings.contextFiles` (`AGENTS.md`, `AGENTS.local.md`, `CLAUDE.md`, `CLAUDE.local.md`, `.agents/AGENTS.md`). Touching a file in a subdirectory with `read`, `write`, or `edit` attaches any unseen nested context files onto that tool result (when `settings.nestedContextFiles` is `true` and project access allows it) without invalidating the system prompt cache.
- **Skills (`SKILL.md`):** Discovered from project `.agents/skills/`, the host's `UserConfig::skill_roots`, and `~/.agents/skills/` (when `homeAgentsDir: true`). Listed in `<available_skills>` for on-demand model reads, or expanded inline when the user types `/name args` (through `prompt`, `steer`, or `follow_up`). Skills appear in `session.commands()` with their optional `argument-hint` frontmatter; a skill named like a built-in command gets a warning diagnostic and is left out of the catalog.

### Nix flake & direnv dev shells (`DevShellEnv`)

When `bash.devShell` is set to `"nix"` or `"direnv"` in a trusted project, `axo` captures the project's environment once (via `nix print-dev-env` or an approved `.envrc`), caches Nix captures under `<cache_dir>/devshell/` (the directory the host passes to `AxoBuilder::cache_dir`) with a GC root, and runs every subsequent `bash` command directly against the captured environment (~3 ms overhead per command instead of ~460 ms for `nix develop -c`). Captures invalidate automatically when `flake.nix`, `flake.lock`, tracked `.nix` files, or `bash.devShellWatch` globs change. Nix mode needs a cache directory: without one, axo logs a warning and uses the host environment. Direnv mode needs none.

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
- `"read"` (default) loads markdown context files, skills, prompt overrides, and safe settings, while dropping code-running project settings (`bash.devShell` and `!command` `defaultModel`). Dev shells remain disabled even if enabled in user settings.
- `"trust"` permits all project settings, including `!command` `defaultModel`, and is required for dev shells regardless of which layer enables `bash.devShell`.

---

## Embedding in Rust

axo owns no configuration directory and reads no configuration files of its own. The host supplies models and a session store, and optionally a `ConfigSource` (user and project settings layers, prompt overrides, skill roots, context files) and a cache directory. `Axo::builder(models, store)...build()` does no configuration I/O; axo samples the source on each `create_session`, `open_session`, and `Axo::schemes` call, and calls `ConfigSource::project` only when the session's project access is `read` or `trust`.

```rust,no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use std::path::{Path, PathBuf};
use std::sync::Arc;
use axo::store::JsonlStore;
use axo::{Axo, ConfigLayer, ConfigSource, PromptPart, SessionOptions, UserConfig};
use axo_ai::{FileCredentialStore, Models, ModelsConfig, ModelsOptions};

/// The host decides where configuration comes from; axo validates, merges and trust-filters it.
struct HostConfig;

impl ConfigSource for HostConfig {
    fn user(&self) -> Result<UserConfig, axo::Error> {
        Ok(UserConfig {
            layer: ConfigLayer {
                origin: "host defaults".into(),
                settings: Some(serde_json::json!({"project": {"paths": {"/repo/**": "trust"}}})),
                ..ConfigLayer::default()
            },
            skill_roots: vec![PathBuf::from("/var/lib/my-host/skills")],
            ..UserConfig::default()
        })
    }

    fn project(&self, _cwd: &Path) -> Result<ConfigLayer, axo::Error> {
        Ok(ConfigLayer::default())
    }
}

let models = Models::load(ModelsOptions {
    cache_dir: Some(PathBuf::from("/var/lib/my-host/cache")),
    extended_context: false,
    config: ModelsConfig::default(),
    credentials: Arc::new(FileCredentialStore::new(PathBuf::from("/var/lib/my-host/auth.json"))),
})
.await?;

// Commands run in process groups by default; no supervisor executable or cgroup needed.
let axo = Axo::builder(
    Arc::new(models),
    Arc::new(JsonlStore::new(PathBuf::from("/var/lib/my-host/sessions"))),
)
.config(Arc::new(HostConfig))
.cache_dir(PathBuf::from("/var/lib/my-host/cache"))
.build();

let session = axo
    .create_session(SessionOptions {
        cwd: PathBuf::from("/repo/rust/axo"),
        model: Some("anthropic/claude-sonnet-4-5".into()),
        ask_enabled: true,
        // recovery defaults to RecoveryPolicy::Resume
        ..SessionOptions::default()
    })
    .await?;

let mut events = session.attach(None).await.events;
let outcome = session
    .prompt("Run the test suite and summarize any failures.")
    .await?;

// Structured parts: a file reference plus handed-over image bytes saved under `local://`.
let outcome = session
    .prompt(vec![
        PromptPart::Text { text: "Compare ".into() },
        PromptPart::File { path: "docs/design.png".into() },
        PromptPart::Text { text: " with this screenshot: ".into() },
        PromptPart::Content { name: "shot.png".into(), bytes: std::fs::read("/tmp/shot.png")? },
    ])
    .await?;
# Ok(())
# }
```

Without `.config(..)`, sessions run on default settings with no project layer. Without `.cache_dir(..)`, the `docs` prompt section and the `axo://` scheme are absent, and Nix dev shells fall back to the host environment with a warning.

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
│       ├── prompts/                # Bundled SYSTEM.md, PROSE.md, and summarizer markdown prompts
│       ├── examples/               # `drive` (CLI session driver) and `memory` (PSS benchmark); their own config layout under `--dir`
│       └── docs/                   # Bundled implementation reference for the `<docs>` system-prompt section
│           ├── README.md           # Documentation index
│           ├── configuration.md    # ConfigSource and EffectiveConfig, settings, models, project access, dev shells, supervision
│           ├── prompts-and-context.md     # System prompt sections, prompt overrides, contextFiles, skills, prompt parts
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
cargo run -p axo --example drive -- --dir <dir> --resume <dir>/sessions/.../session.jsonl "/model openai-codex/gpt-6-luna"
cargo run -p axo --example drive -- --dir <dir> --resume <dir>/sessions/.../session.jsonl "/shake elide"
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
