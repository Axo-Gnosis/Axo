# Handoff: axo claims configuration a host should own

Status: resolved (see plan/design.md Configuration ownership; implemented via `ConfigSource` and `EffectiveConfig::resolve`). Written 2026-11-04.

## The issue

axo is a library meant to be embedded by a host (Axome today; an ACP binary later, see `plan/design.md`). The library currently decides, by itself, where configuration lives on disk, what the files are called, and that there is a file at all. A host that embeds axo therefore ships a second configuration surface it does not own: its users configure the host in the host's directory and the harness in `~/.axo` / `<cwd>/.axo`, with axo's file names and axo's loading rules, even though the host is the product the user sees.

The concrete case that surfaced it: a host feature such as a `/plan` command wants a named model (`modelRoles.plan`). The name means nothing to axo and everything to the host, yet under the current code it has to be written in axo's `settings.jsonc`, in axo's directory, under axo's merge and trust rules, next to a host settings file that holds the rest of the feature's configuration.

## What the code does today

User directory (`crates/axo/src/config.rs`, `AxoDir`): `AXO_DIR`, else `$HOME/.axo`. Host-overridable only as a whole via `AxoBuilder::dir`. Inside it axo fixes the names `settings.jsonc`, `models.jsonc`, `auth.json`, `prompts/<NAME>.md`, `skills/`, `cache/` (`cache/docs`, `cache/devshell`, catalog cache), `sessions/`, `AGENTS.md`.

Project directory: the string `.axo` is hardcoded, not parameterized:
- `config.rs:293` `<cwd>/.axo/settings.jsonc`
- `config.rs:157` `.axo/AGENTS.md` as a context-file name
- `context/prompt.rs:85` `<cwd>/.axo/prompts/<NAME>.md`
- `config.rs:205` `ProjectFiles.axo_dir` toggles the above
- planned in `plan/plan.md` and `plan/design.md`: `.axo/agents`, `~/.axo/agents`, `.axo/extensions`, `~/.axo/extensions`, `~/.axo/mcp.json`

Loading entry points that read those paths directly: `Settings::load` (`config.rs:283`), `load_models_config` (`config.rs:394`), `FileCredentialStore::new(dir.auth_path())` (`axo.rs:105`), `JsonlStore::new(dir.sessions_dir())` (`axo.rs:116`), `prompt_override` (`context/prompt.rs:78`, also used by `compaction/summary.rs:55`), `ensure_bundled_docs` (`context/prompt.rs:63`) and `DirScheme::docs(dir.docs_dir())` (`session.rs:1833`), `load_skills` (`context/skills.rs:36`, root `dir.skills_dir()`), `home_agents_dir` (`context/mod.rs:57`).

The programmatic seam exists but is incomplete. `AxoBuilder::settings(Settings)`, `::models`, `::store`, `::streamer` let a host supply values; when the host supplies `Settings`, `settings_from_dir` becomes false and `Axo::start` (`axo.rs:246-253`) stops merging project-level settings per session. A host that wants project settings is pushed back onto axo's file loader. Trust enforcement (`USER_ONLY_KEYS`, the `!command` `defaultModel` rule for untrusted projects, `project.access`) lives inside `Settings::load` and is only applied to files axo reads itself.

Two facts that make the fix cheap: `Settings` is `#[serde(default)]` and does not reject unknown keys, so a host file may carry axo's keys next to its own; and the merge/trust rules operate on `serde_json::Value` objects before deserialization, so they do not depend on where the values came from.

Cross-tool roots are a different category and are not part of the issue: `AGENTS.md` / `CLAUDE.md` per directory, `.agents/AGENTS.md`, `.agents/skills`, `~/.agents/*` (behind `homeAgentsDir`) are ecosystem conventions other tools also read.

The same leak exists on the model-facing side: the `axo://` scheme name, the bundled docs under it that describe "axo", and the word "axo" in prompt text. Track 4 lets a host replace a built-in scheme but not rename it. This is noted as the adjacent problem; treat it as a follow-on unless it falls out naturally.

## The goal

axo owns formats and semantics; the host owns names, locations and files. Precisely:

1. The library core takes **values and sources**, never paths. `Settings`, `ModelsConfig`, credentials, prompt overrides, agent definitions (Track 2), skills roots, the session store and the docs materialization are each obtained through a host-provided source with a value-level API. axo decides what the values mean; it does not decide which file they came from or what that file is called.
2. Trust and merge semantics stay in axo and keep working for embedders. Because the rules are per layer (user vs project, `ProjectAccess`), a source hands axo **layers**, not a merged result, so axo can still enforce user-only keys and the untrusted-project rules without knowing about files. Sketch, not a mandate:
   ```rust
   pub trait SettingsSource: Send + Sync {
       fn layers(&self, cwd: &Path, access: ProjectAccess) -> Result<SettingsLayers, Error>;
   }
   pub struct SettingsLayers { pub user: Value, pub project: Option<Value> }
   ```
   The host extracts axo's sub-object from wherever it keeps it (nested under a key, flattened, a different file format) at each layer.
3. The file-backed implementations (today's `AxoDir`, `Settings::load`, `load_models_config`, `FileCredentialStore`, `JsonlStore` default, `prompt_override`, skills roots, docs cache) become **one host** among others: a self-contained module (`axo::fs` or similar) that the standalone/ACP binary and the tests use, with `.axo`, `~/.axo`, `AXO_DIR` as *its* choices. An embedder that does not use that module has no `.axo` anywhere and no axo-named file in its user-facing configuration.
4. Per-session project settings work identically whether values come from that module or from a host source. The `settings_from_dir` special case goes away.
5. Nothing in the library core, its docs under `crates/axo/docs/`, or the plan files names `.axo`, `~/.axo`, `settings.jsonc`, `mcp.json`, or `AXO_DIR` as design. Those names appear only in the file-layer module's own documentation. Plan bullets that currently do (Track 2 agent dirs, Track 5 extension and MCP paths) are rewritten as source-provided.

## Scope and constraints

- Clean cutover, no shims: Toph owns every consumer (Axome in `/repo/rust/Axome`, tests, examples). Migrate them.
- Keep the file layer's observable behavior for the standalone case: same files, same merge and trust rules, same diagnostics, so `cargo test -p axo` offline suites and `doc_identifiers` keep passing after docs are updated.
- `crates/axo/docs/` is compiled into the binary and exposed to agents; update it in the same change, written host-neutrally (describe sources and semantics; move the `.axo` layout into the file-layer section or the ACP/standalone docs).
- Update `plan/design.md` (settled decision: values and sources, file layout belongs to a host) and `plan/plan.md` (Track 2 and 5 path bullets; `AGENTS.md` repo rules mention `~/.axo` and should follow).
- Out of scope unless trivial: renaming the `axo://` scheme and prompt identity. Record it in `plan/plan.md` open questions if not done.
- Repository invariants in `AGENTS.md` apply (no unsafe, no `MutexGuard` across `.await`, forward-compatible storage, offline tests).

## Acceptance

- Axome can build `Axo` with its own settings file(s), in its own directory, under its own names, including project-level layers with axo's trust rules applied, without any `.axo` directory existing.
- The standalone file layer still reads `~/.axo` / `<cwd>/.axo` exactly as before, as a host implementation of the sources.
- `rg -n '\.axo|AXO_DIR|settings\.jsonc' crates/axo/src crates/axo/docs plan` matches only the file-layer module and its documentation.
- `cargo check --workspace --all-targets`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check` pass inside `nix develop`.

## Related

- `plan/research/subagents.md` §15 (embedding changes for subagents) assumes this split: agent definitions and model aliases arrive as values from the host.
- `plan/design.md` "Ways to extend axo" and the Track 4 decision that host schemes go in `SessionOptions` are the existing precedent for host-supplied inputs.
