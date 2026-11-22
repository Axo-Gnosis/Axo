# Repository Guidelines

## Embedder-First Design

Axo is an embeddable harness, not a fixed agent product. Build capabilities and enforce their correctness; leave product policy, workflows, presentation, and interpretation to the host. A feature should enable hosts to realize their own designs, not prescribe ours.

Practical rule: **If the model can see it, the host can read it as typed data. If the model can do it, the host can do it through a typed method. If the UI shows it live, the session file can rebuild it later.**

For every feature or architectural change, evaluate it from the host's side:

- **Usable public surface**: Can a host implement the intended behavior through supported APIs, without reaching into internals, duplicating harness logic, or forking axo? Expose the information and operations it actually needs; internal support alone is not a usable feature.
- **Deliberate extension points**: Provide hooks where hosts need to observe, decide, or customize behavior. Make their timing, inputs, outputs, ordering, and failure semantics explicit, with clear ownership between host and harness.
- **Lifecycle fit**: Trace the intended integration end to end, including changes during an active session. Do not make a capability startup-only or immutable by accident; lifecycle restrictions must follow real invariants, not implementation convenience.
- **Coherent, sufficient APIs**: Keep the public contract clear, consistent, and reasonably complete for supported use cases. Prefer composable capabilities over host-specific special cases, without adding speculative abstractions or hooks.

Validate the host-facing path, not just the internal mechanism. If the architecture prevents a legitimate intended integration, treat that as a design gap rather than leaving the host to work around it.

## Living Documentation (Paramount)

Architecture, active plans, and runtime reference docs live in dedicated files—do not duplicate them here, and **never let them drift**:

- **`crates/axo/docs/`** (indexed at `crates/axo/docs/README.md`) & **`README.md`**: Authoritative reference for implemented behavior, configuration, tools, schemes, sessions, and compaction. **`crates/axo/docs/` is compiled into `axo` and exposed to agents at runtime via `axo://`—keeping it accurate and updated on every behavioral or config change is paramount.**
- **`plan/design.md`**: Goals, non-goals, crate architecture, and settled core decisions. Update when core decisions change or milestones complete.
- **`plan/plan.md`**: Active tracks, open design discussions, and unbuilt work. Update as tracks progress or settle.

## Crate Boundaries

Dependency flow is strictly downward (`axo-extensions` -> `axo` -> `axo-agent` -> `axo-ai`):

- **`crates/axo-ai`**: LLM wire layer—message/transcript model, streaming adapters, model catalog, auth, cross-model transforms, error/overflow classification.
- **`crates/axo-agent`**: Stateful turn loop, tool execution (`Tool`), deferred-tool routing (`route`), host tool-policy hooks (`ToolHooks`), loop-driver contract (`LoopHooks`), immutable event sink (`Persist`), queues, and cancellation.
- **`crates/axo`**: Coding-agent harness—`Axo` host, `Session`, JSONL tree store & projection, recovery, compaction, context/skills, root session modes, scoped `ToolPolicy`, deferred tools (`call`, `tool://`), `ExecEnv`, command supervision (`ProcessSupervisor`), background jobs, the command catalog, URL `schemes`, and built-in tools.
- **`crates/axo-extensions`**: Optional extension bridge above `axo`—`Extensions` manager, static `extension.json` discovery, supervised Bun runners (`runner.ts`) and their wire protocol, provider layers, and composition over public session APIs. Nothing below may depend on it.
- **`crates/axo-macros`**: Procedural macros beside the runtime chain. `axo` uses `#[expose]` to generate JSON dispatch and type metadata from Rust method signatures.

## Code Invariants

- **No unsafe code**: `unsafe_code = "deny"` is enforced across the workspace.
- **Mutex discipline**: Never hold a `std::sync::MutexGuard` across an `.await`. Use `tokio::sync::Mutex` when locking across async I/O.
- **Infallible provider streams**: `Streamer::stream` and `Api::stream` never return `Result`; emit setup, auth, or network failures as a terminal `AssistantEvent::Error` on the stream.
- **Tool errors vs failed output**: Return `Err(ToolError)` for a failed call with no useful output (bad arguments, I/O errors, guard violations, cancellation); both become model-visible error results. Return `Ok(ToolOutput { is_error: true, .. })` only when the failure carries output the model should see (e.g. a failing command).
- **Call-result adjacency**: Every assistant `ToolCall` must be immediately followed by its `ToolResultMessage`s in the transcript—never interleave user, system, or steering messages between them.
- **File mutation guards**: File-mutating tools must check `SessionTools::ledger` (`FileLedger`) and serialize per-path writes via `tools::lock_file`.
- **Subprocess containment**: Every local command runs through `ProcessSupervisor` (`SessionTools::supervisor`), placing it in its own container: a process group (`process_group(0)`, group kill) under the default `Supervision::ProcessGroup`, or a cgroup v2 job under `Supervision::Cgroup`, so descendants never leak pipes.
- **Forward-compatible storage**: Wire/config JSON uses `camelCase`; JSONL `Entry` tags use `snake_case` and must preserve `Entry::Unknown` fallback deserialization.
- **Extension surface:** Every plain `pub` method on a guarded type must carry `#[ext]` or `#[ext(skip = "reason")]`. The `#[expose]` macro rejects unmarked methods at compile time. Register each exposed block; the surface gate below catches missing blocks. Generate the TypeScript bindings from Rust, never by hand.

## Development & Testing

Run inside `nix develop`:

```bash
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

- **Offline deterministic tests**: All `cargo test` suites must run offline without API keys or network access. Use `crates/*/examples/` for live model smoke runs.
- **Doc & supervision gates**: `cargo test -p axo --test doc_identifiers` rejects backticked identifiers in README.md, AGENTS.md, and `crates/axo/docs/` that do not exist in source. Cgroup supervision tests run only with `$AXO_TEST_CGROUP=1` in a delegated cgroup. Real-Bun acceptance (`cargo test -p axo-extensions --test bun_provider`) runs only with `$AXO_TEST_BUN` set to an absolute Bun path.
- **Extension gates:** `cargo test -p axo --test ext_surface` checks method decisions, registered blocks and a FREE decision for every reachable public free function. `cargo test -p axo-extensions --test bindings` checks the SDK bindings; `cargo test -p axo --test ext_reference` checks `crates/axo/docs/extension-api.md`. Regenerate both after a surface change:

  ```bash
  UPDATE_BINDINGS=1 cargo test -p axo-extensions --test bindings
  UPDATE_BINDINGS=1 cargo test -p axo --test ext_reference
  ```
- **Reuse test harnesses**: Use `axo_ai::testing::ScriptedApi` (under `features = ["testing"]`) for mocked model turns and `crates/axo/tests/common/mod.rs` (helpers `axo`, `axo_with`, `host`, `options`, `bash_options`, `tool_text`, `dead`, `within`, `until`, `entry`, `delivered`, `request`, `journal`, `only`, `transient`, `message_text`, and the in-memory `ConfigSource` fixture behind `Host::config`) for session tests in `tempfile::tempdir()`.
