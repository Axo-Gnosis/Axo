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
- **`crates/axo`**: Coding-agent harness—`Axo` host, `Session`, JSONL tree store & projection, recovery, compaction, context/skills, `ExecEnv`, URL `schemes`, and built-in tools.

## Code Invariants

- **No unsafe code**: `unsafe_code = "deny"` is enforced across the workspace.
- **Mutex discipline**: Never hold a `std::sync::MutexGuard` across an `.await`. Use `tokio::sync::Mutex` when locking across async I/O.
- **Infallible provider streams**: `Streamer::stream` and `Api::stream` never return `Result`; emit setup, auth, or network failures as a terminal `AssistantEvent::Error` on the stream.
- **Tool errors vs execution failures**: Return `Ok(ToolOutput { is_error: true, .. })` for domain/command failures the model can recover from; reserve `Err(ToolError)` for precondition or guard violations.
- **Call-result adjacency**: Every assistant `ToolCall` must be immediately followed by its `ToolResultMessage`s in the transcript—never interleave user, system, or steering messages between them.
- **File mutation guards**: File-mutating tools must check `SessionTools::ledger` (`FileLedger`) and serialize per-path writes via `tools::lock_file`.
- **Subprocess cleanup**: Child commands must run in isolated process groups (`process_group(0)`) with group termination so descendants never leak pipes.
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
- **Reuse test harnesses**: Use `axo_ai::testing::ScriptedApi` for mocked model turns and `crates/axo/tests/common/mod.rs` (`common::{axo, host, options, within}`) for session tests in `tempfile::tempdir()`.
