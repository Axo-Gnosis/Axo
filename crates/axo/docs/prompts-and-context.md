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
argument-hint: <file>
disable-model-invocation: false
---

Skill body with optional $ARGUMENTS, $@, $1, ${@:2}, ${1:-default} placeholders.
```

- `description` is required (missing or empty descriptions skip the skill with a `Diagnostic`).
- `name` defaults to the parent directory name (or file stem). Names longer than 64 characters, not matching `[a-z0-9-]`, containing `--`, or not matching the parent directory name emit a `Diagnostic` but still load.
- `argument-hint` is optional placeholder text an app shows after `/name` until arguments are typed (`Skill::argument_hint`, trimmed; empty or non-string values become `None`). It is the only skill-argument metadata: a skill takes one free-text argument and has no typed parameters.
- `disable-model-invocation: true` omits the skill from the `<skills>` system-prompt section while keeping `/name` user expansion active.
- A skill named like a [built-in command](#user-commands-cratesaxosrccommandsrs) (`compact`, `shake`, `model`, `thinking`, `navigate`) still loads, but session start logs the warning diagnostic `skill name "<name>" is reserved by the built-in /<name> command; /<name> runs the built-in`, the skill is left out of `Session::commands()`, and `/<name>` runs the built-in.

### `/name` expansion (`expand_skill_command`)

`prompt`, `steer`, and `follow_up` treat a message the same way (`Session::prompt_with` and `Session::queue`):

1. If the first [prompt part](#prompt-parts-cratesaxosrcinputrs) is `Text` starting with `/` and the word before the first whitespace names a built-in command, the [command](#user-commands-cratesaxosrccommandsrs) runs instead; nothing is saved or sent to the model.
2. Otherwise the parts are prepared and rendered into model blocks. Rendering flattens them to one text. Only when the first part is typed `Text` starting with `/<skill-name>` followed by whitespace or end-of-string does `expand_skill_command` turn it into the block below. A message that starts with pasted text (or an empty `Text`) is never a command or a skill, even when its text starts with `/`; it reaches the model verbatim:

```xml
<skill name="my-skill" location="skill://my-skill">
References are relative to skill://my-skill/.

{substituted body}
</skill>
```

A `/word` that names neither a built-in nor a skill stays ordinary prompt text (so `/etc/hosts is broken` reaches the model unchanged).

If the skill body contains no `$` argument placeholders (`$ARGUMENTS`, `$@`, `$1`…`$9`, `${@:N}`, `${@:N:L}`, `${N:-default}`) and the user passed arguments after `/<skill-name>`, the arguments are appended after a blank line.

The submission stores the original parts, not the expansion; the expansion exists only in the user message. Recovery re-renders requeued submissions, so their skills are expanded from the current `SKILL.md` (see [Crash Recovery](sessions-and-compaction.md#crash-recovery-cratesaxosrcrecoveryrs)).

## User Commands (`crates/axo/src/commands.rs`)

`Session::commands()` returns the catalog apps render (`Vec<CommandInfo>`): the five built-ins in the order below, then every skill not shadowed by a built-in name, in skill-discovery order. Skills load once per session, so the catalog does not change while a session is open. Hosts run a built-in either as prompt text (`"/shake images"`, parsed by `Command::parse`) or directly (`Command::decode(name, args)` with JSON arguments, then `Session::run_command`). Built-in commands are never saved as submissions, never sent as user prompts, and never deduplicated by `request_id`; `compact` and `navigate` with `summarize: true` may still make summary requests to the model.

| Command | Policy | Arguments (positional order) | Runs |
| :--- | :--- | :--- | :--- |
| `compact` | `when_idle` | none | `Session::compact()` |
| `shake` | `when_idle` | `mode`: `elide`, `images`, or `thinking` (required) | `Session::shake(mode)` |
| `model` | `alongside` | `model`: `provider/id` (required, string format `model`) | `Models::find`, then `Session::set_model`; an unknown selector is `Error::ModelNotFound` |
| `thinking` | `alongside` | `level`: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max` (required) | `Session::set_thinking(level)` |
| `navigate` | `when_idle` | `entry` (required, string format `entry`), `summarize` (`true`/`false`, default `false`) | `Session::navigate(Some(entry), NavigateOptions { summarize })` |

Each command has one fixed `CommandPolicy`, regardless of its arguments:
- `Alongside` (`"alongside"`): runs during a turn and takes effect from the next request.
- `WhenIdle` (`"when_idle"`): `Error::Busy` while a turn is running.
- `Queued` (`"queued"`): every skill; expanded, then sent through whichever path the app used (`prompt`, `steer`, or `follow_up`).

Running maintenance (compaction, `/shake`, branch summary) makes every built-in return `Error::Busy`, including alongside ones.

`CommandInfo` serializes as camelCase JSON with its kind flattened in:

```json
[
  {"name": "shake", "description": "Move large blocks, images, or thinking out of context into recoverable files", "policy": "when_idle", "kind": "builtin",
   "parameters": {"type": "object", "properties": {"mode": {"description": "What to move out of context into recoverable files: large text blocks (elide), images, or thinking.", "type": "string", "enum": ["elide", "images", "thinking"]}}, "required": ["mode"], "additionalProperties": false}},
  {"name": "review", "description": "Review a file", "policy": "queued", "kind": "skill", "argumentHint": "<file>"}
]
```

- **`parameters`** is a JSON Schema object (draft 2020-12) with every subschema inlined: no `$ref`, `$defs`, `$schema`, or `title`. Unknown properties are rejected.
- **Semantic formats:** a string property with `"format": "model"` takes a `provider/id` selector (hosts can offer their model list); `"format": "entry"` takes a session entry ID (hosts can offer a session-tree picker). There is no bare-ID model form and no path format.
- **Text form:** words after `/name` are split like skill arguments (quotes group words) and map to the schema's properties in order. `boolean` properties accept only `true`/`false`, `integer` properties an `i64`; everything else is a string. Extra words fail with `takes no arguments` or `takes at most {n} arguments`.
- **Errors:** missing, invalid, or extra arguments, or `Command::decode` with an unknown name, return `Error::InvalidCommand { command, message }`, displayed as `/{command}: {message}` (for example `/shake: unknown variant \`bogus\`, expected one of …`). `Value::Null` arguments count as `{}`.

## Prompt Parts (`crates/axo/src/input.rs`)

`prompt`, `steer`, and `follow_up` take `impl Into<PromptInput>` (`PromptInput { parts: Vec<PromptPart> }`; `&str`, `String`, and `&String` convert to one `Text` part, and `Vec<PromptPart>` converts directly); `prompt_with` takes `PromptRequest { input: PromptInput, request_id: Option<String> }`. `PromptPart` serializes with a `type` tag and camelCase names:

| Part | JSON | Meaning | Model sees |
| :--- | :--- | :--- | :--- |
| `Text { text }` | `{"type":"text","text":"…"}` | Typed text | The text |
| `Paste { text }` | `{"type":"paste","text":"…"}` | Pasted text kept inline, marked so apps can redraw it collapsed | The text |
| `File { path }` | `{"type":"file","path":"src/main.rs"}` | A reference to an existing file or directory: relative to cwd, absolute, `~`, or a scheme URL | `@path` only |
| `Content { name, bytes }` | `{"type":"content","name":"shot.png","bytes":"<base64>"}` | Content the user hands over (a paste saved as a file, a file from outside the project, an image). Input only | Never stored; replaced by `Saved` |
| `Saved { url, image }` | `{"type":"saved","url":"local://shot.png","image":true}` | A file axo saved from `Content` | `@url`, plus pixels when `image` |

**`@` is reference only.** Files and saved content reach the model as `@path` (`@"path"` when it contains whitespace) inside the one text block; the model opens them itself with its tools, which strip the leading `@` and the quotes of `@"path"`. File contents are never inlined. Images are the one exception.

**Preparation** runs before anything is written to the log; a failure saves nothing and returns `Error::InvalidInput`. A prompt refused afterwards (`Error::Busy`, a duplicate `request_id`, or a failed write) removes the content it saved; `Busy` and duplicates are checked before any content is saved or rendered.
- `File`: the path must exist (`File not found: {path}`) or name a known scheme; virtual scheme URLs pass unchecked. The part is stored as typed.
- `Content`: `name` must not be empty, `.`, `..`, or contain `/`, `\`, or NUL (`Invalid content name: {name}`). The bytes are written to `<session>/local/{name}` (the `local://` root), never overwriting: on collision axo tries `{stem}-2{.ext}`, `{stem}-3{.ext}`, … (split on the last `.`; dotfiles have no extension). The part becomes `Saved { url: "local://{final name}", image }`, where `image` is whether the bytes sniff as a supported image. IO failures are `Error::Io`.
- `Saved`: must still resolve to an existing `local://` file (`File not found: {url}`), so apps can resubmit parts returned by `Session::fork`. `image` is re-derived from the file's bytes; the value sent is ignored.
- `Text` / `Paste`: unchanged.

**Rendering** produces the user message: one `Text` block (all parts flattened, a skill command expanded when the message starts with typed `/` text), then for each `Saved { image: true }` in order a `Text` block with its URL (plus any resize hints, one per line) and an `Image` block, processed like `read` does for images. When an image cannot be read or processed, that pair becomes one `Text` block `"{url}\n{message}"` (for example `[image unavailable: …]`) and the message still sends. There is no model-capability check here: requests to text-only models turn images into placeholders.

The `submission` entry stores the prepared parts (with `Saved`, never `Content`), so apps can redraw chips after reload or fork; see [JSONL format](sessions-and-compaction.md#jsonl-format-format_version--1).
