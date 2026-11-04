# Repository Guidelines

## Living Documentation (Paramount)

Architecture, active plans, and runtime reference docs live in dedicated files—do not duplicate them here, and **never let them drift**:

- **`crates/axo/docs/`** (indexed at `crates/axo/docs/README.md`) & **`README.md`**: Authoritative reference for implemented behavior, configuration, tools, schemes, sessions, and compaction. **`crates/axo/docs/` is compiled into `axo` and exposed to agents at runtime via `axo://`—keeping it accurate and updated on every behavioral or config change is paramount.**
- **`plan/design.md`**: Goals, non-goals, crate architecture, and settled core decisions. Update when core decisions change or milestones complete.
- **`plan/plan.md`**: Active tracks, open design discussions, and unbuilt work. Update as tracks progress or settle.

## Crate Boundaries

Dependency flow is strictly downward (`axo` -> `axo-agent` -> `axo-ai`):

- **`crates/axo-ai`**: LLM wire layer—message/transcript model, streaming adapters, model catalog, auth, cross-model transforms, error/overflow classification.
- **`crates/axo-agent`**: Stateful turn loop, tool execution (`Tool`), lifecycle hooks (`LoopHooks`), persistence seam (`Persist`), queues, and cancellation.
- **`crates/axo`**: Coding-agent harness—`Axo` host, `Session`, JSONL tree store & projection, recovery, compaction, context/skills, `ExecEnv`, command supervision (`ProcessSupervisor`), background jobs, the command catalog, URL `schemes`, and built-in tools.

## Code Invariants

- **No unsafe code**: `unsafe_code = "deny"` is enforced across the workspace.
- **Mutex discipline**: Never hold a `std::sync::MutexGuard` across an `.await`. Use `tokio::sync::Mutex` when locking across async I/O.
- **Infallible provider streams**: `Streamer::stream` and `Api::stream` never return `Result`; emit setup, auth, or network failures as a terminal `AssistantEvent::Error` on the stream.
- **Tool errors vs failed output**: Return `Err(ToolError)` for a failed call with no useful output (bad arguments, I/O errors, guard violations, cancellation); both become model-visible error results. Return `Ok(ToolOutput { is_error: true, .. })` only when the failure carries output the model should see (e.g. a failing command).
- **Call-result adjacency**: Every assistant `ToolCall` must be immediately followed by its `ToolResultMessage`s in the transcript—never interleave user, system, or steering messages between them.
- **File mutation guards**: File-mutating tools must check `SessionTools::ledger` (`FileLedger`) and serialize per-path writes via `tools::lock_file`.
- **Subprocess containment**: Every local command runs through `ProcessSupervisor` (`SessionTools::supervisor`), placing it in its own container: a process group (`process_group(0)`, group kill) under the default `Supervision::ProcessGroup`, or a cgroup v2 job under `Supervision::Cgroup`, so descendants never leak pipes.
- **Forward-compatible storage**: Wire/config JSON uses `camelCase`; JSONL `Entry` tags use `snake_case` and must preserve `Entry::Unknown` fallback deserialization.

## Development & Testing

Run inside `nix develop`:

```bash
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

- **Offline deterministic tests**: All `cargo test` suites must run offline without API keys or network access. Use `crates/*/examples/` for live model smoke runs.
- **Doc & supervision gates**: `cargo test -p axo --test doc_identifiers` rejects backticked identifiers in README.md, AGENTS.md, and `crates/axo/docs/` that do not exist in source. Cgroup supervision tests run only with `$AXO_TEST_CGROUP=1` in a delegated cgroup.
- **Reuse test harnesses**: Use `axo_ai::testing::ScriptedApi` (under `features = ["testing"]`) for mocked model turns and `crates/axo/tests/common/mod.rs` (helpers `axo`, `host`, `options`, `bash_options`, `tool_text`, `dead`, `within`, and the in-memory `ConfigSource` fixture behind `Host::config`) for session tests in `tempfile::tempdir()`.
