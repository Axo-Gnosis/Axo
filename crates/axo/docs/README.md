# axo Documentation

`axo` is an embeddable, headless coding-agent harness core written in Rust across three crates: `axo-ai`, `axo-agent`, and `axo`.

This directory documents the implemented behavior and public APIs of the workspace. It lives inside the `axo` crate so the five guides are bundled with the crate and embedded in its system-prompt documentation.

## Reference Guides

| Document | Covers |
| :--- | :--- |
| [`configuration.md`](configuration.md) | User (`~/.axo/`) and project (`<cwd>/.axo/`) directories, `settings.jsonc`, `models.jsonc`, `auth.json`, project access levels (`ignore`, `read`, `trust`), and Nix / direnv dev-shell capture |
| [`prompts-and-context.md`](prompts-and-context.md) | The sectioned system prompt (`system`, `docs`, `tools`, `schemes`, `work`, `prose`, `project_context`, `skills`, `cwd`, `dev_shell`, `date`, host sections), prompt file overrides, the `contextFiles` map, on-demand nested context loading, `.agents/skills` (`argument-hint`, names reserved by built-in commands, `/name` expansion), and prompt parts (`PromptPart`: text, pastes, `@` file references, content saved under `local://`) |
| [`sessions-and-compaction.md`](sessions-and-compaction.md) | Durable JSONL session trees (`JsonlStore`) with `submission` parts, `Projection` and `context_edit`s, crash recovery (requeued submissions re-rendered from their parts), `attach`, `navigate`, `fork` (`Fork::editable` parts), `/shake`, automatic compaction, transient retries, and overflow recovery |
| [`extending.md`](extending.md) | Customizing `axo` through declarative files (`.agents/skills`, `AGENTS.md`, `.axo/prompts/`, `models.jsonc`) and embedding or extending the Rust crates (`Axo`, `Session` prompting with `PromptInput`, the command catalog and `run_command`, `Tool`, `Scheme`, `ToolFactory`, `LoopHooks`, `ExecEnv`, `SessionStore`, `Api`, `Streamer`) |
