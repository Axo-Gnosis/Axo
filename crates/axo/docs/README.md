# axo Documentation

`axo` is an embeddable, headless coding-agent harness core written in Rust across three crates: `axo-ai`, `axo-agent`, and `axo`.

This directory documents the implemented behavior and public APIs of the workspace. It lives inside the `axo` crate: this index and its four guides are compiled into the binary. When the host sets `AxoBuilder::cache_dir`, they are written to `<cache_dir>/docs/` and reached at runtime through `axo://README.md`, which the `docs` system-prompt section points to; without a cache directory or on write failure, neither the section nor the scheme exists (see [Bundled documentation](prompts-and-context.md#bundled-documentation-docs)).

The harness name defaults to `axo`. `AxoBuilder::name` changes the documentation scheme name and the text of the `docs` section (the section key stays `docs`) and the bundled `SYSTEM`/`SUMMARY` self-references; materialized docs use the chosen URL scheme, while Rust crate names stay unchanged.

## Reference Guides

| Document | Covers |
| :--- | :--- |
| [`configuration.md`](configuration.md) | The `ConfigSource` contract (`UserConfig`, `ConfigLayer`, `PromptOverride`), `EffectiveConfig` evaluation and trust filtering, host-owned resources (including harness-name validation), settings and absent-input behavior, model selectors and user-only `modelRoles`, child defaults and budgets (`subagents`), models and credentials, project access, Nix / direnv dev-shell capture, config value resolution, and host-selected command supervision (`Supervision`) |
| [`prompts-and-context.md`](prompts-and-context.md) | System-prompt sections including fixed `delegation` and `team`, prompt overrides and summarizer prompts, bundled docs materialization, agent/team definitions, model-facing `spawn` and schema-validated `yield`, team assignments and mail, context files and nested discovery, skills, prompt parts (`PromptPart`), background jobs (`bg_N`) and results in context, command promotion, `wait` and mutual-wait errors, `proc://`, `agent://` messaging routes, and escaped deliveries |
| [`sessions-and-compaction.md`](sessions-and-compaction.md) | Storage layout and single-writer lock, the session journal, durable JSONL trees and receipts, crash recovery, command-job durability, persisted child specs and team links, multi-row spawn admission and replay, `child_start` team identity, `job_end` notes, mail submissions, exactly-once delivery and reopen reconciliation, parking/revival, idle steers and follow-ups, shutdown recovery, turn budgets and team slot reservation, projection, attach, branching, shake, compaction, and transient retry and overflow recovery |
| [`extending.md`](extending.md) | Declarative configuration and Rust embedding, `SessionEvent`, the command catalog (`Command`, `CommandInfo`, `run_command`), `Session::spawn` / `Session::spawn_team` and their request/handle types, `SpawnHook` policy with `SpawnOrigin` and `SpawnTeam`, child revival, `JobInfo` / `JobKind`, deterministic model lanes, builder resources, session controls, `SessionTools` / `ToolFactory` host tools and supervised commands, queued inputs and message origins, and extension traits |
