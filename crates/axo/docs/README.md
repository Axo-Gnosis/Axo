# axo Documentation

`axo` is an embeddable, headless coding-agent harness core written in Rust across three crates: `axo-ai`, `axo-agent`, and `axo`.

This directory documents the implemented behavior and public APIs of the workspace. It lives inside the `axo` crate so the five guides are bundled with the crate and embedded in its system-prompt documentation.

## Reference Guides

| Document | Covers |
| :--- | :--- |
| [`configuration.md`](configuration.md) | User (`~/.axo/`) and project (`<cwd>/.axo/`) directories, `settings.jsonc`, `models.jsonc`, `auth.json`, project access levels (`ignore`, `read`, `trust`), and Nix / direnv dev-shell capture |
| [`prompts-and-context.md`](prompts-and-context.md) | The sectioned system prompt (`system`, `docs`, `tools`, `work`, `prose`, `project_context`, `skills`, `cwd`, `dev_shell`, `date`, host sections), prompt file overrides, the `contextFiles` map, on-demand nested context loading, and `.agents/skills` |
| [`sessions-and-compaction.md`](sessions-and-compaction.md) | Durable JSONL session trees (`JsonlStore`), `Projection` and `context_edit`s, crash recovery, `attach`, `navigate`, `fork`, `/shake`, automatic compaction, transient retries, and overflow recovery |
| [`extending.md`](extending.md) | Customizing `axo` through declarative files (`.agents/skills`, `AGENTS.md`, `.axo/prompts/`, `models.jsonc`) and embedding or extending the Rust crates (`Axo`, `Session`, `Tool`, `LoopHooks`, `ExecEnv`, `SessionStore`, `Api`, `Streamer`) |
