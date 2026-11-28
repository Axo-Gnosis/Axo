# Repository Guidelines

## The Axo Family

Shared by the AGENTS.md of axo, Axo-Extras, Axome, and Axophyte; change all four together.

- **axo**: the embeddable agent-harness library. It owns harness architecture and the public API every host builds on, designed for any host, never one.
- **Axo-Extras**: optional, opinionated capabilities core should not impose. Host-agnostic, built only on axo's public API to axo's standard; each host adopts, ignores, or replaces each crate.
- **Axome**: Toph's desktop host. It proves axo's surface and exposes its gaps; its product choices never define axo or Extras.
- **Axophyte**: Toph's personal persona, workflows, and extensions. It inspires ideas but has no bearing on the engineering of the others.

Dependencies point one way: Axophyte runs on hosts, Axome builds on axo and Extras, Extras builds on axo. A lower layer never names, special-cases, or assumes a higher one.

Where work belongs:

1. **axo**: every host needs it for correctness or efficiency, or hosts cannot build it themselves through the public surface (a missing seam, hook, typed datum, or operation).
2. **Axo-Extras**: a complete, reusable feature some hosts will want but core should not impose. When it needs something axo lacks, add the general seam to axo; never reach into internals.
3. **Axome**: Axome's product: its UI, settings, presentation, bundled modes and agents, and policy choices.
4. **Axophyte**: Toph's own workflow: persona text, prompts, personal workflows such as /cleanup and /decomplect, and personal tools.

"Axome needs X" or "Axophyte does X" is evidence of a need, not the design. Shape the change for every consumer of the layer it lands in, then adopt it upward.

Axophyte ("Axo") may also be the persona you are running as. Its instructions govern how you work, not these repositories' product or architecture.

### What this means in axo

- Axome and Axo-Extras are consumers and evidence, not specifications. Restate a need they report as a generic host scenario before designing the API, and name them in docs only as examples.
- Heavy use by Axome or Extras does not earn a feature a place in core; it must pass rule 1 above. A capability that only some hosts want belongs in Extras, with axo supplying any seam it needs.
- Nothing in axo's code, prompts, or bundled docs refers to Axophyte or depends on its conventions.
- Extras pins axo-ai by git rev, which must match its host's axo rev. A breaking axo-ai change means bumping Extras and Axome together.

## Embedder-First Design

axo is an embeddable harness, not a fixed agent product. Build capabilities and enforce their correctness; leave product policy, workflows, presentation, and interpretation to the host. A feature should enable hosts to realize their own designs, not prescribe ours.

Practical rule: **If the model can see it, the host can read it as typed data. If the model can do it, the host can do it through a typed method. If the UI shows it live, the session file can rebuild it later.**

**Efficient by default.** Token and context efficiency is axo's job, not a host policy. Tool design and output, tool guidelines and prompts, prompt caching, clearing, and compaction must work well out of the box with zero host code; never ship axo assuming the host will make it efficient. Hosts get settings and hooks to tune, replace, or disable those defaults.

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
- **`crates/axo-agent`**: Stateful turn loop, direct and nested tool execution (`Tool`, `Calls`), deferred-tool routing (`route`), host tool-policy hooks (`ToolHooks`), loop-driver contract (`LoopHooks`), immutable event sink (`Persist`), queues, and cancellation.
- **`crates/axo`**: Coding-agent harness—`Axo` host, `Session`, JSONL tree store & projection, recovery, compaction, context/skills, root session modes, scoped `ToolPolicy`, deferred tools (`call`, `tool://`), `ExecEnv`, command supervision (`ProcessSupervisor`), background jobs, the command catalog, URL `schemes`, and built-in tools, including feature-gated `codemode` over the public `axo::script` sandbox.
- **`crates/axo-extensions`**: Optional extension bridge above `axo`—`Extensions` manager, static `extension.json` discovery, supervised Bun runners (`runner.ts`) and their wire protocol, provider layers, and composition over public session APIs. Nothing below may depend on it.
- **`crates/axo-macros`**: Procedural macros beside the runtime chain. `axo` and `axo-extensions` use parameterized `#[expose]` to generate JSON dispatch and type metadata from Rust method signatures, and bare `#[expose]` to validate withheld-only impls.

## Code Invariants

- **No unsafe code**: `unsafe_code = "deny"` is enforced across the workspace.
- **Mutex discipline**: Never hold a `std::sync::MutexGuard` across an `.await`. Use `tokio::sync::Mutex` when locking across async I/O.
- **Infallible provider streams**: `Streamer::stream` and `Api::stream` never return `Result`; emit setup, auth, or network failures as a terminal `AssistantEvent::Error` on the stream.
- **Tool errors vs failed output**: Return `Err(ToolError)` for a failed call with no useful output (bad arguments, I/O errors, guard violations, cancellation); both become model-visible error results. Return `Ok(ToolOutput { is_error: true, .. })` only when the failure carries output the model should see (e.g. a failing command).
- **Call-result adjacency**: Every assistant `ToolCall` must be immediately followed by its `ToolResultMessage`s in the transcript—never interleave user, system, or steering messages between them.
- **File mutation guards**: File-mutating host tools must use `SessionTools::write_file`, which checks the write scope, serializes per-path mutations, and refuses a `FileWrite::Replace` over an existing file whose current bytes this session has not read or written (`FileLedger` freshness). `FileWrite::Edit` needs no earlier read: the edit closure proves freshness by matching the current bytes. Put the returned file records in `details.files` using `axo::file_details`. See [Session tools](crates/axo/docs/extending.md#session-tools-and-supervised-subprocesses).
- **Subprocess containment**: Every local command runs through `ProcessSupervisor` (`SessionTools::supervisor`), placing it in its own container: a process group (`process_group(0)`, group kill) under the default `Supervision::ProcessGroup`, or a cgroup v2 job under `Supervision::Cgroup`, so descendants never leak pipes.
- **Forward-compatible storage**: Wire/config JSON field names use `camelCase`; enum values and tags retain their declared serde case. JSONL `Entry` tags use `snake_case` and must preserve `Entry::Unknown` fallback deserialization.
- **Extension surface:** Every plain `pub` method on a guarded type must carry `#[ext]` or `#[ext(skip = "reason")]`. The `#[expose]` macro rejects unmarked methods at compile time. Register each dispatch block; the surface gate below catches missing registrations. Generate the TypeScript bindings from Rust, never by hand.

## Development & Testing

Run inside `nix develop`:

```bash
cargo check --workspace --all-targets
cargo nextest run --workspace   # or `cargo test --workspace` (`nextest` skips doctests; pass `--no-pager` to `list`)
cargo test --doc --workspace
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
- **Reuse test harnesses**: Use `axo_ai::testing::ScriptedApi` (under `features = ["testing"]`) for mocked model turns and `crates/axo/tests/common/mod.rs` for session tests in `tempfile::tempdir()`. It provides session setup and journal assertions, cancellation helpers (Hang, `start_hanging`, `run_ends`), append-gating fixtures (GatedStore and WriteGate), and the in-memory `ConfigSource` fixture behind `Host::config`.
