# Prompts, Context Files, Skills & Agents

`axo` represents the system prompt and tool set as `SystemMessage` entries inside the transcript (`crates/axo/src/context/prompt.rs`). Replaying the transcript reconstructs the exact prompt and tool definitions at any point in a session, and mid-session updates emit a diff `SystemMessage` containing only changed sections (`Some(rendered)`) or removed sections (`None`).

Provider requests use `Transcript::new` with the system prompt and tool declarations already carried by `Message::System` entries. Construction removes failed or aborted assistant turns and their tool results from the request; the persisted session history retains them.

Start-time diagnostics for context files, skills, agent definitions and tool allowlists, schemes and bundled docs, prompt overrides, host sections, and reserved command names remain available to the host through `session.diagnostics()` (`&[Diagnostic]`) and are still logged as warnings. Runtime `NestedContext` read errors are only logged, not retained in this startup snapshot.

## System Prompt Sections

`build_sections` produces an `IndexMap<String, String>` where each non-empty body is wrapped in `<name>\n{body}\n</name>` in this exact order:

| # | Section | Source |
| :--- | :--- | :--- |
| 1 | `delegation` | Children only: fixed bundled `crates/axo/prompts/DELEGATION.md`; always first, not overridable or removable |
| 2 | `system` | Non-empty selected agent body, else `SYSTEM` override, else bundled `crates/axo/prompts/SYSTEM.md` |
| 3 | `docs` | `"<name>'s documentation, including how to extend it: <name>://README.md"` (`name` defaults to `axo`; materialized from the bundled docs; only when the host set `AxoBuilder::cache_dir`) |
| 4 | `tools` | Deduplicated, trimmed `Tool::prompt_guidelines()` from the active tools, formatted as `- {rule}` bullets |
| 5 | `schemes` | URL path guidance and one bullet per scheme's non-empty prompt documentation |
| 6 | `work` | `WORK` override (no bundled default; omitted unless provided) |
| 7 | `prose` | `PROSE` override, else bundled `crates/axo/prompts/PROSE.md` |
| 8 | `project_context` | `<project_instructions path="…">\n{content}\n</project_instructions>` blocks joined by `\n\n` |
| 9 | `skills` | Skill catalog with `skill://<name>` locations when `read` or `bash` is active and at least one skill has `disable_model_invocation == false` |
| 10 | `team` | Team members only: fixed identity, charter, member brief, ordered roster and messaging guidance |
| 11 | `cwd` | `inputs.cwd.display()` |
| 12 | `dev_shell` | Trimmed `Resolved::prompt_note` from the active `ExecEnv` (`"Commands run inside the Nix dev shell of {dir}; its tools are already on PATH."`). Direnv and host environments add no note |
| 13 | `date` | Local date formatted as `%Y-%m-%d`, refreshed when a prompt runs on a new day |
| 14 | `<host sections>` | Entries from `SessionOptions::sections`: `Some(body)` inserts or replaces in place, `None` removes the section; not inherited by children |

The fixed child sections are inserted after overrides: `delegation` first, and `team` immediately before `cwd` (at the end if `cwd` is absent). An agent's section switches cannot remove them.

### Bundled documentation (`docs`)

The URLs below use the default harness name, `axo`. `AxoBuilder::name` changes the docs scheme and section to the chosen name. Materialization rewrites literal `axo://` URLs in bundled documentation; crate names stay `axo`. Only the bundled `SYSTEM` and `SUMMARY` prompts expand their `{name}` placeholders; host overrides are used verbatim.

When the host sets `AxoBuilder::cache_dir`, constructing a session's schemes (including through `Axo::schemes` for a closed session) materializes the crate's bundled documentation (`README.md`, `configuration.md`, `prompts-and-context.md`, `sessions-and-compaction.md`, `extending.md`) into `<cache_dir>/docs/`, updating files on disk only when their contents differ. The `docs` section points to `axo://README.md`, which resolves to the bundled entry point in that directory. If that directory cannot be written, scheme construction records a `Diagnostic` and omits both the `docs` section and the built-in `axo://` scheme. Prompt refreshes only render the already-loaded sources; they perform no filesystem I/O. Without a cache directory, axo writes nothing, omits the `docs` section without a diagnostic, and registers no `axo://` scheme.

### Built-in tool guidelines (`tools`)

Before agent allowlists and depth filtering, the built-in order is `read`, `write`, `edit`, `bash`, `grep`, `find`, `wait`, `spawn`, followed by `ask` when enabled. `spawn` is withheld when the session's depth is at least `subagents.maxDepth` (roots have depth 0). Filtering it after the allowlist avoids an unknown-tool diagnostic for an inherited allowlist naming `spawn`. Children with an output schema receive `yield` after the allowlist regardless of whether it names that tool.

Only `read`, `write`, `edit`, and `bash` define `prompt_guidelines()` in the built-in tool set:
- `read`: `"Use read to examine files instead of cat or sed."`
- `write`: `"Use write only for new files or complete rewrites."`
- `edit`: `"When changing several places in one file, use one edit call with several entries in edits[]."`
- `bash`: `"A background job's result arrives automatically as a message; never poll it with read, sleep or repeated commands. Call wait when you have nothing else to do until it finishes."` and `"List jobs with read proc:// and stop one with write proc://bg_N/kill (empty content)."` (see [Background Jobs](#background-jobs-cratesaxosrcjobsrs))

### Host section validation

Host section names in `SessionOptions::sections` must start with an ASCII lowercase letter (`a-z`) and contain only `a-z`, `0-9`, `_`, or `-`. Names are validated once at session start; invalid names emit a `Diagnostic` and are skipped. Overriding an existing section name updates its value at its original position in `IndexMap` rather than moving it to the end.

## Prompt Overrides (`EffectiveConfig::prompts`)

axo discovers no prompt-override files of its own (agent definition bodies are separate, below). The host supplies overrides through `ConfigSource` as `ConfigLayer::prompts`, keyed by a `PROMPT_NAMES` entry (`SYSTEM`, `WORK`, `PROSE`, `SUMMARY`, `COMPACTION`, `COMPACTION_UPDATE`, `COMPACTION_SPLIT_TURN`, `BRANCH_SUMMARY`), each a `PromptOverride { origin, text }`:
- `EffectiveConfig::resolve` trims every text and applies the user layer's overrides, then the project layer's when project access is `Read` or `Trust` and the user's `project.files.config` is `true`; the project wins per name. Under `Ignore`, or with `project.files.config: false`, the project layer is never requested.
- A key outside `PROMPT_NAMES` fails session start with `Error::Config` naming that override's `origin`.
- A missing key means no override. An unreadable prompt is the host's error to report from `ConfigSource`; axo never sees a file.
- Overrides are a snapshot: they are sampled once per `create_session` or `open_session` and held for the session's lifetime (later rebuilds of the system prompt, such as a date rollover or a dev-shell note change, reuse that snapshot). A fork inherits its parent's snapshot.

### System-prompt overrides

| Name | Section | Bundled default | Empty / whitespace-only override |
| :--- | :--- | :--- | :--- |
| `SYSTEM` | `system` | `crates/axo/prompts/SYSTEM.md` | Omitted from the prompt |
| `WORK` | `work` | None | Omitted from the prompt |
| `PROSE` | `prose` | `crates/axo/prompts/PROSE.md` | Omitted from the prompt |

### Summarizer prompts (`SummaryPrompts::load`)

Taken from the session's override snapshot when a session starts (`crates/axo/src/compaction/summary.rs`):

| Name | Bundled default | Used for |
| :--- | :--- | :--- |
| `SUMMARY` | `crates/axo/prompts/SUMMARY.md` | System prompt for separate summary requests; prepended to the checkpoint instructions in the user message appended to the live conversation |
| `COMPACTION` | `crates/axo/prompts/COMPACTION.md` | First compaction checkpoint |
| `COMPACTION_UPDATE` | `crates/axo/prompts/COMPACTION_UPDATE.md` | Subsequent compaction checkpoints updating a previous summary |
| `COMPACTION_SPLIT_TURN` | `crates/axo/prompts/COMPACTION_SPLIT_TURN.md` | Summarizing the prefix of a split turn |
| `BRANCH_SUMMARY` | `crates/axo/prompts/BRANCH_SUMMARY.md` | Summarizing an abandoned branch during `Session::navigate` with `summarize: true` |

If a summarizer override is empty or whitespace-only, `SummaryPrompts::load` records a `Diagnostic` whose `origin` is `DiagnosticOrigin::Host` with the override's origin label (`"empty summary prompt ignored; using the bundled prompt"`) and keeps the bundled prompt. Diagnostics about files axo read (context files, skills, the docs directory) carry `DiagnosticOrigin::Path` instead; `origin` is `None` when the problem has no location.

## Context Files (`crates/axo/src/context/files.rs`)

### Startup discovery (`load_context_files`)

1. **User files:** `~/.agents/AGENTS.md` (when `home_agents_dir` is `true`), then the host's `UserConfig::context_files` in order. Missing files are skipped.
2. **Project walk:** When `project` is `true` (`project_access != ProjectAccess::Ignore`), `axo` walks every directory from `project_root(cwd)` (the git root, or `cwd` outside a repository) down to `cwd`.
3. **File selection & ordering (`context_file_names`):**
   - Keys in `settings.context_files` must be non-empty relative paths with only `Component::Normal` segments; absolute paths or paths containing `..` or `.` emit a `Diagnostic` and are skipped.
   - Enabled keys (`true`) are ordered in map order, except any key matching `<stem>.<tag>.md` is placed immediately after `<stem>.md` when `<stem>.md` is also enabled (multi-tag chains such as `A.x.y.md` → `A.x.md` → `A.md` stay adjacent recursively).
4. **Deduplication:** Files are deduplicated by canonical path (`fs::canonicalize`) and by identical content (SHA-256 of the BOM-stripped text), so symlinks or identical copies of `AGENTS.md` and `CLAUDE.md` load only once. UTF-8 BOMs are stripped.

### On-demand nested context files (`NestedContext`)

When project access is not `Ignore`, `settings.nested_context_files` is `true`, and the selected agent has not disabled context files, successful `read`, `write`, and `edit` calls invoke `NestedContext::attach(&path)` on `SessionTools::context`:
- For a touched path inside `project_root`, `NestedContext` checks every directory from `project_root` down to the file's parent directory.
- Any enabled context file whose canonical path and content are not already present in the startup `context_files` or in the active projection's tool results is appended to that tool result as `<project_instructions path="…">\n{content}\n</project_instructions>`.
- `NestedContext::restore(&messages)` reconstructs the seen set from the current projected messages on session open, branch navigation, compaction, and `/shake`. If a tool result carrying a nested context file is elided or compacted away, the next file access in that directory reattaches it.
- Read errors encountered by `NestedContext` at runtime are only logged; they do not appear in `Session::diagnostics`.

## Agent definitions

Hosts supply directories through `ConfigLayer::agent_roots`, not a settings key. At session start, axo scans trusted project roots first, then user roots, each in supplied order. Project roots require `ProjectAccess::Trust`; non-empty roots returned at `Read` are ignored with a warning. At `Ignore`, or with `project.files.config: false`, the project layer is not requested. No agent directory under `.agents/` is discovered implicitly.

Roots are walked recursively in sorted file-name order for `*.md`, following symlinks and respecting the same hidden-file and ignore rules as skills. Missing roots and files without a leading `---` YAML frontmatter block are skipped silently; an empty block is allowed. A canonical file reached twice is loaded once. Agents and teams share a namespace: the first name wins across roots, then `SessionOptions::agents` supplies programmatic `AgentSpec` values after file definitions. Later name collisions produce diagnostics.

### Frontmatter and body

```markdown
---
name: reviewer
description: Read-only code review.
model: "@bard:low"
tools: [read, grep, find]
contextFiles:
  CLAUDE.md: false
sections:
  prose: false
---
Review the code for correctness and report actionable findings.
```

| Key | Meaning |
| :--- | :--- |
| `name` | Defaults to the file stem when omitted or empty. Uses the skill name rule: non-empty, at most 64 characters, only lowercase ASCII letters, digits, and hyphens; no leading, trailing, or consecutive hyphens. Unlike skills, violations drop the definition |
| `description` | Optional listing text, default empty |
| `model` | A [model selector](configuration.md#model-selectors); root sessions consider it after the explicit session selector and recorded model, before `defaultModel`. Children use the [child model cascade](configuration.md#child-sessions-subagents--axosubagentsettings) |
| `tools` | Optional allowlist over built-in **and host** tools, as a list or comma-separated string (trimmed, empty entries dropped). Omitted keeps all tools; an empty list keeps none. Unknown tool names produce diagnostics when the agent is selected. `ask` is removed unless listed, and listing it does not enable it when `ask_enabled` is false |
| `contextFiles` | `false` disables all home, user, project, and nested context files, including their name/file diagnostics. A name-to-boolean map overrides `settings.contextFiles`, preserving existing key positions and appending new keys. Omitted or `true` keeps settings for roots and uses `subagents.contextFiles` for children |
| `sections` | Section-name-to-boolean map: `false` removes that section; `true` changes nothing. Names follow the [host section rule](#host-section-validation); invalid names are diagnosed and removed. Host `SessionOptions::sections` can restore removed sections in root sessions; children's fixed `delegation` section cannot be removed |
| `output` | JSON Schema, compiled for validity at load. Invalid schemas drop the definition. For children, defines the result returned through `yield`; ignored for root sessions |
| `maxTurns` | Model requests per child run before the wrap-up turn; omitted uses `subagents.maxTurns` (default 50). Ignored for root sessions. The tool list stays unchanged; budget enforcement blocks calls through an internal hook, except `yield` |

The trimmed body replaces the `system` section when non-empty; an empty body keeps the configured or bundled `SYSTEM`. Agent bodies are used verbatim, without harness-name substitution, and do not change `WORK`, `PROSE`, or summarizer prompt overrides.

Unreadable files, malformed frontmatter, invalid names or schemas, and collisions produce `Diagnostic` entries and skip the affected definition. Unknown frontmatter keys produce diagnostics but keep an otherwise valid definition; the compatibility keys `blocking`, `spawns`, and `thinkingLevel` are silently ignored. Programmatic specs receive the same name, section, schema, and collision checks, and their bodies are trimmed. File diagnostics carry `DiagnosticOrigin::Path`; programmatic diagnostics have no origin and name the agent in the message.

### Root selection and catalog

`SessionOptions::agent: Option<String>` selects a root agent by name; `None` runs without one. A missing name or a team name returns `Error::AgentNotFound`. `SessionOptions::agents: Vec<AgentSpec>` adds definitions supplied in code; the public types live in `axo::context::agents`. `Session::agents() -> &[AgentDef]` exposes the loaded agent and team catalog. Definitions and the selected root are snapshots: forks inherit them; reopening loads again from the options and roots supplied then, so pass the root name again.

### Child prompts

`Session::spawn` and the model's `spawn` tool select a catalog agent, an `InlineSpec`, or a plain delegate. A plain delegate keeps the parent's available tools except `ask` and `yield`, but does not copy the parent's agent body or transcript: an empty child body uses the inherited `SYSTEM` override or bundled default. Inline tool lists must be subsets of the parent's available tools excluding those two tools. Named agents use their own allowlists over built-in and host tools.

Children share the parent's resolved configuration and prompt overrides, agent catalog, host tool factories, schemes, and tool/spawn hooks. They do **not** inherit host `SessionOptions::sections`, and `ask` is always disabled. Context files default to off through `subagents.contextFiles`; an agent can supply its own map or disable them explicitly.

The fixed `delegation` section tells the child to work autonomously without asking questions and to report its outcome, evidence, and hand-off needs to the parent. It is inserted first after all section overrides, cannot be replaced or removed, and is not a `PROMPT_NAMES` override. The resolved spec, including spawn-hook replacements and child defaults, is persisted in `SpawnLink::spec`; revival reuses it rather than looking up an edited agent definition. See [Spawning child sessions](extending.md#spawning-child-sessions).

### Spawning and returning results

`spawn` starts children in the background and returns their `agent_N` ids immediately, not their final answers. Its description lists defined agents, teams with their roster names, and available model roles. Prefer a defined agent or team; use an inline spec or ad-hoc roster only when none fits. Omit `agent`, `spec` and `members` for a plain delegate. Leave the model unset unless there is a reason to change it.

| Parameter | Meaning |
| :--- | :--- |
| `agent` | Optional catalog agent or team name; mutually exclusive with `spec` and `members` |
| `spec` | Optional inline single-agent object: required `prompt`, optional `tools` allowlist, `model`, and `output` JSON Schema |
| `task` | Required, non-blank assignment for a single agent. For teams, the fallback initial prompt for members without an entry in `tasks`; never a copy of the parent's conversation |
| `tasks` | Teams only: object mapping roster names to their initial assignments. Every member needs a non-blank task; unknown names fail |
| `members` | Ad-hoc team only: ordered array of objects with required `name`, optional `agent` or `spec` (exclusive; neither means a plain delegate), and optional standing `brief` |
| `charter` | Ad-hoc teams only: standing instructions shared by every member; a defined team uses its file body |
| `context` | Optional background appended to each assignment as `\n\nContext:\n{context}` when non-blank |
| `model` | Single agents only: optional model selector, including `@role` and a thinking suffix; overrides the spec's model |
| `output` | Single agents only: optional JSON Schema overriding the agent's own result schema before spawn hooks run |

Keep working after spawning: each finished run's result arrives automatically, or through `wait` when there is nothing else to do. Read [agent URLs](#agent) for status, results, conversations and messaging routes. `spawn` is `Replay::Safe`: recovery adopts every existing child for a repeated call id instead of creating duplicates.

A child with `output` must call `yield` to return its final result; a text answer alone is not a result. With an explicit `"type": "object"` schema, the tool arguments are the result object. Other schemas are wrapped in an object with required `result`, so a string schema takes `{"result":"answer"}` and returns the string itself. Accepted strings are delivered as raw text; other values use compact JSON.

Arguments are validated before tool hooks or execution. Rejected calls report validation errors for the child to fix. A turn ending without a valid yield gets one `yield_nudge` per run; ending again without yielding reports an error rather than accepting the text. Three rejected yield calls fail the run. Failure reports include the last rejected attempt, or the last text answer when there was none. The internal turn-budget hook always permits `yield`, and its wrap-up notice asks output children to call it with their best final result. `yield` is `Replay::Safe`; its state resets for each run.

### Teams

A file with a `members` key defines a team instead of an agent. Its keys are `name`, `description`, and `members`; its body is the charter. Each member has a required `agent` name, an optional `as` alias, and an optional `brief`. The roster name is the alias when present, otherwise the agent name. Teams must have at least one member, every reference must name an agent (not a team), and roster names must be valid and unique; `parent` is reserved. Invalid teams are diagnosed and dropped after the catalog is loaded. Teams are listed by `Session::agents()` and can be spawned by name, but cannot run as root agents.

For a defined team, call `spawn` with `{"agent":"review-team","task":"check the change"}` or supply `tasks` by roster name. For an ad-hoc team, use `{"members":[{"name":"a","agent":"reviewer"},{"name":"b","spec":{"prompt":"Check correctness."}}],"charter":"Compare findings.","tasks":{"a":"check the API","b":"check behavior"}}`. Each member gets its explicit task or the shared fallback, plus any non-blank context. Team calls reject call-level `model` and `output`; put these in agent files or member specs instead. Nested teams and per-member call overrides are not supported.

The result identifies a per-parent team id and each member's child row in roster order, with details shaped as `{"team":"team_1","jobs":{"a":"agent_1","b":"agent_2"}}`. Each member has its own run and reports its own result to the parent. The fixed `team` section names the member and team, includes the charter and brief, and lists the roster with agent descriptions and a “(you)” marker. Members exchange mail through the [agent scheme](#agent); there is no broadcast. See [wait](#wait) for waiting on teammates, and [Concurrent runs](sessions-and-compaction.md#budgets-and-lifecycle) for first-run slot reservation and team-size limits.

## Skills (`crates/axo/src/context/skills.rs`)

### Discovery (`load_skills`)

`load_skills` scans roots in this order:
1. `<dir>/.agents/skills` for each directory from `cwd` up to `project_root(cwd)` (when `project` is `true` and `project.files.skills` is `true`).
2. The host's `UserConfig::skill_roots`, in order.
3. `~/.agents/skills` (when `home_agents_dir` is `true`).

Within each root:
- Every `SKILL.md` under the root is a skill, visited in sorted file-name order. Dot-entries, `node_modules`, and `.gitignore`/`.ignore`/`.fdignore` matches are skipped, and symlinks are followed.
- Once a directory contains `SKILL.md`, that file defines the skill and `load_skills` does not recurse deeper into that directory.
- The same `SKILL.md` reached twice (a symlink or overlapping roots) is skipped silently (compared by canonical path). Symlink loops are skipped by the walker.
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

- `description` is required (missing or empty descriptions skip the skill with a `Diagnostic`). An unreadable file or invalid YAML frontmatter also skips the skill with a `Diagnostic`.
- `name` defaults to the parent directory name. Names longer than 64 characters, empty or outside `[a-z0-9-]`, starting or ending with `-`, or containing `--` emit a `Diagnostic` but still load, and so do descriptions longer than 1024 characters.
- `argument-hint` is optional placeholder text an app shows after `/name` until arguments are typed (`Skill::argument_hint`, trimmed; empty or non-string values become `None`). It is the only skill-argument metadata: a skill takes one free-text argument and has no typed parameters.
- `disable-model-invocation: true` omits the skill from the `<skills>` system-prompt section while keeping `/name` user expansion active.
- A skill named like a [built-in command](#user-commands-cratesaxosrccommandsrs) (`compact`, `shake`, `model`, `thinking`, `navigate`) still loads, but session start logs the warning diagnostic `skill name "<name>" is reserved by the built-in /<name> command; the command catalog lists the built-in`, and the skill is left out of `Session::commands()`. Prompt text `/<name>` still expands the skill; a host that parses typed commands with `Command::parse` first runs the built-in instead.

### `/name` expansion (`expand_skill_command`)

`prompt`, `steer`, and `follow_up` treat a message the same way (`Session::prompt_with` and `Session::queue`). They never run a [built-in command](#user-commands-cratesaxosrccommandsrs): `/model x` passed to them is prompt text like any other.

The parts are prepared and rendered into model blocks. Rendering flattens them to one text. Only when the first [prompt part](#prompt-parts-cratesaxosrcinputrs) is typed `Text` starting with `/<skill-name>` followed by whitespace or end-of-string does `expand_skill_command` turn it into the block below. A message that starts with pasted text (or an empty `Text`) is never a skill, even when its text starts with `/`; it reaches the model verbatim:

```xml
<skill name="my-skill" location="skill://my-skill">
References are relative to skill://my-skill/.

{substituted body}
</skill>
```

A `/word` that names no skill stays ordinary prompt text (so `/etc/hosts is broken` reaches the model unchanged).

If the skill body contains no `$` argument placeholders (`$ARGUMENTS`, `$@`, `$N` for any 1-based digit index, `${@:N}`, `${@:N:L}`, or `${N|@|ARGUMENTS:-default}`) and the user passed arguments after `/<skill-name>`, the arguments are appended after a blank line.

The submission stores the original parts, not the expansion; the expansion exists only in the user message. Recovery re-renders requeued submissions, so their skills are expanded from the current `SKILL.md` (see [Crash Recovery](sessions-and-compaction.md#crash-recovery-cratesaxosrcrecoveryrs)).

## User Commands (`crates/axo/src/commands.rs`)

`Session::commands()` returns the catalog of built-in commands (`compact`, `shake`, `model`, `thinking`, `navigate`) and discovered skills. The command catalog, dispatch via `Session::run_command`, policies, JSON Schema argument schemas, and typed command text parsing are documented in [Commands](extending.md#commands).
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
- `File`: plain paths and file-backed scheme URLs must exist (`File not found: {path}`). Virtual scheme URLs pass unchecked, and an unlocatable URL returns its locate error. The part is stored as typed.
- `Content`: `name` must not be empty, `.`, `..`, or contain `/`, `\`, or NUL (`Invalid content name: {name}`). The bytes are written to `<session>/local/{name}` (the `local://` root), never overwriting: on collision axo tries `{stem}-2{.ext}`, `{stem}-3{.ext}`, … (split on the last `.`; dotfiles have no extension). The part becomes `Saved { url: "local://{final name}", image }`, where `image` is whether the bytes sniff as a supported image. IO failures are `Error::Io`.
- `Saved`: must still resolve to an existing `local://` file (`File not found: {url}`), so apps can resubmit parts returned by `Session::fork`. `image` is re-derived from the file's bytes; the value sent is ignored.
- `Text` / `Paste`: unchanged.

**Rendering** produces the user message: one `Text` block (all parts flattened, a skill command expanded when the message starts with typed `/` text), then for each `Saved { image: true }` in order a `Text` block with its URL (plus any resize hints, one per line) and an `Image` block, processed like `read` does for images. When an image cannot be read or processed, that pair becomes one `Text` block `"{url}\n{message}"` (for example `[image unavailable: …]`) and the message still sends. There is no model-capability check here: requests to text-only models turn images into placeholders.

The `submission` entry stores the prepared parts (with `Saved`, never `Content`), so apps can redraw chips after reload or fork; see [JSONL format](sessions-and-compaction.md#jsonl-format-format_version--1).

## Background Jobs (`crates/axo/src/jobs/mod.rs`, `crates/axo/src/jobs/command.rs`)

Every `bash` command and host command submitted through `SessionTools::run_command` runs in its own container under the session's `ProcessSupervisor`: a process group by default, or a cgroup v2 job directory when the host selects `Supervision::Cgroup` through `AxoBuilder::supervision` (see [Command supervision](configuration.md#command-supervision-axosupervision)). The shared command runner owns execution and output capture; the job registry owns admission and delivery. A command can move to the background as job `bg_N`; its result reaches the model later as a message. How results are journaled and delivered exactly once is in [Background Job Durability](sessions-and-compaction.md#background-job-durability).

### Promotion (`bash` and host commands)

A running command is promoted to a background job when the first of these happens:
- `async: true` is set in `bash` arguments, or a host's `CommandRequest` sets `run_async: true` (immediately);
- it is still running after `bash.promoteAfterMs` milliseconds (default `60000`, or 60 seconds; `0` disables timed promotion);
- a steer is pending: the user sent a message while it ran (`ToolContext::steer`).

Environment resolution (dev-shell capture) stays in the foreground; the promotion timer and steer promotion start only once the process runs. A command that already finished is never promoted, and promotion is attempted at most once per call. The tool result then returns at once with the output so far (or `(no output yet)`) followed by:

```text
Command is still running in the background as bg_N. Full output: artifact://<n>. Its result is delivered automatically when it finishes; call wait if you have nothing else to do. Inspect with read proc://bg_N; stop with write proc://bg_N/kill.
```

A truncated foreground `bash` result names `artifact://<n>` in its notice and has `details: {"fullOutput": "artifact://<n>"}`. Its `details` when promoted are `{"job": "bg_N", "fullOutput": "artifact://<n>"}`. After promotion, Stop (`Session::abort`) and the call's cancellation no longer reach the job; only `write proc://bg_N/kill`, the call's own `timeout` (which keeps its original deadline and has no default), and session shutdown stop it. Stopped by kill, the result reads `Command was stopped (proc://bg_N/kill)` with `is_error: true`.

**Retained helpers.** A result is the root shell's: it returns when the root exits plus a fixed 100 ms drain. If processes the command started are still alive in its container (for example backgrounded `&` or `nohup` children, which stay in the process group; under cgroup supervision also `setsid` descendants, which leave the process group and are invisible to process-group mode), the result, foreground or background, ends with `Processes started by this command are still running as bg_N (read proc://bg_N; stop with write proc://bg_N/kill). Their output continues in artifact://<n>.` They stay listed and killable as `bg_N` until they exit; no second result follows.

**Capture loss.** If writing the output spool fails, the command keeps running and the result shows the bounded output already captured, with `[Output capture failed: {err}. Showing partial output only.]` appended and no artifact URL.

### `wait`

`wait` takes no arguments (`{"type":"object","properties":{}}`, `Replay::Safe`, `ExecutionMode::Sequential`). It blocks until a background command (`bg_N`) or subagent (`agent_N`) reports, then returns every pending row message, joined by blank lines with the labels described under [Results in context](#results-in-context). A steer interrupts it with a notice listing running row ids; Stop returns `Operation aborted` (error). It sleeps on registry and team changes rather than polling, and committed result receipts prevent redelivery. Any tool batch containing `wait` runs one call at a time, so an earlier promoted `bash` or `spawn` has committed admission before `wait` checks jobs.

Incoming teammate mail also wakes `wait`. With no row results to return, it responds `New messages arrived; they follow this result.`; the queued mail is delivered immediately after the tool batch, not embedded in that tool result. Outside a team, no running admitted jobs and no mail returns `No background jobs are running.` immediately.

In a team, `wait` keeps waiting while teammates can still reply, even with no jobs of its own. It fails all waiting members with a mutual-wait error when every running member is waiting and nothing can produce a reply: idle or unavailable members count as stuck, while maintenance, pending mail/results, or running admitted jobs prevent that diagnosis. Send the needed message or finish with a result rather than waiting in a cycle.

### `proc://`

The session's `proc://` scheme (`Target::Virtual`) inspects and stops background command jobs and subagents without consuming results. List both with `read proc://`, inspect one with `read proc://bg_N` or `read proc://agent_N`, and stop either with `write proc://<id>/kill` (empty content):

| Operation | Result |
| :--- | :--- |
| `read proc://` | One line per job: `bg_N  {status}  {command first line, max 120 chars}` for commands, or the child row, status, agent name (or “subagent”), and expanded model for children. Team members show “<member> of team_N” in place of the agent name. Status is `running`, `helpers running`, `finished`, or `failed`; an empty registry returns `No background jobs.` |
| `read proc://bg_N` | `Job:`, `Command:`, `Status:`, `Output: artifact://<n>`, plus `Result:` once finished and until the result is delivered (the delivered text lives in the transcript, not the registry). Unknown id: `No job bg_N`. Unknown path: `Unsupported proc:// path: {path}. Use proc://, proc://<id> or proc://<id>/kill.` |
| `write proc://bg_N/kill` | Stops a listed job (promoted, or retaining helpers) and any retained helpers: `Stopping bg_N.`, or `bg_N is not running.` |
| `read proc://agent_N` | Child row details: `Job:`, `Agent:`, optional `Team: team_N (member)`, `Model:`, `Session:`, `Status:`, `Details: agent://agent_N`, plus `Result:` for its latest undelivered message |
| `write proc://agent_N/kill` | Aborts the running child session; does not remove its row or prevent later prompts or follow-ups |

Reading a finished job's result here does not consume it; its delivery still follows. Promoted jobs, jobs retaining helpers, child sessions, and jobs rebuilt from the journal are listed; ordinary foreground commands leave no entry. `Axo::schemes` (outside an open session) has neither `proc://` nor `agent://`.

### `agent://`

The session's virtual `agent://` scheme reads children spawned by that session; list their ids with `read proc://`. Children can also write to their parent, and team members can write to their own teammates by roster name. These named routes do not expose other sessions' status or transcripts.

| Operation | Result |
| :--- | :--- |
| `read agent://agent_N` | Agent id, name (or “subagent”), optional `Team: team_N (member)`, model, session id, status, and assistant-turn count. Status is `running`, `finished`, or `failed`, as in `proc://`, with no idle label. Shows latest text while running, otherwise the last result, marked partial when its turn budget was exhausted |
| `read agent://agent_N/result` | Full latest completed-run result, even after delivery; errors if no result exists yet. A trailing slash is also accepted |
| `read agent://agent_N/result/<json-pointer>` | Parses the result as JSON and selects the pointer after adding its leading slash; for example `result/findings/0` selects the first finding. Strings return raw text, other values pretty-printed JSON. Missing pointers or non-JSON results are errors |
| `read agent://agent_N/transcript` | Projected conversation, including compaction summaries, not the raw journal. System messages are omitted; tool calls are abbreviated to 200 characters and tool results to their first line (up to 200 characters), with a count of additional lines. Page with the read tool's `offset` and `limit` |
| `write agent://agent_N` | Sends non-blank content through `Session::follow_up`, reviving a parked child if needed. An idle child starts a run; an active one queues the message until it would otherwise finish. Returns `Sent to agent_N. Its reply arrives as a new result.` |
| `write agent://agent_N/steer` | Sends non-blank content through `Session::steer`, reviving a parked child if needed. Read after the current tool calls finish; an idle child starts a run. Returns `Steered agent_N: it reads your message after its current tool calls.` |
| `write agent://parent` | Sends a mid-run note without ending the child's run; returns `Sent to your parent.`. Delivery is at the parent's next turn boundary or through `wait`, never a steer; an idle parent wakes subject to its Stop latch. Roots and forks have no parent route. A closed parent or one no longer tracking the child is an error, not a successful send |
| `write agent://<name>` | Sends durable mail to a teammate in this session's roster; returns `Sent to <name>.`. Mail is read at the next turn boundary, wakes `wait`, and starts an idle receiver's run. It never steers or waits for the receiver to finish its run. Self-mail, names outside the roster and sibling access from non-team children are errors |

Result and transcript paths are read-only; `/steer`, `parent` and teammate paths are write-only. Reads never consume pending deliveries. Writes are actions with `Replay::Never`, so crash recovery never repeats a potentially completed send. Teammate mail arrives as `Message from <name> (reply with write agent://<name>):` followed by its content, with axo wrapper tags escaped.

### Results in context

A result not consumed by `wait` reaches the model as a `job_result` custom message, lowered to a user message:

```text
Background job results:

[bg_N] <command>
<result>
```

Child messages use the same envelope, with the child row in brackets followed by its catalog agent name (or “subagent”); team rows use “<member> of team_N”. Mid-run notes prefix that label with “message from”. Budget-limited results add “(partial: turn budget exhausted)”. A note does not change the child's run status. See [Child sessions](sessions-and-compaction.md#child-sessions) for durability.

Child delivery text is capped at 2000 lines or 50 KB, with an `agent://agent_N/result` pointer for the full result. An oversized first line is replaced by a notice suggesting that URL or a JSON pointer. The pointer always names the latest result, even if the capped delivery was an older run. Both command and child deliveries escape axo's fixed wrapper tags (such as `<system>` and `<conversation>`) by changing their opening `<` to `&lt;`; arbitrary host section names are not covered. The journal's `job_end` retains the full, unescaped text.

During a run, automatic row-message delivery occurs at the next turn boundary after the current tool batch; it never counts as a steer or promotes `bash`. A pending row message satisfies `wait` through its result reservation instead. With the session idle, it starts a run of its own unless automatic continuation is latched off by a Stop or an unsuccessful run. A new `prompt` places pending results before the user's message.
