# axo core design

This is the design of axo's core: what it ports from pi, how that maps to Rust, and the few places it deliberately differs.

- **Source:** pi v1.3.0. Detailed behavior, with citations, lives in [`pi/`](pi/README.md). This document points there instead of repeating it.
- **Status:** design settled ([Decided](#decided)); implementation not started.

## Layers

axo keeps pi's three layers, plus a thin binary on top:

| axo crate | Ports | Owns |
|---|---|---|
| `axo-ai` | pi `packages/ai` | message model, streaming events, the provider `Api` trait, built-in adapters, model registry, auth, retry, overflow detection, cost |
| `axo-agent` | pi `packages/agent` | the agent loop, the tool trait, agent events, loop hooks, steering and follow-up queues, cancellation |
| `axo` | pi `coding-agent/src/core` | `Session` (what hosts use), session storage, compaction, context assembly, built-in tools, config |
| `axo-acp` | — | the `axo` binary: ACP over stdio, mapped onto `axo` |

Each crate depends only on the ones above it in the table. Sworm depends on `axo`.

### Why four crates

- **The dependency direction is enforced, not just intended.**
  - `axo-agent` cannot see session files, and `axo-ai` cannot see the loop.
  - In Rust only a crate boundary guarantees this; with modules, `pub(crate)` lets anything reach anything.
  - It keeps the loop testable with a scripted provider and no disk, and the provider layer testable with no loop.
- **Each lower layer is useful alone.**
  - `axo-ai` is "call any model, get a typed streamed reply": Sworm can use it for one-off calls such as titles or commit messages without opening a session.
  - `axo-agent` is a generic tool-calling loop, usable for an agent that isn't about code.
- **The binary stays out of the library.** Hosts that embed `axo` never compile the ACP code or its dependencies.
- **Builds stay incremental.** `axo-ai` carries the heavy HTTP stack and changes least once written, so edits to tools or sessions don't rebuild it.

More crates than this would be ceremony. Tools, context and config stay modules inside `axo`. The extension host gets a crate only when extensions are designed.

### Workspace layout

```
axo/
├── Cargo.toml              workspace: shared versions, lints, edition 2024
├── flake.nix               dev shell
├── crates/
│   ├── axo-ai/             types · event (AssistantEvent, Accumulator) · api (Api trait, sse, http,
│   │                       anthropic, completions, responses, codex) · transform · models · auth ·
│   │                       retry · overflow · cost · json (partial/repair) · testing (ScriptedApi)
│   ├── axo-agent/          agent (Agent, queues) · run (the loop) · tool (Tool, ToolOutput, Replay) ·
│   │                       hooks (LoopHooks) · event (AgentEvent, delivery) · cancel (CancelReason)
│   ├── axo/                Axo (builder, process-wide state) · session (Session, attach, recovery) ·
│   │                       store (entries, JSONL, lock) · project · compaction · context
│   │                       (prompt, context files, skills) · tools · config
│   └── axo-acp/            the `axo` binary: stdio JSON-RPC, ACP ↔ Session mapping
└── docs/
```

| Crate | Main dependencies | Rough size (Rust) |
|---|---|---|
| `axo-ai` | `tokio`, `reqwest` (rustls, streaming), `serde_json` (`preserve_order`), `futures`, `regex`, `thiserror`, `jiff` | 3–4k lines |
| `axo-agent` | `axo-ai`, `tokio-util` (cancellation), `jsonschema` (argument validation) | 1.5–2k lines |
| `axo` | `axo-agent`, `ignore` + `grep-searcher` (grep/find), `similar` (diffs), `image`, `jsonc-parser`, `uuid`, `rustix` (process groups) | 6–8k lines |
| `axo-acp` | `axo`; ACP types either from the `agent-client-protocol` crate or written by hand (decided at M6) | under 1k lines |

Sizes are estimates from the pi notes [INFERENCE]; they are for orientation, not targets.

## Data model (`axo-ai`)

axo ports pi's v1.3.0 message model, including its newest idea: **the system prompt and the tool set live in the transcript as system messages.**
- The first system message carries the prompt as named sections.
- Later system messages patch it: append content, replace or remove a section by name, add or remove tools.
- Replaying the transcript yields the exact prompt and tool set at any point. This is what makes resume, branching and compaction consistent, and it keeps the prompt prefix cache-stable.

```rust
#[serde(tag = "role", rename_all = "camelCase")]
enum Message { System(SystemMessage), User(UserMessage), Assistant(AssistantMessage), ToolResult(ToolResultMessage) }

#[serde(tag = "type", rename_all = "camelCase")]
enum AssistantBlock {
    Text { text: String, text_signature: Option<String> },
    Thinking { thinking: String, replay: Option<ReasoningReplay>, redacted: bool },
    ToolCall { id: String, name: String, arguments: Map<String, Value>, arguments_error: Option<String> },
}
enum UserBlock { Text { .. }, Image { data: String /* base64 */, mime_type: String } }
// Opaque data a provider needs back on the next request, tagged with the API that produced it.
enum ReasoningReplay { AnthropicSignature(String), AnthropicRedacted(String), OpenAiReasoningItem(Value), ReasoningField(String), ReasoningDetails(Value) }

struct Usage { input: u64, output: u64, cache_read: u64, cache_write: u64, reasoning: Option<u64>, total_tokens: u64, cost: Cost }
enum StopReason { Stop, Length, ToolUse, Error, Aborted }
enum ThinkingLevel { Off, Minimal, Low, Medium, High, XHigh, Max }
```

- **Assistant messages** also record the API, provider and model, the response id and model, the raw stop reason, the error message, the thinking level, a timestamp and a duration. Full field list: [`pi/ai.md` §2](pi/ai.md).
- **Opaque provider data is kept intact.** Examples: Anthropic thinking signatures, and the name of the field an OpenAI-compatible server used for reasoning. Replaying the data unchanged is what keeps multi-turn thinking and prompt caching working.
  - **A typed enum instead of a string.** pi stores all of these in one string field, `thinkingSignature`, and even reuses it for the reasoning *field name*. axo gives each kind its own variant (an idea taken from Rig), so data from one API can never be mistaken for another's. See [`research/message-models.md`](research/message-models.md).
- **`serde_json` with `preserve_order`.** Tool-call arguments must replay byte-for-byte as the model produced them. Otherwise the prompt-cache prefix breaks.
- **Message content is always stored as a `Vec`.** A plain string is still accepted when deserializing.

### Streaming events: one deliberate difference

pi attaches the whole partial message to every streaming event, which costs nothing in JavaScript because it is a shared reference. In Rust that would mean a copy per token. So in axo:
- Events carry `{content_index, delta}`.
- Only `Start` and the terminal `Done`/`Error` carry a full message.
- One `Accumulator` type rebuilds the partial message. The loop, hosts and the ACP adapter all share it.

The event order follows pi:
- `Start` is emitted once the HTTP request is accepted.
- A setup error may arrive with no `Start` before it.
- A stream never fails by returning `Err`. Failures end it with an `Error` event whose message has `StopReason::Error` or `StopReason::Aborted` and keeps any partial content.

## Providers (`axo-ai`)

```rust
trait Api: Send + Sync {
    fn stream(&self, model: &Model, transcript: &Transcript, opts: StreamOptions, cancel: CancellationToken)
        -> BoxStream<'static, AssistantEvent>;
}
```

- **Built-in adapters:**
  1. Anthropic Messages.
  2. OpenAI Chat Completions, which also covers llama.cpp, Qwen and other OpenAI-compatible servers.
  3. OpenAI Responses, in its Codex variant (ChatGPT's backend) first. Plain Responses for API keys follows, sharing the same code.

  Each is a port of pi's adapter minus the vendor branches (listed in [`pi/ai.md` §4](pi/ai.md)), over `reqwest` with hand-rolled SSE.
- **Logins.** API keys from the environment or `auth.json` for every provider. The one built-in login is **OpenAI Codex**, because a ChatGPT subscription is the normal way to use Codex models: a browser login with PKCE and a local callback, then token refresh. It is ported from pi's `auth/oauth/openai-codex.ts` (about 440 lines of TypeScript). Other subscription logins (Claude, Copilot, …) are left for later, possibly porting omp's implementations.
- **Model catalog.** Model facts (context window, output limit, reasoning, image input, prices) are fetched from [models.dev](https://models.dev) and cached under `~/.axo/cache/`. The cache refreshes in the background and is used as-is when offline. Like omp's catalog refresh, fetched rows supply only numbers and flags; which wire API and auth a provider uses lives in a small table in code. Local servers (llama.cpp and other OpenAI-compatible ones) are discovered through their `/v1/models` endpoint. `models.jsonc` adds or overrides models and providers and always wins. Unlike omp, axo ships no bundled baseline; that can be added if offline first runs ever matter.
- **Registry.** A `Models` registry maps provider ids to providers. A provider has a base URL, headers, auth, a model list and the `Api`s it uses. Model compatibility quirks are a flat struct of `Option`s, filled in by auto-detection and overridden by the model's own settings, as in pi.
- **Auth seam.** Every auth path resolves to a `ModelAuth { api_key, headers, base_url }`. That is the only thing an auth plugin hands to the core, which matches the OAuth split in [`design.md`](design.md#providers-and-oauth).
  - **Credentials.** A `CredentialStore` trait. OAuth refresh runs under a per-provider lock, and a cancelled request never abandons a refresh in progress.
  - **Anthropic subscription login is the exception.** pi also switches the system preamble, the tool-name casing and the beta flags when it sees a subscription token. axo will express this as a flag on `ModelAuth` once that login exists.
- **Cross-model replay.** `transform_messages` makes a transcript produced by one model replayable to another:
  - drops thinking from other models;
  - leaves out failed and aborted turns;
  - normalizes tool-call ids;
  - adds a "No result provided" result for any tool call left without one.

  This is a pure function and deserves a regression test.
- **Retry and overflow.**
  - HTTP retry covers only the request, up to the response headers.
  - Turn-level retry and context-overflow detection are pure functions over the final message.
  - Overflow is checked first, because it is handled by compaction, not by retrying.
  - pi's pattern lists are part of the contract, so they are copied as they are.

**Recommended: port pi-ai rather than build on genai.** axo's message model *is* pi's, so the adapters' real work is mapping that model to each wire format: signatures, reasoning fields, cache markers, usage. A library would add a second message model to convert through and would still lack those details. The estimate is roughly 2,900–3,850 lines of Rust for the type model plus both adapters ([`pi/ai.md` §6](pi/ai.md)).

## Agent loop (`axo-agent`)

This is a port of pi's loop; [`pi/loop.md` §3.1](pi/loop.md) has it step by step.

**A turn** is one assistant response plus the execution of its tool calls. **A run** is one call of the loop: it repeats turns until:
- the response has no tool calls, and
- no steering or follow-up message is queued.

**Tools:**

```rust
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> &Value;                  // JSON Schema, usually derived with schemars
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }
    fn replay(&self) -> Replay { Replay::Never }    // may it run again after a crash? See Durability
    fn prompt_snippet(&self) -> Option<&str> { None }
    fn prompt_guidelines(&self) -> &[&str] { &[] }
    async fn execute(&self, call: ToolCall, cx: &ToolContext, cancel: CancellationToken,
                     updates: &dyn Fn(ToolUpdate)) -> Result<ToolOutput, ToolError>;
}
struct ToolOutput { content: Vec<UserBlock>, details: Option<Value>, is_error: bool, terminate: bool }
```

- **Errors:**
  - An `Err` becomes an error result the model sees; the loop never fails because a tool failed.
  - A failing bash command returns `Ok` with `is_error: true`.
  - `details` is for hosts and logs only and never reaches the model.

**Hooks.** A `LoopHooks` trait, every method with a no-op default. The hooks are:
- `transform_context`, `convert_to_llm` (auth is resolved inside `Models::stream`, so there is no `api_key` hook);
- `prepare_request`, `prepare_next_turn`, `finish_turn`;
- `before_tool_call`, `after_tool_call`;
- `steering`, `follow_up`.

`Session` implements them; the extension system will chain onto the same seams later. When each one fires: [`pi/loop.md` §4](pi/loop.md).

**Events.** `AgentEvent` follows pi:
- `AgentStart`/`AgentEnd`, `TurnStart`/`TurnEnd`;
- `MessageStart`/`MessageUpdate`/`MessageEnd`;
- `ToolExecutionStart`/`ToolExecutionUpdate`/`ToolExecutionEnd`.

Event delivery has two paths:
- **Persistence** is one ordered subscriber that the loop awaits. The session uses it to save each finished message on `MessageEnd` before the next request goes out, and it may adjust the message before it is saved, as pi allows.
- **Everything else** (hosts, UIs, the ACP adapter) receives events through a bounded broadcast channel. A receiver that falls behind is told it lagged and catches up from a snapshot (see [Durability](#durability)), so a slow or remote client can never stall the agent.

**Invariants carried over from pi:**
- `finish_turn` runs before `TurnEnd`.
- **Parallel tool calls** (the default):
  - Preparation, including `before_tool_call`, runs sequentially in source order.
  - `ToolExecutionEnd` events come in completion order.
  - Tool-result messages come in the order the model issued the calls.
- **Running a tool sequentially.** Calls run one at a time if the loop is configured sequential, or if any tool in the batch asks for it.
- **Truncated output.** When a response stops at the output-token limit, every tool call in it fails without running.
- **Steering** is injected only after all of the current turn's tool calls finish. **Follow-ups** are injected only when the run would otherwise end.
- **Message placement.** Nothing is ever inserted between a tool call and its result. Custom messages arriving mid-run wait for the turn to end.
- **Failing hooks.** A failing `before_tool_call` blocks the call. Other hook failures are logged and ignored.
- **Failed and aborted turns.** Their assistant messages stay in the transcript but are excluded from requests.

**Cancellation.** Each run gets a `CancellationToken`, and tools get child tokens. Cancelling ends the provider stream with an aborted message, never an `Err`. A cancellation carries a reason:
- **`User`:** the user stopped the run. The aborted assistant message is kept, and every tool call left without a result gets an "aborted" result, so the saved transcript is complete on its own. pi instead patches such calls at request time.
- **`Shutdown`:** the host is stopping. Nothing is written as aborted; the run stays open and resumes when the session is next opened.

## Sessions (`axo`)

`Session` is what a host holds: pi's `AgentSession`, minus the TUI. It wraps an agent and adds:
- persistence;
- compaction;
- retry;
- model and thinking-level changes;
- prompt-template expansion;
- forking and tree navigation.

**The session tree is the source of truth.** Before every request the session rebuilds the context by projecting the tree. The agent's in-memory transcript is only a cache of that projection.

### Format

The format is append-only JSONL. Line 1 is a header. Every other line is an entry with an `id`, a `parent_id` and a timestamp, so one file holds a tree. axo ports pi's v3 entry set and numbers it as axo format version 1, with no compatibility with pi files. The entry types are:
- `message`, `model_change`, `thinking_level_change`, `usage`;
- `compaction`, `branch_summary`;
- `custom` (extension state, never in context), `custom_message` (in context);
- `context_edit` (patch or hide an earlier message);
- `label`, `session_info`;
- `leaf` (records a tree navigation, so the current position survives a restart);
- `run_start`, `run_end`, `run_resumed`, `submission` (see [Durability](#durability)).

Unknown entry types are preserved on load, not dropped. Fields and semantics of the pi entries: [`pi/session.md` §2–3](pi/session.md).

### Storage and projection

- **Storage.** A `SessionStore` trait with a JSONL default. Files are created only when the first user message arrives. Each entry is appended as one line, and a torn last line is repaired on load. Unlike pi, any whole-file rewrite goes through a temp file and a rename.
- **Projection.** A pure function `project(entries, leaf) -> Projection`. It applies the latest compaction on the path, then the `context_edit`s, and yields the messages, the model and the thinking level. It is cached per session and updated incrementally on append; pi rebuilds it on every request.
- **Location.** `~/.axo/sessions/--<encoded cwd>--/<timestamp>_<uuidv7>.jsonl`. A host can supply its own store instead.
- **Branches and sessions.** One session file holds a tree. A new session exists only when the user explicitly forks or clones.
  - **Navigating** moves the current position (the leaf) inside the same file, and appends a `leaf` entry so the move survives a restart. The next message becomes a new branch from that point. Optionally the abandoned branch is first summarized into a `branch_summary` entry. Branches in one file are alternatives you switch between; only one is active at a time.
  - **Forking** (`fork(entry_id)`) copies the root-to-entry path into a new session file, with a new id, which records its parent session and the entry it forked from. Forking from a user message puts the position just before that message and returns its text for editing, as pi's `/fork` does. Forking at the current position is pi's `/clone`.
  - **A fork is a separate session.** It is listed and resumed on its own, has its own lock and its own runs, and can run at the same time as the original. Use a fork for parallel work and a branch for trying an alternative.

### Compaction and retry

Both are ported as they are; the algorithm is in [`pi/session.md` §3.8–3.9](pi/session.md).

- **When it triggers:** when context tokens exceed `context_window − reserve_tokens`. The defaults are 16,384 reserved and 20,000 kept as the recent tail.
- **When it is checked:** before a prompt, between turns, and after a run.
- **Cut point:** never at a tool result. A cut in the middle of a turn summarizes the start of that turn separately.
- **Repeated compactions** update the previous summary instead of starting over. Read and modified files are tracked across summaries.
- **Overflow recovery:**
  1. Hide the failed attempt and its tool results with `context_edit`s.
  2. Compact.
  3. Retry once.
- **Auto-retry** of transient errors uses backoff (pi's defaults: 3 retries, 2 s base delay, a provider-requested delay is honored up to 60 s), and also hides each failed attempt from the projection.
- **The summarizer. Blocked: Toph has open questions.** Still to decide: the guidelines the summary prompts give the model, and a cheaper model for summaries (wanted; how it is chosen is open). Until then, the starting point is pi's: the session's current model, and pi's prompts as bundled markdown files rather than strings in code.

### Durability

axo is built for an always-on host from the start: sworm-server keeps sessions running with no client attached, and a server restart must not lose a run. pi's coding agent has none of this. pi-durable does, but it is a separate experimental runtime of about 16k lines that pi itself doesn't use. axo takes four of its rules and builds them on the session file it already has. Full study: [`research/durable.md`](research/durable.md).

**The key observation.** The assistant message that requests tool calls is saved before those tools run, so the session file already records what was about to happen. What it lacks is a record that a run was still in progress.

- **Run markers.** `run_start` is appended before a run's first request, and `run_end` (done, aborted, failed or interrupted) when it settles. A session whose last `run_start` has no `run_end` has an unfinished run.
- **Recovery on open.** For an unfinished run:
  1. Each tool call without a result is handled by the tool's `replay` policy. `Safe` tools (`read`, `grep`, `find`, `write`, `ask`) run again. `Never` tools (`bash`, `edit`) get an error result saying they were interrupted and may have partly run, with the path to any output saved so far.
  2. Queued prompts that never ran are queued again.
  3. A `run_resumed` entry is appended, and the run continues.

  A run that keeps crashing (5 resumes in a row with no progress) is parked as interrupted instead of looping. The host chooses the policy: sworm-server resumes; the interactive ACP binary can mark the run interrupted and let the user decide.
- **Durable prompts.** Prompts, steers and follow-ups are saved as `submission` entries before they are queued, so a restart doesn't lose them. A submission may carry a `request_id`; submitting the same id twice returns the first submission. That stops a trigger that fires both before and after a restart from running twice.
- **One writer.** Opening a session for writing takes an OS file lock (`File::try_lock`), released automatically if the process dies. A second writer gets `SessionBusy`. Read-only opens don't lock.
- **Catching up.** Every entry has a sequence number, its position in the file. `session.attach(from)` returns the entries a client missed since `from`, a snapshot of the run in progress (the partial reply, running tools, any pending `ask` question, the queue), and a live event receiver. This is also what a lagging receiver uses to resync, and it maps directly onto ACP's `session/load`.
- **bash after a crash.** Commands get `PR_SET_PDEATHSIG` so they die with the server, and their full output is spooled under the session directory rather than `/tmp`, so an interrupted result can point to it.
- **Deferred:** saving partial replies while they stream (a crash simply re-sends the request), durable timers and schedules (those belong to Sworm), and locks shared across machines.

The estimated cost is 1.0–1.6k lines of Rust, spread over M2 (`replay`, cancel reasons, `continue_run`, the split event delivery) and M4 (the entries, recovery, the lock, the cursor and `attach`). Subagents later reuse the same rules: a child session is linked to the tool call that started it, so a resumed parent finds its child instead of starting a second one.

## Context (`axo`)

- **System prompt.** Built from named sections, most stable first so the prompt cache holds:
  1. `system`: from `SYSTEM.md`;
  2. `tools`: generated from the active tools, each tool carrying its own short guidance;
  3. `work`: from `WORK.md`;
  4. `prose`: from `PROSE.md`;
  5. `project_context`: the `AGENTS.md` files, the user's first, then the project's;
  6. `skills`: generated;
  7. `cwd`: the working directory, plus the dev-shell note when one is active;
  8. sections added by hosts and extensions.

  Other details:
  - Every section is wrapped in `<name>` tags.
  - When the prompt changes, only the changed sections are written to the transcript.
  - There is no date in it, which keeps the prefix cache-stable.
- **Prompt files.** axo's own guidance lives in bundled markdown files, never in code, so every opinion is visible and replaceable:

  | File | Holds | Default |
  |---|---|---|
  | `SYSTEM.md` | What the agent must know to work in axo: sessions, tool use, `ask`, interruptions. It stays neutral, with no persona or style, so a persona only ever adds. | axo's text. Replace it at your own risk |
  | `WORK.md` | How the work is done: quality bar, verification, scope | None; the section is empty unless the user writes one |
  | `PROSE.md` | How the agent talks: length, tone, formatting | A short bottom-line-up-front style: answer first, plain words, sized to the question |
  | `AGENTS.md` | Anything else: persona, soul, project rules | None (the standard context file, below) |

  - **Overriding.** A file in `~/.axo/` replaces the bundled default, and one in `<project>/.axo/` replaces both. An empty file switches that section off. Hosts and extensions can set the same sections in code.
  - **Not supported:** `APPEND_SYSTEM.md` (the user's `~/.axo/AGENTS.md` already adds text without editing anything), `SOUL.md`, `PERSONALITY.md`, `RULES.md`.
  - **Tool guidance belongs to the tool.** Removing or replacing a tool removes its guidance, so prompt files never mention specific tools.
- **Context files.** One file per directory, choosing the first match among `AGENTS.override.md`, `AGENTS.md`, `CLAUDE.md` and `.agents/AGENTS.md`. Global files come first, then directories from the git root (or cwd outside a repository) in towards cwd. Unlike pi, the walk stops at the git root.
- **Skills** (the one cross-tool standard, the Agent Skills format used by Codex, Cursor, OpenCode, Gemini CLI, Amp, pi and omp).
  - **Format:** a directory per skill with a `SKILL.md` holding `name` and `description` frontmatter. Parsing is lenient: a skill is dropped only when its description is missing.
  - **Discovery:** project `.agents/skills` from cwd up to the git root, then `~/.axo/skills`, then `~/.agents/skills`.
  - **What the model sees:** a list of name, description and location only. The model reads the body itself with `read`.
  - **Explicit use:** the user types `/name args`. The skill's body is inlined into the message, with `$ARGUMENTS`, `$@` and `$1`… filled in, and wrapped in tags so compaction can recognise and keep it. There is no `/skill:` prefix.
  - **Model use:** the model picks a skill when its description matches. A skill with `disable-model-invocation: true` (an Agent Skills field) is hidden from the model and only the user can start it.
  - **One namespace.** Skills and extension commands share `/name`. On a clash the extension command wins and axo reports a diagnostic at load.
  - **No commands folder.** Something the agent should be able to run is a skill. Something only the user runs, or that needs code (including commands that call other commands), is an extension command. axo reads no `commands/` or `prompts/` directories.
- **Not read from `.agents/`:** rules (they duplicate `AGENTS.md` and skills), hook files (axo has no hook concept; extensions do that job), MCP files (MCP servers are configured in `settings.jsonc`), `.agents/SYSTEM.md`, and other tools' directories such as `.claude/`. Survey: [`research/agents-dir.md`](research/agents-dir.md).
- **Hosts can switch project resources off.** `SessionOptions` has a `project_resources` switch, so Sworm can ignore a repository's `.agents/` until it trusts it.
- All `.agents/` kinds share one directory walk and load into plain structs that a host can also build directly. Loaders report problems as diagnostics instead of failing.

## Built-in tools (`axo`)

- **Common behavior:**
  - Output is capped at 2,000 lines or 50 KB. `read` keeps the head and says which offset continues; `bash` keeps the head and the tail (see below).
  - Paths resolve against the session cwd.
  - Notices and error strings follow pi's ([`pi/context.md` §3.7](pi/context.md)).
- **`read`.** As in pi: numbered text with an offset to continue from. Images are detected by magic bytes and resized to fit 2000×2000 and 4.5 MB. Unlike pi, files with NUL bytes are reported as binary instead of dumped.
- **`edit`.** pi's exact replace: `{path, edits: [{oldText, newText}]}`. Each `oldText` must match the original file exactly once, and the edits must not overlap. If an exact match fails, it falls back to fuzzy matching: trailing whitespace, smart quotes, dashes and Unicode spaces are normalized. It keeps BOMs and line endings, repairs common argument mistakes, and returns `details { diff, patch, first_changed_line }`. Claude was trained on this format, and tau's author removed hashline after his benchmark showed no gain ([`research/edit-formats.md`](research/edit-formats.md)).
- **Stale-file guard** (an addition to pi, for several agents sharing a repository):
  - **The record.** Each session records a content hash for every file it reads, writes or edits.
  - **The check.** `edit` and `write` on an existing file compare that hash with the file on disk. With no record, they refuse with "read the file first". If the hash differs, the file changed since this session last saw it (another agent, a `bash` command, or the user), and they refuse with "it changed; re-read it".
  - **Updating the record.** The session's own edits update its hash, so consecutive edits need no re-read.
  - **Locking across sessions.** One process-wide lock per file serializes each read-modify-write, so two sessions in the same process never interleave.

  This gives the staleness protection hashline offered without coupling it to the edit format.
- **`write`.** As in pi, under the same per-file lock.
- **`ask`.** Ported from omp. It asks the user one or more structured questions. Each question has an id and options with optional descriptions. The model may mark one option recommended and allow multi-select, and every question also accepts free text ("Other"). The run waits for the answer.
  - **No UI trait.** The tool emits a `QuestionPending` event with the questions and waits; any attached client answers with `session.answer(call_id, answers)`. That suits several clients and a server that runs with nobody attached: the question simply waits, and the host can notify you.
  - **Offered only when someone can answer.** The host says so in `SessionOptions` (the ACP binary only when its client can show choices).
  - **Durable.** The tool is `Replay::Safe`: after a restart the question is asked again, since nothing ran. A pending question is part of the `attach` snapshot.
  - **Validation and results as in omp.** Labels that collide with the reserved "Other" are rejected, and question ids must be unique. Answers are formatted as `id: choice` lines. An optional timeout picks the recommended option; it is off by default. Dismissing the question instead of answering aborts the run.
- **`bash`.** pi's tool, with five small additions from omp. Comparison: [`research/shell-tool.md`](research/shell-tool.md).
  - **It runs real bash, found on `PATH`, and is named `bash`.** NixOS has no `/bin/bash`, and inside a dev shell the project's own bash comes first. Models write bash; fish would break 14 of 30 common idioms (heredocs, `if/then/fi`, `[[ ]]`, …) and silently mis-handle 3 more. The login shell is never read or named in the prompt. Commands still get your environment, because it is inherited or captured (see the exec environment below).
  - **As in pi:** each command runs in its own process group, abort and timeout kill the whole group, output is read until it has been idle for 100 ms after exit, and there is no default timeout.
  - **Added from omp:** stopping sends SIGTERM, waits 1 s, then sends SIGKILL; non-interactive environment variables (pagers off, editors and password prompts off, `CI=true`); output keeps the head and the tail, with the full output spooled to a file; long lines are cut; terminal escape codes are stripped.
  - **Not adopted from omp:** its embedded bash clone and rewritten coreutils, a persistent shell between calls, the interceptor that blocks commands meant for other tools, and PTY mode.
  - **Later:** background jobs (a long command keeps running and its result arrives as a follow-up submission, with `wait`), and named services. To leave room, the exec backend returns a running handle owned by the session rather than a finished result, and running jobs are recorded in the session log so recovery can report them.
- **`grep` and `find`.** These run in-process on the ripgrep crates (`ignore`, `grep-searcher`) instead of shelling out to `rg`/`fd`. Output formats and limits follow pi's: 100 matches, 1,000 results, 500 characters per line.
- **Pluggable backends.** pi gives each tool a replaceable backend for file and process access; that becomes axo's resolver and exec seams. This is where `scheme://` URLs and remote execution plug in later.

### Exec environment and Nix dev shells

Every command `bash` runs gets its environment from an `ExecEnv` provider, chosen per session in `SessionOptions`. The built-in providers are:
- **`HostEnv`:** the environment axo was started with. This is the default.
- **`StaticEnv`:** an environment the host passes in. Sworm already captures dev shells per folder and can hand its result over.
- **`DevShellEnv`:** the project's Nix dev shell.
  - **Captured once, not per command.** It runs the flake's dev shell once (or uses an allowed direnv setup, as omp does), including its `shellHook`, and saves the resulting environment. Every later command runs plain `bash -c` with it.
  - **Cost.** About 3 ms extra per command. Wrapping each command in `nix develop -c` instead costs about 460 ms on Sworm's flake.
  - **When it recaptures.** When `flake.nix`, `flake.lock`, `.envrc` or a tracked `.nix` file changes. Captures are cached under `~/.axo/cache/devshell/`.
  - **What the model sees.** One fixed line in the system prompt saying commands already run inside the project's dev shell. Capture failures appear as notes in tool results.
  - **Turning it on.** Settings choose `off`, `direnv` or `auto` (also capture flakes). Capturing a flake runs project code, so it can only be enabled from the user's own settings, not a project's.

The provider is a trait, so other environments (devenv, containers, remote hosts) can be added later, including by extensions. Nix ships built in because it is how Toph works. Design and timings: [`research/nix-shell.md`](research/nix-shell.md).

### Later tools (not in the first set)

These are omp tools Toph wants or may want. None is built in M1–M6, but the core leaves room for each.

| Tool | What it does | Where it fits in axo |
|---|---|---|
| `task` (with subagents) | Starts one or more subagents, each with its own context and an optional role and output schema, often in parallel, and returns their results. | Built on child sessions plus the durability links (a resumed parent finds its children instead of starting new ones). Roles are agent definitions read from `.agents/agents` and `~/.axo/agents`: markdown with `name`, `description`, `tools`, `model`, `thinking` and `output` frontmatter, the shape Claude Code, Gemini, OpenCode, Cursor and omp have converged on. |
| `wait` | Blocks until background work (async subagents, background commands) delivers a result. | Comes with background jobs. Results also arrive on their own as follow-up submissions; `wait` is for when the model has nothing else to do. |
| `eval` | A persistent JavaScript or Python kernel. The model writes code that computes, transforms data, and calls other tools in loops, which saves many turns. | An extension, most naturally running in the Bun extension host, which is already a JS runtime. |
| `ast_edit` | Structural search-and-rewrite across many files using ast-grep patterns (for example `foo($A)` → `bar($A)`), previewed, then applied. | An optional extension or Cargo feature. It needs tree-sitter grammars, the heavy dependency the edit tool avoided. |
| `glob` | Finds files by name pattern. | Already in the first set, named `find` (decided). It may borrow omp's newest-first sorting. |
| `write` | Creates or overwrites a file. | Already in the first set. omp's version can also write into archives and internal URLs; axo gets that from the scheme registry later. |

## Config (`axo`)

- **Directories:**
  - **User:** `~/.axo/` (`AXO_DIR` overrides it). It holds:
    - `settings.jsonc` and `models.jsonc`;
    - `auth.json` (machine-written, mode 0600, locked);
    - `sessions/`;
    - `SYSTEM.md`, `WORK.md`, `PROSE.md` and `AGENTS.md`;
    - `skills/` (and `agents/` once `task` exists);
    - `cache/`, for dev-shell captures.
  - **Project:** `<cwd>/.axo/settings.jsonc`, plus `SYSTEM.md`, `WORK.md` and `PROSE.md` overrides. Shareable project resources (skills, agents) go in `.agents/`, so other tools can read them too.
  - **Format:** hand-edited config is JSONC. When axo writes a setting back, it edits the file in place so comments survive (for example with `jsonc-parser`'s CST).
- **Settings.** Global and project settings are deep-merged, and the project wins.
  - Config values accept `$ENV` and `!command`.
  - Keys and defaults follow pi, minus the TUI ones.
- **Files are optional for hosts.** Everything a file provides is a plain Rust struct a host can build directly, so Sworm can run axo without touching `~/.axo`.

## Library API (sketch)

```rust
let axo = Axo::builder().dir(axo_dir).build().await?;        // config, providers, credentials, resources
let session = axo.create_session(SessionOptions { cwd, model: Some("anthropic/<model>".into()), ..Default::default() }).await?;
let Attach { missed, live, mut events } = session.attach(None);   // history, in-progress snapshot, broadcast receiver
session.prompt("fix the failing test").await?;               // returns when the run settles
session.steer("use the existing helper instead");           // safe from other tasks while running
session.abort().await;
let forked = session.fork(entry_id).await?;
```

## ACP binary (`axo-acp`)

The binary maps ACP onto `Session`:

| ACP | `Session` |
|---|---|
| `session/new` and `session/load` | create and open (replay) |
| `session/prompt` | `prompt` |
| `session/cancel` | `abort` |
| `session/update` | streams `AgentEvent`s through the shared `Accumulator` |

Everything else waits until a client needs it.

## Not ported

- **Interface:** the TUI and themes, export to HTML, and user `!` shell commands.
- **Packaging and extras:** pi packages, telemetry, cache warming, codemode, image generation.
- **Models:** virtual models, the provider zoo beyond the adapters above, and pi's file migrations.
- **Project trust.** pi requires trusting a project before loading its settings, skills and `SYSTEM.md`. axo loads them without asking; a trust gate comes with extensions, since that is when untrusted code could run.

## Decided

- **Context-file walk:** stops at the git root.
- **Project trust:** none until extensions exist.
- **Config format:** JSONC.
- **Edit tool:** pi's exact replace with fuzzy fallback, plus the stale-file guard. No hashline, and no omp edit crate.
- **Durability:** built in from the start, as described in [Durability](#durability).
- **Default tools:** every tool in the first built-in set is on by default: `read`, `write`, `edit`, `bash`, `grep`, `find`, and `ask` whenever a client can answer.
- **Provider layer:** port pi-ai's adapters.
- **Current position:** persisted as `leaf` entries.

## Build order

Each milestone is real, kept code, with tests only for the invariants named above.

| # | Milestone | Done when |
|---|---|---|
| M1 | `axo-ai`: types, events, `Accumulator`, Anthropic, Chat Completions and Codex Responses adapters, API keys from env and `auth.json`, the Codex login, the fetched model catalog, a scripted test provider | Streams a multi-block reply with thinking and tool calls from local Qwen and from Claude Haiku. The Codex login and adapter are built in M1 but first tested live with Toph |
| M2 | `axo-agent`: loop, tool trait with `replay`, hooks, queues, cancellation with reasons, `continue_run`, split event delivery | The scripted provider passes the ordering invariants; a real model completes a multi-tool run |
| M3 | `axo` tools and context: `read`, `write`, `edit`, `bash`, `grep`, `find`, `ask`, the `ExecEnv` seam with `DevShellEnv` (flake capture), system prompt and prompt files, context files, skills with `/name` invocation, config | A session in one of Toph's flake repos, started outside `nix develop`, reads, edits and runs tests in the dev shell, and asks a question that a test client answers |
| M4 | `axo` sessions: store, projection, resume, navigation, fork, run markers, submissions, recovery, lock, `attach` | Kill the process mid-tool, reopen, and the run completes (with an interrupted result for bash); fork from a message; a client attaching mid-run catches up |
| M5 | Compaction, branch summaries, auto-retry, overflow recovery. **Blocked** until the summarizer questions above are settled | A long session compacts and keeps working |
| M6 | `axo-acp` binary | An ACP client (Zed, or Sworm's existing ACP view) chats with `axo` |

After M4, measure memory: idle and mid-turn, for one and for three sessions in one process. The target is to be clearly under three `pi --mode rpc` processes (160–209 MiB).
