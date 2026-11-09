# axo Documentation

`axo` is an embeddable, headless coding-agent harness core written in Rust across three crates: `axo-ai`, `axo-agent`, and `axo`.

This directory documents the implemented behavior and public APIs of the workspace. It lives inside the `axo` crate: this index and its four guides are compiled into the binary. When the host sets `AxoBuilder::cache_dir`, they are written to `<cache_dir>/docs/` and reached at runtime through `axo://README.md`, which the `docs` system-prompt section points to; without a cache directory, neither the section nor the scheme exists.

The harness name defaults to `axo`. `AxoBuilder::name` changes the documentation scheme and section and the bundled `SYSTEM`/`SUMMARY` self-references; materialized docs use the chosen URL scheme, while Rust crate names stay unchanged.

## Reference Guides

| Document | Covers |
| :--- | :--- |
| [`configuration.md`](configuration.md) | The `ConfigSource` contract (`UserConfig`, `ConfigLayer`, `PromptOverride`), `EffectiveConfig` evaluation and trust filtering, settings and absent-input behavior, model selectors and user-only `modelRoles`, child defaults and budgets (`subagents`), models and credentials, project access, Nix / direnv dev-shell capture, and host-selected command supervision (`Supervision`) |
| [`prompts-and-context.md`](prompts-and-context.md) | System-prompt sections and overrides, fixed child-only `delegation` framing and host-section isolation, agent definitions, model-facing `spawn` and schema-validated `yield`, team catalogs, context-file switches and nested loading, skills and command expansion, prompt parts (`PromptPart`), command promotion and retained helpers, `wait`, `proc://`, `agent://`, and capped, escaped command/child deliveries |
| [`sessions-and-compaction.md`](sessions-and-compaction.md) | Durable JSONL session trees and receipts, child spawn links with persisted specs, tool-origin admission and crash-safe child adoption, `child_start` and per-run `job_end` messages, exactly-once delivery and parent-reopen reconciliation, parking/revival, idle follow-ups, shutdown recovery, turn budgets and FIFO child-run slots, `Projection`, crash recovery, `attach`, `navigate`, `fork`, `/shake`, compaction, transient retries, and overflow recovery |
| [`extending.md`](extending.md) | Declarative configuration and Rust embedding, `Session::spawn` with `SpawnRequest` / `InlineSpec` / `ChildHandle`, output overrides and `SpawnHook` policy with `SpawnOrigin`, `Axo::session` revival and `Session::agent`, `JobInfo` / `JobKind`, `ScriptedApi::lane`, builder resources and supervision, prompting and commands, and extension traits (`Tool`, `Scheme`, `LoopHooks`, `ExecEnv`, `SessionStore`, `Persist`, `CredentialStore`, `Api`, `Streamer`) |
