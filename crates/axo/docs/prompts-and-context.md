# Prompts, Context Files & Skills

`axo` represents the system prompt and tool set as `SystemMessage` entries inside the transcript (`crates/axo/src/context/prompt.rs`). Replaying the transcript reconstructs the exact prompt and tool definitions at any point in a session, and mid-session updates emit a diff `SystemMessage` containing only changed sections (`Some(rendered)`) or removed sections (`None`).

## System Prompt Sections

`build_sections` produces an `IndexMap<String, String>` where each non-empty body is wrapped in `<name>\n{body}\n</name>` in this exact order:

| # | Section | Source |
| :--- | :--- | :--- |
| 1 | `system` | `SYSTEM.md` override, else bundled `crates/axo/prompts/SYSTEM.md` |
| 2 | `docs` | `"axo's documentation, including how to extend it: axo://README.md"` (materialized from the bundled docs) |
| 3 | `tools` | Deduplicated, trimmed `Tool::prompt_guidelines()` from the active tools, formatted as `- {rule}` bullets |
| 4 | `schemes` | URL path guidance and one bullet per scheme's non-empty prompt documentation |
| 5 | `work` | `WORK.md` override (no bundled default; omitted unless provided) |
| 6 | `prose` | `PROSE.md` override, else bundled `crates/axo/prompts/PROSE.md` |
| 7 | `project_context` | `<project_instructions path="…">\n{content}\n</project_instructions>` blocks joined by `\n\n` |
| 8 | `skills` | Skill catalog with `skill://<name>` locations when `read` or `bash` is active and at least one skill has `disable_model_invocation == false` |
| 9 | `cwd` | `inputs.cwd.display()` |
| 10 | `dev_shell` | Trimmed `Resolved::prompt_note` from the active `ExecEnv` (`"Commands run inside the Nix dev shell of {dir}; its tools are already on PATH."` or `"Commands run with the direnv environment of {dir}."`) |
| 11 | `date` | Local date formatted as `%Y-%m-%d`, refreshed when a prompt runs on a new day |
| 12 | `<host sections>` | Entries from `SessionOptions::sections`: `Some(body)` inserts or replaces in place, `None` removes the section |

### Bundled documentation (`docs`)

`build_sections` materializes the crate's bundled documentation (`README.md`, `configuration.md`, `prompts-and-context.md`, `sessions-and-compaction.md`, `extending.md`) into `<AXO_DIR>/cache/docs/` (`AxoDir::docs_dir()`), updating files on disk only when their contents differ. The `docs` section points to `axo://README.md`, which resolves to the bundled entry point in that directory. If `<AXO_DIR>/cache/docs/` cannot be written, `build_sections` records a `Diagnostic` and omits the `docs` section.

### Built-in tool guidelines (`tools`)

Only `read`, `write`, and `edit` define `prompt_guidelines()` in the built-in tool set:
- `read`: `"Use read to examine files instead of cat or sed."`
- `write`: `"Use write only for new files or complete rewrites."`
- `edit`: `"When changing several places in one file, use one edit call with several entries in edits[]."`

### Host section validation

Host section names in `SessionOptions::sections` must start with an ASCII lowercase letter (`a-z`) and contain only `a-z`, `0-9`, `_`, or `-`. Invalid names emit a `Diagnostic` and are skipped. Overriding an existing section name updates its value at its original position in `IndexMap` rather than moving it to the end.

## Prompt Overrides (`prompts/`)

`prompt_override` checks `<cwd>/.axo/prompts/<NAME>.md` first (when project access is not `Ignore` and `project.files.axoDir` is `true`), then `<AXO_DIR>/prompts/<NAME>.md`.

### System-prompt files

| File | Section | Bundled default | Empty / whitespace-only override |
| :--- | :--- | :--- | :--- |
| `SYSTEM.md` | `system` | `crates/axo/prompts/SYSTEM.md` | Omitted from the prompt |
| `WORK.md` | `work` | None | Omitted from the prompt |
| `PROSE.md` | `prose` | `crates/axo/prompts/PROSE.md` | Omitted from the prompt |

### Summarizer prompt files (`SummaryPrompts::load`)

Loaded when a session starts (`crates/axo/src/compaction/summary.rs`):

| File | Bundled default | Used for |
| :--- | :--- | :--- |
| `SUMMARY.md` | `crates/axo/prompts/SUMMARY.md` | System prompt for separate summary requests, and appended rules for live summary requests |
| `COMPACTION.md` | `crates/axo/prompts/COMPACTION.md` | First compaction checkpoint |
| `COMPACTION_UPDATE.md` | `crates/axo/prompts/COMPACTION_UPDATE.md` | Subsequent compaction checkpoints updating a previous summary |
| `COMPACTION_SPLIT_TURN.md` | `crates/axo/prompts/COMPACTION_SPLIT_TURN.md` | Summarizing the prefix of a split turn |
| `BRANCH_SUMMARY.md` | `crates/axo/prompts/BRANCH_SUMMARY.md` | Summarizing an abandoned branch during `Session::navigate` with `summarize: true` |

If a summarizer override file is empty or whitespace-only, `SummaryPrompts::load` records a `Diagnostic` (`"Empty summary prompt override ignored"`) and keeps the bundled prompt.

## Context Files (`crates/axo/src/context/files.rs`)

### Startup discovery (`load_context_files`)

1. **User files:** `~/.agents/AGENTS.md` (when `home_agents_dir` is `true`), then `<AXO_DIR>/AGENTS.md`.
2. **Project walk:** When `project` is `true` (`project_access != ProjectAccess::Ignore`), `axo` walks every directory from `project_root(cwd)` (the git root, or `cwd` outside a repository) down to `cwd`.
3. **File selection & ordering (`context_file_names`):**
   - Keys in `settings.context_files` must be non-empty relative paths with only `Component::Normal` segments; absolute paths or paths containing `..` or `.` emit a `Diagnostic` and are skipped.
   - Enabled keys (`true`) are ordered in map order, except any key matching `<stem>.<tag>.md` is placed immediately after `<stem>.md` when `<stem>.md` is also present in the map.
4. **Deduplication:** Files are deduplicated by canonical path (`fs::canonicalize`) and by exact trimmed UTF-8 content, so symlinks or identical copies of `AGENTS.md` and `CLAUDE.md` load only once. UTF-8 BOMs are stripped.

### On-demand nested context files (`NestedContext`)

When `settings.nested_context_files` is `true`, successful `read`, `write`, and `edit` calls invoke `SessionTools::nested_context_for(&path)`:
- For a touched path inside `project_root`, `NestedContext` checks every directory from `project_root` down to the file's parent directory.
- Any enabled context file whose canonical path and content are not already present in the startup `context_files` or in the active projection's tool results is appended to that tool result as `<project_instructions path="…">\n{content}\n</project_instructions>`.
- `NestedContext::restore(&messages)` reconstructs the seen set from the current projected messages on session open, branch navigation, compaction, and `/shake`. If a tool result carrying a nested context file is elided or compacted away, the next file access in that directory reattaches it.

## Skills (`crates/axo/src/context/skills.rs`)

### Discovery (`load_skills`)

`load_skills` scans roots in this order:
1. `<dir>/.agents/skills` for each directory from `cwd` up to `project_root(cwd)` (when `project` is `true` and `project.files.skills` is `true`).
2. `<AXO_DIR>/skills`.
3. `~/.agents/skills` (when `home_agents_dir` is `true`).

Within each root:
- Direct `*.md` files in the root and recursive subdirectories are scanned in sorted filename order.
- Once a directory contains `SKILL.md`, that file defines the skill and `load_skills` does not recurse deeper into that directory.
- Symlink loops and duplicate files are skipped via `fs::canonicalize`.
- First skill name wins; later skills with the same name are skipped with a `Diagnostic`.

### Frontmatter & validation

`SKILL.md` begins with YAML frontmatter delimited by `---`:

```markdown
---
name: my-skill
description: Short description shown to the model in <available_skills>.
disable-model-invocation: false
---

Skill body with optional $ARGUMENTS, $@, $1, ${@:2}, ${1:-default} placeholders.
```

- `description` is required (missing or empty descriptions skip the skill with a `Diagnostic`).
- `name` defaults to the parent directory name (or file stem). Names longer than 64 characters, not matching `[a-z0-9-]`, containing `--`, or not matching the parent directory name emit a `Diagnostic` but still load.
- `disable-model-invocation: true` omits the skill from the `<skills>` system-prompt section while keeping `/name` user expansion active.

### `/name` expansion (`expand_skill_command`)

When a user prompt starts with `/<skill-name>` followed by whitespace or end-of-string, `expand_skill_command` expands it before saving the submission:

```xml
<skill name="my-skill" location="skill://my-skill">
References are relative to skill://my-skill/.

{substituted body}
</skill>
```

If the skill body contains no `$` argument placeholders (`$ARGUMENTS`, `$@`, `$1`…`$9`, `${@:N}`, `${@:N:L}`, `${N:-default}`) and the user passed arguments after `/<skill-name>`, the arguments are appended after a blank line.
