# Extension API parity, plan B: brief for the planner

This is a brief, not a plan. It gives the agent that executed plan A everything it needs to write plan B without asking Toph anything that is already decided. Where a choice is a preference, the decision is recorded here. Where a choice depends on what plan A produced, this brief says so and says how to settle it from the code.

Delete this file once plan B has landed.

## 1. What plan B must deliver

Plan A built the mechanism (attribute macro, generated TypeScript, generic runner call, typed errors, subscriptions) and applied it to the 71 public `Session` methods. Plan B finishes parity for the rest of the public `axo` API:

1. **A `tools` object for tool handlers.** A TypeScript tool gets what a Rust tool gets: the session's file, path, scheme and command facilities with the same safety rules.
2. **Handler inputs widened to their Rust equivalents.** Policies, spawn hooks, tools and schemes receive the data their Rust counterparts receive.
3. **A `host` object and the lifecycle hook.** Listing and reading sessions directly; create, open, fork and close through a hook the host implements.
4. **A `models` object.** List models and providers, report where a credential comes from, estimate tokens, run a one-off completion.
5. **Every remaining exported type decided.** Each public method outside `Session` is exposed, adapted, or withheld with a reason. The guard's pending list is empty at the end.
6. **A generated reference** of what is exposed and what is withheld, shipped with the runtime docs.

Done means: `PENDING` in `crates/axo/tests/ext_surface.rs` is empty and the test covers every exported type; a TypeScript tool can edit a file with the same stale-write refusal as the built-in tool; an extension can fork a session through the host's hook; an extension can list models and get a completion.

## 2. Decisions already made (do not reopen, do not re-ask)

| Topic | Decision |
|---|---|
| Parity rule | TypeScript binds Axo's public Rust API. Everything is exposed unless withheld for ownership, authority or transport, with a written reason. |
| Trust | Extensions are trusted code. No sandbox, no per-call authority check, no origin tagging. |
| Mirror Rust | Each TypeScript handler receives what its Rust twin receives. Commands get the session. Tools get `tools`, not the session. A tool that wants the session calls `ctx.session(id)` on purpose. |
| Enforcement | Rules are enforced by the compiler or a test, never by convention. Agents write all the code and miss conventions. |
| Mechanism | `#[expose]` on `impl` blocks, `#[ext]` or `#[ext(skip = "reason")]` on every public method, explicit block lists (no link-time registration), specta-generated TypeScript with a staleness test. Reuse it; do not add a second mechanism. |
| Single implementation | Rust implements; TypeScript forwards. `runner.ts` gets no per-method code and no per-method validation. |
| Stability | None promised. Breaking the SDK and the wire is fine; Toph owns all consumers. |
| No workflow engine | No slash-text chaining, no declarative workflows. TypeScript is the workflow language. |
| Lifecycle | List sessions, read a closed journal, spawn children: exposed directly. Create, open, fork, close: only through a host-implemented adoption hook; refused with a clear error code when the host registered none. The host owns the resulting session, attaches extensions and updates its UI through its normal code path, so an extension-started fork looks to the UI exactly like a user-started one. |
| Lower crates | From `axo-ai`: model and provider listing, credential source, token estimates, one-off completion. Nothing separate from `axo-agent`. Types from both crates that appear in exposed signatures get wire shapes. |
| Secrets | No exposed method returns a credential secret, a resolved API key, or auth headers. |
| Host registrations | Extensions never replace the host's own tools, policies, hooks, schemes, commands or section list. They contribute through their manifest. |
| Opened up in plan A | Answering and dismissing pending questions, accepting and revising mode results, and navigation are callable by extensions. |
| Custom state | Extension state keys stay prefixed `<extension-id>:`. |
| Nested calls | Chain tracking refuses a registered command re-entering itself; a 30 s stall detector covers the rest. Extend the same two tools; do not design a third. |

### Withheld in plan B, with the reason to write in the marker

| What | Reason |
|---|---|
| `Axo::builder`, all of `AxoBuilder`, `JsonlStore::new`, the traits `SessionStore`, `SessionWriter`, `ConfigSource`, `ExecEnv`, `StaticEnv::host`, `DevShellEnv::new` | host construction and host singletons |
| `Axo::supervisor`, `ProcessSupervisor::new`/`spawn`, all of `Process` and `Container`, `exec::supervisor::run` | live process handles; commands go through `tools.runCommand` |
| `Models::auth` | returns credential secrets |
| `Models::login` | interactive; the host renders login UI |
| `Models::load`, `reload`, `refresh`, `register`, `unregister`, `replace`, `set_unavailable`, `provider_layer`, `validate`, `hidden` | the host owns the registry; extensions contribute providers through their manifest |
| `Schemes::new`, `Artifacts::new`, `HostTool::*`, `Projection::apply`, `FileLedger::restore`, `NestedContext::new`/`reset`/`restore`/`disabled`, `EntryId::generate` | construction or internal state rebuilding with no meaning across the wire |
| `Axo::schemes` | takes host scheme objects; a live session's schemes are reached through `tools` |

If a method not listed here needs a decision, apply the parity rule: expose it unless one of the three grounds applies, and write the ground in the skip reason. Pure helpers on data types (`ToolLimits::permits`, `BashAccess::is_full`, `Entry::usage`, `JobStatus::label` and similar) carry data that already crosses; skip them with `derivable from the value in TypeScript` unless a caller needs the Rust logic, in which case expose them on the nearest object.

## 3. How to work

- Repo `/repo/rust/axo`. Run cargo through `nix develop -c …`. No Python. Real Bun for gated tests: pass an absolute path in `AXO_TEST_BUN` (last known: `/nix/store/0gk371kgjyv7s0az07hxjrl8g9k0pf3m-bun-1.4.4/bin/bun`). No staging or commits unless Toph asks.
- The dev shell has cargo only: no `tsc`, no node. TypeScript is checked by Bun importing it.
- Follow `AGENTS.md` (repo root): embedder-first design, living documentation, code invariants.
- Orchestrate with subagents that each own named files, skip builds and formatters, and never touch another owner's files. The orchestrator owns manifests, regenerates bindings and runs every gate.
- Toph does not write code and comes from Java. Explain in plain, practical terms with a concrete example; abstract framing loses him. Ask him only about real preferences not covered here, through a short form with a recommended default.
- The plan is an execution spec: a fresh agent must be able to run it top to bottom with no design decisions left. Ground every path, symbol and signature by reading the code in the planning session.

## 4. First: read what plan A actually built

The code is the truth, not this brief. Before designing anything, read:

- `crates/axo-macros/src/lib.rs`: the real macro contract (attribute arguments, owned-type mapping, generated arg/result structs, the `target = …` wire-wrapper form).
- `crates/axo/src/ext.rs`: `Block`, `Entry`, `Meta`, `Caller`, `InvokeError`, `Base64`, the session block list and `Session::invoke`.
- `crates/axo/src/session/wire.rs`: how adapted methods are written (`SessionWire`).
- `crates/axo/tests/bindings.rs` and `packages/axo-extension-sdk/bindings.ts`: how types and the `Session` interface are generated.
- `crates/axo/tests/ext_surface.rs`: the guard and its `PENDING` list. **That list is plan B's authoritative remainder.** The inventory in section 6 was not verified complete.
- `crates/axo-extensions/src/session.rs` (the `invoke`, `subscribe`, `unsubscribe` actions), `peer.rs` (the call chain), `runner.ts` (the `Proxy` handle, stall detector), `packages/axo-extension-sdk/index.ts`.

Then record in plan B: how many types gained the specta derive and any that needed manual overrides; any `Session` method plan A had to withhold unexpectedly (for example a non-`Send` future); whether the task-local call chain and Bun's `AsyncLocalStorage` worked or fell back.

Plan B generalises three things plan A hard-coded to `session`:

- the block list and `invoke` (one per exposed object),
- the bridge's `invoke` action, which refuses any object other than `session`,
- the bindings generator, which emits one `Session` interface.

## 5. Design direction per area

These are the agreed shapes. Exact signatures are plan B's job.

### 5.1 `tools` for tool handlers

Problem being fixed: a TypeScript tool that writes a file with Bun's own APIs skips the file ledger, the per-file lock and snapshots, so the built-in `edit` cannot detect its changes and the UI cannot show or undo them. Its subprocesses are not session jobs and not sandboxed. The runner does not start in the session's working directory.

- Expose `SessionTools` as the `tools` object: working directory, session ID, `resolve_path`, `locate`, `rewrite_urls`, `bash_access`, `write_scope`, `check_write`, scheme `parse`/`exact`/`resolve`/`read`/`write`, the ledger snapshot, snapshot read, and a supervised command run.
- **Lift the guarded write.** The sequence in the built-in write tool (`crates/axo/src/tools/write.rs:89–162`) is: `locate` → `check_write` → `lock_file` → `ledger.check_fresh` (existing file with different content only) → write → `record_change` under the lock. `edit.rs:106–191` repeats it. Move it into one public `SessionTools` method that both built-in tools call, and expose that method. Add a matching read that records the ledger hash the way `read.rs:130` does. Do not expose `lock_file` or `record_change` on their own: a lock guard cannot cross the wire, and recording without the lock is the bug being fixed.
- **`run_command` needs the live tool call.** `SessionTools::run_command(&self, cx: ToolContext, request: CommandRequest)` takes the tool's context (cancellation, updates, job delivery). The bridge has that context inside `RemoteTool::execute` (`crates/axo-extensions/src/session/bindings.rs:85`), and the protocol already serves runner calls parented to a host call (that is how `update` works). Serve `tools` calls as parented calls of the tool invocation so the real `ToolContext` is used. A `tools` call after the tool returned is refused, which is correct.
- Scheme writes through `tools` apply `check_write` first, as the built-in write tool does for virtual targets (`write.rs:89–114`).
- Outside a tool call there is no `tools` object. A command handler that wants file safety uses the session's own methods.

### 5.2 Handler inputs

Bring each handler's input up to its Rust twin (gaps found in research):

| Handler | Missing today |
|---|---|
| Tool | steering signal, invocation ID, per-call `target`, dynamic `replay`; `tools` (5.1) |
| Tool policy | full agent and mode specs (names only today), effective write scope, the assistant message, the existing result's `terminate` |
| Spawn hook | `task` (was omitted because `PromptInput` had no wire form; plan A gave it one), tool limits, bash and write ceilings, full mode and model, the originating tool call ID inside `origin` |
| Scheme | nothing structural; paths cross as strings and a non-UTF-8 path fails that call |
| Entry events | plan A left the `onEntry` pump beside the new subscriptions. Fold `onEntry` onto the subscription mechanism so there is one event path |

Build these payloads from the Rust types with serde, replacing the hand-built `json!` in `session/bindings.rs` (`scope_json`, `call_json`, the spawn map). The generated bindings then type the handler arguments, and the hand-written SDK handler types go.

### 5.3 `host` and the lifecycle hook

- Direct, no hook needed: `Axo::list_sessions`, `open_session_read`, `read_snapshot`, `squash_changes`, `catalog`, and `session(id)` for reaching a child by ID. Journal reads are paged (a full journal can exceed the 64 MiB frame cap); reuse plan A's paging shape.
- The hook lives in `axo-extensions`, is implemented by the host, and covers create, open, fork and close. For each, the host builds the non-serialisable parts of `SessionOptions` (tools, policies, hooks, schemes, exec environment), calls `Extensions::compose`, keeps the returned session alive, calls `Extensions::attach`, and tells its UI. The extension receives the new session's ID and uses it through `ctx.session(id)`.
- Why a hook and not a direct call: `Axo::create_session`, `open_session` and `Session::fork` return an owned session, and dropping the last owner shuts it down. Something must hold it, and the host's UI must learn it exists. Opening a session that is already open returns `SessionBusy`; the hook lets the host hand back the one it already holds.
- Create and open take a data-only request (cwd, model selector, agent, thinking, project access, sections). Fork takes an entry ID and returns the new ID plus the editable parts (`Fork.editable`). Close asks the host to shut down and release.
- With no hook registered every lifecycle call is refused with one clear error code. Listing and reading still work.
- Spawned children: `spawn` returns a receipt. Children inherit extension registrations but are not attached by the bridge today, so their session actions and events do not work. Attach them so `ctx.session(childId)` works.
- Fact to design around: `Extensions::new(models, supervisor, options)` holds no `Axo`. The `host` object needs one; the host must hand it over (with the hook, or separately). Choose one way and state it.

### 5.4 `models`

- Expose: `Models::find`, `all`, `providers`, `credential_source`; the three estimators in `crates/axo-ai/src/estimate.rs` (`estimate_text_tokens`, `estimate_blocks_tokens`, `estimate_message_tokens`); a one-off completion.
- `ProviderInfo` and `CredentialSource` carry no secrets and need derives.
- Completion: takes a model, messages and the data fields of `SimpleStreamOptions` (thinking, max tokens, temperature, tool choice). It must not accept `headers`. It runs through the host's `Streamer`, the same one sessions use, so the scripted test API serves it. `Axo` keeps `models` and `streamer` private with no accessor (`crates/axo/src/axo.rs:32–37`); add what is needed in `axo` as a small facade and expose the facade. Provider streams never return `Err`; a failure is a terminal `AssistantEvent::Error`. Return the final `AssistantMessage` (use `Accumulator`), and map the error event to a typed failure. Offer streamed events through the existing subscription mechanism; do not add a second streaming protocol.
- Cancellation: a cancelled call drops the stream.

### 5.5 Everything else exported

Work through `PENDING`. For each type: annotate its `impl` blocks, add derives following plan A's wire-shape rules, add the object to the generator if it is addressable (`tools`, `host`, `models`), and remove it from `PENDING`. Free functions are outside the macro's reach; the pure ones that TypeScript cannot reproduce (`jobs_from_entries`, `project`, `squash`, `file_change`, `Command::parse`/`decode`, the `valid_*_name` checks, `AgentSpec::parse`, `ModeSpec::parse`, `KeywordSpec::parse`) go on the `host` object as facade methods. Extend the guard so an exported `pub fn` at module level must be either facaded or listed with a reason.

When `PENDING` is empty, delete it and the two checks that reference it, so the guard simply covers every exported type.

### 5.6 Generated reference and docs

- Generate `crates/axo/docs/extension-api.md` from macro metadata: every exposed method per object, and every withheld method with its reason. Check it in with a staleness test like the bindings.
- A new guide must be added in three places or it is invisible: the `BUNDLED_DOCS` array (`crates/axo/src/schemes/dir.rs:11–42`), the doctest module in `crates/axo/src/lib.rs:73–91`, and the index in `crates/axo/docs/README.md`. `doc_identifiers` scans it automatically: every backticked CamelCase or SCREAMING_SNAKE token must exist as a word in `crates/*/src`.
- Bring `plan/design.md`, `plan/plan.md`, `README.md`, `AGENTS.md` and `crates/axo/docs/extending.md` to the final state, including the plain statement that extensions are trusted and nothing here is a sandbox.

## 6. Facts from research (re-verify lines before relying on them)

**Transport.** One authenticated Unix socket per runner generation; 4-byte length plus JSON; 64 MiB frame cap both ways; one terminal reply per call; Rust allows 256 live runner calls and closes the peer on the 257th; the runner starts at most 128 host calls and queues the rest. Runner calls with a `parent` are served by the parent host call's callbacks and are cancelled when it ends; unparented calls go to the session registry. Cancelling drops the Rust future and never rolls back a mutation.

**`Axo` (`crates/axo/src/axo.rs`).** `create_session(options) -> Result<Session>` :168; `open_session(path, options)` :192; `session(id) -> Result<Arc<Session>>` :250; `open_session_read(path) -> (Header, Vec<SeqEntry>)` :255; `read_snapshot(session, sha256) -> Vec<u8>` :261; `squash_changes(session, changes)` :267; `list_sessions(cwd) -> Vec<SessionInfo>` :276; `schemes(path, access, host)` :290; `catalog(cwd, access, agents, modes, keywords) -> Catalog` :319. `SessionInfo` has paths and a `SystemTime` and no serde. `Catalog` holds `Arc`s and no serde.

**`SessionTools` (`crates/axo/src/tools/mod.rs`, `writes.rs`).** Public fields: `cwd`, `exec`, `ledger`, `context`, `schemes`, `supervisor`, `snapshots`. Methods: `session_id` :199, `record_change(path, url, before, after) -> Value` :207, `bash_access` :254, `run_command(cx, request)` :263, `locate(path) -> Located` :272, `rewrite_urls` :295, `resolve_path` :315, `write_scope` (writes.rs:90), `check_write(target)` (writes.rs:102). Its constructor is crate-private. Errors here are `ToolError`, not `axo::Error`; the typed-error mapping needs a case for it.

**`Schemes` (`crates/axo/src/schemes/mod.rs`).** `context` :189, `artifacts` :194, `prompt_docs` :225, `parse` :236, `exact` :277, `resolve` :291, `read` :296, `write` :318. `SchemeUrl`, `SchemeContext`, `Target` have no serde.

**`FileLedger` (`tools/ledger.rs`).** `snapshot` :83, `hash` :91, `record` :97, `recorded` :117, `check_fresh` :126. `lock_file(path) -> OwnedMutexGuard<()>` :167.

**`Models` (`crates/axo-ai/src/models/mod.rs`).** `find(selector) -> Option<Model>` :438; `all() -> Arc<[Model]>` :448; `providers() -> Vec<ProviderInfo>` :453; `credential_source(provider)` :480; `login` :491; `auth(model) -> ModelAuth` :516 (secrets). `Streamer::stream(model, transcript, opts, cancel) -> BoxStream<'static, AssistantEvent>` (`api/mod.rs:29–54`).

**Extension manager (`crates/axo-extensions`).** `Extensions::compose(options) -> SessionOptions`, `attach(&Arc<Session>)`, `detach(id)` in `src/session.rs`. Attachment holds a weak reference; children get actions and events but no layer. Rust→runner handler payloads are hand-built in `src/session/bindings.rs`.

**Other types with public methods** (from an inventory that was not verified complete): `Artifacts`, `SnapshotStore`, `CommandOutput`, `NestedContext`, `Projection`, `EffectiveConfig`, `Settings`, `Entry`, `EntryBase`, `EntryId`, `Header`, `ChildHandle`, `TeamHandle`, `ToolLimits`, `BashAccess`, `AgentSpec`, `ModeSpec`, `KeywordSpec`, `Command`, `EnabledExtension`, `ProjectSettings`, `RunAs`, `TeamMember`, `AgentDef`, `ContextFile`, `JobStatus`.

**Tests.** `axo_ai::testing::ScriptedApi` for scripted model turns; helpers in `crates/axo/tests/common/mod.rs`; scripted fake runner in `crates/axo-extensions/tests/bridge.rs`; real Bun in `tests/bun_provider.rs` with fixtures under `tests/fixtures/`. All `cargo test` suites run offline.

## 7. What plan B must contain

- Ordered steps that keep the tree building and existing tests passing after each, with single file owners and stated parallel splits.
- For every new or changed symbol: exact signature, wire name and JSON shape; for every removal: every call site.
- The final decision for every name in `PENDING`, as a table.
- Acceptance checks with concrete input and expected observable output, at minimum:
  - a fixture TypeScript tool edits a file through `tools`; a later built-in edit sees the ledger entry and a snapshot exists;
  - a stale write through `tools` is refused the way the built-in tool refuses it;
  - a fixture tool runs a command through `tools` and it appears as a session job;
  - a spawn hook receives the task and the originating call ID;
  - with a hook registered, an extension forks the current session, the host's hook is called, and the fork answers `ctx.session(newId)` calls; with none, the call is refused with the lifecycle error code; listing and reading work either way;
  - an extension lists models and gets a completion from the scripted API; no exposed value contains a credential;
  - `ext_surface` passes with no pending list; the generated reference is current.
- Gates after each step: `cargo check --workspace --all-targets`, the step's tests, `cargo test -p axo --test bindings --test ext_surface --test doc_identifiers`, the real-Bun suite when the runner changed, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`.
- Pre-decided fallbacks for anything that may not hold, so the executor never stalls.
