# Prompts, Context Files, Skills & Agents

`axo` represents the system prompt and tool set as `SystemMessage` entries inside the transcript (`crates/axo/src/context/prompt.rs`). Replaying the transcript reconstructs the exact prompt and tool definitions at any point in a session, and mid-session updates emit a diff `SystemMessage` containing only changed sections (`Some(rendered)`) or removed sections (`None`).

Provider requests use `Transcript::new` with the system prompt and tool declarations already carried by `Message::System` entries. Construction removes failed or aborted assistant turns and their tool results from the request; the persisted session history retains them.

Start-time diagnostics for context files, skills, agent and mode definitions, tool allowlists, schemes and bundled docs, prompt overrides, host sections, and reserved or hidden command names remain available to the host through `session.diagnostics()` (`&[Diagnostic]`) and are still logged as warnings. Runtime `NestedContext` read errors are only logged, not retained in this startup snapshot.

## System Prompt Sections

`build_sections` produces an `IndexMap<String, String>` where each non-empty body is wrapped in `<name>\n{body}\n</name>` in this exact order:

| # | Section | Source |
| :--- | :--- | :--- |
| 1 | `delegation` | Children only: fixed bundled `crates/axo/prompts/DELEGATION.md`; always first, not overridable or removable |
| 2 | `system` | Non-empty selected agent body, else `SYSTEM` override, else bundled `crates/axo/prompts/SYSTEM.md` |
| 3 | `docs` | `"<name>'s documentation, including how to extend it: <name>://README.md"` (`name` defaults to `axo`; materialized from the bundled docs; only when the host set `AxoBuilder::cache_dir`) |
| 4 | `tools` | Deduplicated, trimmed `Tool::prompt_guidelines()` from the direct tools in the prompt shape, formatted as `- {rule}` bullets; deferred-tool promotions do not add guidelines here |
| 5 | `schemes` | URL path guidance and one bullet per scheme's non-empty prompt documentation |
| 6 | `work` | `WORK` override (no bundled default; omitted unless provided) |
| 7 | `prose` | `PROSE` override, else bundled `crates/axo/prompts/PROSE.md` |
| 8 | `project_context` | `<project_instructions path="…">\n{content}\n</project_instructions>` blocks joined by `\n\n` |
| 9 | `skills` | Skill catalog with `skill://<name>` locations when `read` or `bash` is active and at least one skill has `disable_model_invocation == false` |
| 10 | `team` | Team members only: fixed identity, charter, member brief, ordered roster and messaging guidance |
| 11 | `cwd` | `sources.cwd.display()` |
| 12 | `dev_shell` | Trimmed `Resolved::prompt_note` from the active `ExecEnv` (`"Commands run inside the Nix dev shell of {dir}; its tools are already on PATH."`). Direnv and host environments add no note |
| 13 | `date` | Local date formatted as `%Y-%m-%d`, refreshed when a prompt runs on a new day |
| 14 | `<host sections>` | Entries from `SessionOptions::sections`: `Some(body)` inserts or replaces in place, `None` removes the section; not inherited by children |

The fixed child sections are inserted after overrides: `delegation` first, and `team` immediately before `cwd` (at the end if `cwd` is absent). An agent's section switches cannot remove them.

### Bundled documentation (`docs`)

The URLs below use the default harness name, `axo`. `AxoBuilder::name` renames the docs scheme to `<name>://` and names the harness in the `docs` section's text; the section key stays `docs`. Materialization rewrites literal `axo://` URLs in bundled documentation; crate names stay `axo`. Only the bundled `SYSTEM` and `SUMMARY` prompts expand their `{name}` placeholders; host overrides are used verbatim.

When the host sets `AxoBuilder::cache_dir`, constructing a session's schemes (including through `Axo::schemes` for a closed session) materializes the crate's bundled documentation (`README.md`, `configuration.md`, `prompts-and-context.md`, `sessions-and-compaction.md`, `extending.md`) into `<cache_dir>/docs/`, updating files on disk only when their contents differ. Child sessions reuse their parent's loaded skills and docs availability instead of discovering or materializing again. The `docs` section points to `axo://README.md`, which resolves to the bundled entry point in that directory. If that directory cannot be written, scheme construction records a `Diagnostic` (retained by an open session, only logged by `Axo::schemes`) and omits both the `docs` section and the built-in `axo://` scheme. Prompt refreshes only render the already-loaded sources; they perform no filesystem I/O. Without a cache directory, axo writes nothing, omits the `docs` section without a diagnostic, and registers no `axo://` scheme.

### Built-in tool guidelines (`tools`)

Before agent allowlists and depth filtering, the built-in order is `read`, `write`, `edit`, `bash`, `grep`, `find`, `wait`, `kill`, `spawn`, `send`, followed by `ask` when enabled. `spawn` is withheld when the session's depth is at least `subagents.maxDepth` (roots have depth 0). Filtering it after the allowlist avoids an unknown-tool diagnostic for an inherited allowlist naming `spawn`. `kill` and `send` bypass agent allowlists, then presence is derived from the effective tools: `kill` exists when `bash` or `spawn` remains; `send` exists when `spawn` remains or the session has a parent or teammates. Children with an output schema receive `yield` after the allowlist regardless of whether it names that tool. `call` is appended when deferred tools remain, or a root has mode definitions.

Only `read`, `write`, `edit`, and `bash` define `prompt_guidelines()` in the built-in tool set:
- `read`: `"Use read to examine files instead of cat or sed."`
- `write`: `"Use write only for new files or complete rewrites."`
- `edit`: `"When changing several places in one file, use one edit call with several entries in edits[]."`
- `bash`: `"A background job's result arrives automatically as a message; never poll it with read, sleep or repeated commands. Call wait when you have nothing else to do until it finishes."` and `"List jobs with read proc:// and stop one with kill."` (see [Background Jobs](#background-jobs-cratesaxosrcjobsmodrs-cratesaxosrcjobscommandrs))

### Deferred tools

Host tools may be direct (`HostTool::direct`) or deferred (`HostTool::deferred`). Direct tools contribute declarations and guidelines to the initial prompt. Deferred tools remain callable but are omitted from those initial declarations and guidelines. Agent allowlists filter both sets. A deferred name colliding with a direct tool is ignored with a diagnostic; among deferred duplicates, the first wins.

The name `call` is always reserved for the dispatcher: host direct or deferred tools with that name are ignored with diagnostic `host tool "call" is reserved for deferred tools; ignored`, even when no deferred tools or modes remain.

Before a request, changes in deferred names are announced as a persisted, non-display `tool_catalog` custom message with `details.added` and `details.removed` name arrays. Reconciliation folds additions and removals from **all surviving catalog notices in projection order**, rather than using a latest-notice snapshot, then compares that announced set with the current catalog. It lists additions with summaries and removed names, rather than changing the system prompt. Summaries use the first non-blank description line, remove control characters and stop at 200 characters; the announcement explicitly treats summaries as descriptions, not instructions. Changes to descriptions alone do not produce a name-change announcement.

The reserved, read-only `tool://` scheme lists the current deferred catalog, including tools already promoted to direct declarations. An empty catalog reads `No deferred tools.`. `read tool://<name>` returns the full description, guidelines, JSON Schema, and a `call` example; an unknown name reports `No deferred tool named <name>; read tool:// to list them.`. Writes fail with `tool:// is read-only; run a deferred tool with call.`. Reading a named tool also requests promotion; merely listing the catalog does not.

Use `call {"tool":"name","args":{…}}`: `tool` is required and `args` is an optional object defaulting to `{}`. Routing resolves against the effective direct and deferred tools, retaining the original call id while using the target name and arguments for schema validation, hooks, events, execution, cancellation, replay policy and sequential scheduling. A missing target fails with `Tool <name> not found`; `call` is not an allowlist bypass. Dispatch is one hop, not recursive.

Promotion happens at the next request boundary, in documentation-request order, without removing the tool from the deferred catalog. `Model::tool_support()` governs cache-preserving declarations: `TranscriptToolSupport::Changes` permits promotion; `TranscriptToolSupport::Additions` permits it except for `yield`; `TranscriptToolSupport::None` leaves tools callable only through the deferred surface unless already declared. Promotions append tool declarations, not deferred guidelines to the system prompt. Projection changes reconstruct promotions from replayed system declarations intersected with the current catalog. Unmounting a tool removes its promotion; on addition-only models that removal can require resending the tool list rather than preserving the cached prefix.

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
| `runAs` | `root`, `subagent`, or `any` (default); controls whether the definition can be a root persona, a spawned agent, or both. Root-only agents are omitted from the spawn listing and rejected as team members |
| `model` | A [model selector](configuration.md#model-selectors); root sessions consider it after the explicit session selector and recorded model, before `defaultModel`. Children use the [child model cascade](configuration.md#child-sessions-subagents--axosubagentsettings) |
| `tools` | Optional allowlist over built-in **and host** direct/deferred tools, as a list or comma-separated string (CSV entries are trimmed and empty entries dropped; lists are retained as supplied). Omitted keeps all tools; an empty list removes ordinary tools. Unknown tool names produce diagnostics when the agent is selected. Lifecycle tools `kill` and `send` are retained according to the presence rules above, and required `yield` and `call` are added after filtering. `ask` is removed unless listed, and listing it does not enable it when `ask_enabled` is false |
| `contextFiles` | `false` disables all home, user, project, and nested context files, including their name/file diagnostics. A name-to-boolean map overrides `settings.contextFiles`, preserving existing key positions and appending new keys. Omitted or `true` keeps settings for roots and uses `subagents.contextFiles` for children |
| `sections` | Section-name-to-boolean map: `false` removes that section; `true` changes nothing. Names follow the [host section rule](#host-section-validation); invalid names are diagnosed and removed. Host `SessionOptions::sections` can restore removed sections in root sessions (appended after core sections rather than returning to their original position); children's fixed `delegation` section cannot be removed |
| `output` | JSON Schema, compiled for validity at load. Invalid schemas drop the definition. For children, defines the result returned through `yield`; ignored for root sessions |
| `maxTurns` | Model requests per child run before the wrap-up turn; omitted uses `subagents.maxTurns` (default 50). Ignored for root sessions. The tool list stays unchanged; budget enforcement blocks calls through an internal hook, except `yield` |

The trimmed body replaces the `system` section when non-empty; an empty body keeps the configured or bundled `SYSTEM`. Agent bodies are used verbatim, without harness-name substitution, and do not change `WORK`, `PROSE`, or summarizer prompt overrides.

Unreadable files, malformed frontmatter, invalid names or schemas, and collisions produce `Diagnostic` entries and skip the affected definition. Unknown frontmatter keys produce diagnostics but keep an otherwise valid definition; the compatibility keys `blocking`, `spawns`, and `thinkingLevel` are silently ignored. Programmatic specs receive the same name, section, schema, and collision checks, and their bodies are trimmed. File diagnostics carry `DiagnosticOrigin::Path`; programmatic diagnostics have no origin and name the agent in the message.

### Root selection and catalog

`SessionOptions::agent: Option<String>` selects a root persona by name. `None` keeps the session's recorded agent (none for a new session); `Some` selects it, recording a change on reopen. A missing name or a team name returns `Error::AgentNotFound`; an explicit subagent-only selection returns `Error::AgentNotRoot`. The journal stores the name in `agent_change`, not a frozen spec: reopening resolves the current definition again. If the recorded persona is missing or now subagent-only, axo retains a diagnostic and records that the session runs without it.

`SessionOptions::agents: Vec<AgentSpec>` adds definitions supplied in code; the public types live in `axo::context::agents`, and `AgentSpec` and `RunAs` are also re-exported from `axo`. `Session::agents() -> &[AgentDef]` exposes the loaded agent and team catalog. Forks inherit the live persona and catalog. On reopen, pass programmatic definitions and host sections/tools again; these are host inputs, not serialized definitions.

`Axo::catalog(cwd, project_access, agents, modes)` takes the working directory, optional project access, an agent-spec slice and a mode-spec slice. It resolves the same configuration and definitions without creating a session, performs blocking file I/O, and returns `Result<Catalog, Error>` with `Catalog { config, agents, modes, diagnostics }`; modes are shared `Arc<ModeSpec>` values and model roles live in `config.settings.model_roles`.

### Child prompts

`Session::spawn` and the model's `spawn` tool select a catalog agent, an `InlineSpec`, or a plain delegate. A plain delegate keeps the parent's available tools except `ask`, `yield`, `call`, and `enter_mode`, but does not copy the parent's agent body or transcript: an empty child body uses the inherited `SYSTEM` override or bundled default. Inline tool lists must be subsets of the parent's available tools excluding those four tools. Named agents use their own allowlists over built-in and host tools. The child's own surface adds required `yield` and `call` under the normal rules; it does not inherit the parent's dispatcher or mode-entry tool.

A named agent with `runAs: root` fails spawning with `Error::AgentNotSubagent`. Team definitions referencing one are dropped with a root-only-member diagnostic.

Children share the parent's resolved configuration and prompt overrides, agent catalog, host tool factories and host sections, schemes, and tool/spawn hooks. Spawning samples host tools and sections from the parent's live prompt shape; `ask` is always disabled. Context files default to off through `subagents.contextFiles`; an agent can supply its own map or disable them explicitly.

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

Keep working after spawning: each finished run's result arrives automatically, or through `wait` when there is nothing else to do. Read [agent URLs](#agent) for status, results and conversations; use [send](#send) for messages. `spawn` is `Replay::Safe`: recovery adopts every existing child for a repeated call id instead of creating duplicates.

A child with `output` must call `yield` to return its final result; a text answer alone is not a result. With an explicit `"type": "object"` schema, the tool arguments are the result object. Other schemas are wrapped in an object with required `result`, so a string schema takes `{"result":"answer"}` and returns the string itself. Accepted strings are delivered as raw text; other values use compact JSON.

Result reconstruction matches a successful `yield` result to its assistant call by call id. For `call {"tool":"yield","args":{…}}`, it extracts the inner `args` before applying the output schema, not the outer dispatcher arguments.

Arguments are validated before tool hooks or execution. Rejected calls report validation errors for the child to fix. A turn ending without a valid yield gets one `yield_nudge` per run; ending again without yielding reports an error rather than accepting the text. Three rejected yield calls fail the run. Failure reports include the last rejected attempt, or the last text answer when there was none. The internal turn-budget hook always permits `yield`, and its wrap-up notice asks output children to call it with their best final result. `yield` is `Replay::Safe`; its state resets for each run.

### Teams

A file with a `members` key defines a team instead of an agent. Its keys are `name`, `description`, and `members`; its body is the charter. Each member has a required `agent` name, an optional `as` alias, and an optional `brief`. The roster name is the alias when present, otherwise the agent name. Teams must have at least one member, every reference must name an agent (not a team), and roster names must be valid and unique; `parent` is reserved. Invalid teams are diagnosed and dropped after the catalog is loaded. Teams are listed by `Session::agents()` and can be spawned by name, but cannot run as root agents.

For a defined team, call `spawn` with `{"agent":"review-team","task":"check the change"}` or supply `tasks` by roster name. For an ad-hoc team, use `{"members":[{"name":"a","agent":"reviewer"},{"name":"b","spec":{"prompt":"Check correctness."}}],"charter":"Compare findings.","tasks":{"a":"check the API","b":"check behavior"}}`. Each member gets its explicit task or the shared fallback, plus any non-blank context. Team calls reject call-level `model` and `output`; put these in agent files or member specs instead. Nested teams and per-member call overrides are not supported.

The result identifies a per-parent team id and each member's child row in roster order, with details shaped as `{"team":"team_1","jobs":{"a":"agent_1","b":"agent_2"}}`. Each member has its own run and reports its own result to the parent. The fixed `team` section names the member and team, includes the charter and brief, and lists the roster with agent descriptions and a “(you)” marker. Members exchange mail through [send](#send); there is no broadcast. See [wait](#wait) for waiting on teammates, and [Concurrent runs](sessions-and-compaction.md#budgets-and-lifecycle) for first-run slot reservation and team-size limits.

## Mode definitions

Modes add root-session instructions and call-time tool limits without replacing the root persona. Hosts select or end a mode with `Session::set_mode`; the selection is journaled and takes effect for instructions and limits at the next request boundary. Hosts supply `ConfigLayer::mode_roots` and `SessionOptions::modes`; no `.agents/` mode directory is discovered implicitly. Trusted project roots precede user roots, each in supplied order. Project roots require `ProjectAccess::Trust`; at `Read` they are ignored with a warning, and at `Ignore` or with `project.files.config: false` the project layer is not requested.

`load_modes` recursively walks roots in sorted file-name order with the shared skill/agent hidden-file, ignore and symlink rules, loading `*.md` files. Missing roots and files without leading YAML frontmatter are skipped; an empty frontmatter block is valid. Canonical paths are deduplicated. Files precede programmatic specs, and the first mode name wins. Modes have their own namespace, separate from agents and teams.

```markdown
---
name: plan
description: Plan a change.
argument-hint: task
disable-model-invocation: true
model: "@planner"
tools:
  allow: [read, grep]
  deny: [write]
result:
  schema: {type: string}
  review: true
  then: Implement the approved plan.
bash: read-only
---
Produce a concrete plan.
```

| Key | Stored meaning |
| :--- | :--- |
| `name` | Defaults to the file stem when omitted or empty; same strict name rules as agent definitions |
| `description` | Optional listing text, default empty |
| `argument-hint` | Optional string copied into command metadata |
| `disable-model-invocation` | Boolean, default false; true excludes the mode from `enter_mode`, without affecting host selection or command listing |
| `model` | Optional model selector, resolved on selection rather than during definition loading; mode-owned model/thinking changes can be restored on exit |
| `tools` | `ToolLimits`: optional `allow` list and `deny` list (default empty). `ToolLimits::permits` requires membership in `allow` when set, and always rejects names in `deny`; omitted allow permits any name not denied, empty allow permits none |
| `result` | Optional `ModeResult`; when present, `schema` defaults to a string JSON Schema, `review` to false, and `then` to no continuation text. Mounts a deferred `yield` while the mode is effective; review waits for the host before acceptance ends the mode |
| Other keys | Preserved in `ModeSpec::extra` as host policy data, without unknown-key diagnostics |

Bodies are trimmed and file definitions retain their path. Unreadable files, malformed frontmatter, invalid names, invalid result schemas, built-in command names and duplicate mode names produce diagnostics and skip the definition. Programmatic specs receive the same validation and body trimming. File diagnostics have `DiagnosticOrigin::Path`; programmatic diagnostics have no origin and identify the mode. Result schemas are compiled for validity at load time.

`Session::modes()` and `Catalog::modes` expose shared mode definitions. `Session::commands()` lists built-ins first, modes second (`CommandKind::Mode` with `CommandPolicy::Alongside` and the argument hint), then skills not hidden by either. A skill with a mode's name still loads, but is omitted from that command catalog and gets a path diagnostic: `skill name "<name>" is hidden by the mode /<name>; the command catalog lists the mode`. Ordinary prompt skill expansion is unchanged. `Command::parse` / `Command::decode` handle built-ins only; use `Session::parse_command` for mode commands.

### Model entry and results

Roots with at least one mode whose `disable_model_invocation` is false offer deferred `enter_mode`, subject to the root persona's tool allowlist and ordinary direct/deferred name-collision rules. Children never receive this built-in. Its description starts `Switch this session into a mode: instructions, and sometimes tool limits, for one kind of work. Modes:` and appends one `- <name>: <description>` line per model-invocable mode. Its object schema requires `mode`, a string enum of those names, and rejects additional properties. Invoke it with `call {"tool":"enter_mode","args":{"mode":"plan"}}`; it is `Replay::Safe` and returns `Mode "<name>" starts with your next step; its instructions follow.`. Like host selection, the instructions and limits take effect at the next request boundary. The active mode's gate may block `enter_mode`; it is not a lifecycle exemption.

An effective mode with `result` mounts deferred `yield`, even when the root persona's allowlist omits it. It is also exempt from the mode allow/deny gate, but not host policies. Read `tool://yield` for its current schema and use `call {"tool":"yield","args":{…}}`. An explicit `"type":"object"` schema takes the result object directly; every other schema is wrapped in an object with required `result` and no additional properties. For the default string schema, use `{"result":"answer"}`. Validation happens before tool hooks and execution: invalid arguments leave the mode active for correction.

Execution requires that the yield's mode is both selected and effective. A host selection change invalidates the old yield immediately, even while the in-flight batch retains the old mode's ordinary tool limits. Completion clears the mode only if the same definition is still selected, so a concurrent replacement cannot be ended by an older result.

The description is `End the current mode by returning its result. The arguments must match the result schema; if they are rejected, fix the listed errors and call yield again.`; wrapped schemas append ` Put the result in "result".`. Documentation reads promote this changing-schema tool to native declarations only on `TranscriptToolSupport::Changes`; on `Additions` or `None`, keep using `call`. Its deferred registration follows the effective mode at request boundaries and disappears when the effective mode ends. Unlike child output, mode results do not impose a missing-yield nudge or three-rejection run limit.

When any modes are loaded for a root, host direct or deferred tools named `yield` are ignored, even if no mode is active or model-invocable, with diagnostic `host tool "yield" is reserved for modes; ignored`. Roots with modes retain `call` after persona filtering so mode results stay callable. See [Mode result review](sessions-and-compaction.md#mode-result-review) for acceptance, continuation, review, and recovery.

### Mode notices and call-time limits

`Session::mode()` returns the selected mode, including a selection waiting for the next request boundary. Selection alone does not start a model run. A change during an in-flight request leaves that request's tool batch under the previous effective mode. At the next boundary, the selected mode becomes effective and is exposed through `ToolScope::mode` and `SpawnContext::mode`.

Instructions enter context as a persisted `Entry::CustomMessage` of reserved type `mode_notice`, with `display: false` and `details: {"mode": "<name>"}` (null on exit), not as a replacement system section. Before each request, reconciliation compares the effective name with the most recent projected mode notice's name. It does not compare bodies or hashes. If compaction removes the notice, the next request reannounces an active mode; if the latest notice still names the effective mode, no duplicate is appended.

An entry notice contains the trimmed body exactly in this wrapper:

```xml
<mode name="<name>">
<body>
</mode>
```

When switching from an announced mode, it begins with `Mode "<previous>" has ended.` and a newline. An explicit allow list adds `Only these tools run while this mode is active: <comma-separated names>.`; a non-empty deny list adds `These tools do not run while this mode is active: <comma-separated names>.`, each on its own line. The final line is:

```text
This mode stays active until the user ends it.
```

For a definition with `result`, the final line instead reads:

```text
Finish this mode by calling yield with its result: call {"tool": "yield", "args": …}; read tool://yield for the arguments.
```

Ending an announced mode emits `Mode "<name>" has ended; its instructions and tool limits no longer apply.`. Hosts cannot post their own `mode_notice` messages. The `mode` tag belongs to the protected wrapper set: wherever `escape_wrappers` sanitizes untrusted wrapper text, opening and closing mode tags have their `<` changed to `&lt;`.

Mode allow/deny limits gate execution rather than removing ordinary tool declarations or guidelines, preserving the stable prompt/tool prefix for caching. Deny wins over allow, and an empty allow list permits no ordinary tools. The gate checks the routed target name, so deferred `call` dispatch does not bypass it. A blocked call reports `<tool> is unavailable in mode "<name>".` without terminating the run. `yield` is exempt from this gate; host policies may still reject calls. Mode extras remain host policy data, not built-in enforcement.

Children do not inherit the mode body or catalog and cannot select a root mode. Instead, after spawn hooks and replacements, the parent's effective limits intersect the child's tool allowlist; when no child allowlist exists, axo materializes one from the parent's built-in and host direct/deferred tools, excluding `ask` and `yield`, then filters it. That resolved list persists in `SpawnLink::spec`, so later parent mode changes do not relax an existing child and revival retains its limits. Normal child lifecycle-tool presence rules still apply.

See [Root modes](sessions-and-compaction.md#root-modes) for branch/reopen behavior and model restoration.

## Skills (`crates/axo/src/context/skills.rs`)

### Discovery (`load_skills`)

`load_skills` scans roots in this order (child sessions reuse their parent's loaded skills instead of discovering again):
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

Host messages sent with `Session::post(HostMessage { custom_type, text, display, delivery })` are not user skill invocations: their text bypasses skill expansion, including when recovered from the journal. They enter the conversation without changing the system prompt or tools, preserving that stable request prefix for caching. `display` controls client presentation, not model visibility. See [Host messages](extending.md#host-messages) for `Steer`, `FollowUp`, and non-waking `Quiet` delivery.

## User Commands (`crates/axo/src/commands.rs`)

`Session::commands()` returns built-in commands (`compact`, `shake`, `model`, `thinking`, `navigate`), loaded modes, and discovered skills not hidden by built-ins or modes, in that order. `Session::parse_command` tries built-ins first, then recognizes `/<mode-name> [prompt…]` as `Command::Mode { name, prompt }`; the optional prompt is the trimmed remainder, not parsed positional arguments. Unknown names return no command. `Session::run_command` selects the mode, then sends a non-empty prompt through `Session::steer` (starting a run if idle). With no prompt it only selects the mode. Modes use `CommandPolicy::Alongside`, so selection is allowed during a turn; instructions and limits change at the next request boundary. `prompt`, `steer`, and `follow_up` do not parse mode commands themselves. The command catalog, built-in dispatch, policies, JSON Schema argument schemas, and typed command text parsing are documented in [Commands](extending.md#commands).

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
- a steer is pending: the user sent a message while it ran (`ToolContext::steer`);
- the host calls `Session::background(call_id)`.

Environment resolution (dev-shell capture) stays in the foreground; the promotion timer and steer promotion start only once the process runs. A command that already finished is never promoted, and promotion is attempted at most once per call. The tool result then returns at once with the output so far (or `(no output yet)`) followed by:

```text
Command is still running in the background as bg_N. Full output: artifact://<n>. Its result is delivered automatically when it finishes; call wait if you have nothing else to do. Inspect with read proc://bg_N; stop with kill.
```

Every foreground result has `details: {"exit": <typed exit or null>, "durationMs": <milliseconds>}`; a truncated result also names `artifact://<n>` in its notice and adds `"fullOutput": "artifact://<n>"`. Promoted results keep `details: {"job": "bg_N", "fullOutput": "artifact://<n>"}`. Typed exits distinguish exit codes, signals, timeout seconds, model/host kills, foreground aborts and shutdown or reopen interruption (see [host job types](extending.md#background-jobs-for-hosts)). After promotion, Stop (`Session::abort`) and the call's cancellation no longer reach the job; only `kill {"id":"bg_N"}`, `Session::kill_job`, the call's own `timeout` (which keeps its original deadline and has no default), and session shutdown stop it. A model kill reads `Command was stopped (kill bg_N)`; a host kill reads `Command was stopped by the user`; a shutdown reads `Command was interrupted when the session closed`; all have `is_error: true`.

**Retained helpers.** A result is the root shell's: it returns when the root exits plus a fixed 100 ms drain. If processes the command started are still alive in its container (for example backgrounded `&` or `nohup` children, which stay in the process group; under cgroup supervision also `setsid` descendants, which leave the process group and are invisible to process-group mode), the result, foreground or background, ends with `Processes started by this command are still running as bg_N (read proc://bg_N; stop with kill). Their output continues in artifact://<n>.` They stay listed and killable as `bg_N` until they exit; no second result follows.

**Capture loss.** If writing the output spool fails, the command keeps running and the result shows the bounded output already captured, with `[Output capture failed: {err}. Showing partial output only.]` appended and no artifact URL.

### `wait`

`wait` takes no arguments (`{"type":"object","properties":{}}`, `Replay::Safe`, `ExecutionMode::Sequential`). It blocks until a background command (`bg_N`) or subagent (`agent_N`) reports, then returns every pending row message, joined by blank lines with the labels described under [Results in context](#results-in-context). A steer interrupts it with a notice listing running row ids; Stop returns `Operation aborted` (error). It sleeps on registry and team changes rather than polling, and committed result receipts prevent redelivery. Any tool batch containing `wait` runs one call at a time, so an earlier promoted `bash` or `spawn` has committed admission before `wait` checks jobs.

When results are returned, `details: {"jobs": ["bg_1", "agent_2/3", …]}` identifies the individual messages consumed, just like automatic `job_result` delivery. Message text remains in the tool result and corresponding journal entries.

Incoming teammate mail also wakes `wait`. With no row results to return, it responds `New messages arrived; they follow this result.`; the queued mail is delivered immediately after the tool batch, not embedded in that tool result. Outside a team, no running admitted jobs and no mail returns `No background jobs are running.` immediately.

In a team, `wait` keeps waiting while teammates can still reply, even with no jobs of its own. It fails all waiting members with a mutual-wait error when every running member is waiting and nothing can produce a reply: idle or unavailable members count as stuck, while maintenance, pending mail/results, or running admitted jobs prevent that diagnosis. Send the needed message or finish with a result rather than waiting in a cycle.

### `proc://`

The session's read-only `proc://` scheme (`Target::Virtual`) inspects background command jobs and subagents without consuming results. List both with `read proc://`, inspect one with `read proc://bg_N` or `read proc://agent_N`, and stop one with [kill](#kill).

| Operation | Result |
| :--- | :--- |
| `read proc://` | One line per job: `bg_N  {status}  {command first line, max 120 chars}` for commands, or the child row, status, agent name (or “subagent”), and expanded model for children. Team members show “<member> of team_N” in place of the agent name. Status is `queued` (a child waiting for a run slot), `running`, `helpers running`, `finished`, or `failed`; an empty registry returns `No background jobs.` |
| `read proc://bg_N` | `Job:`, `Command:`, `Status:`, `Output: artifact://<n>`, plus `Result:` once finished and until the result is delivered (the delivered text lives in the transcript, not the registry). Unknown id: `No job bg_N`. Unknown path: `Unsupported proc:// path: {path}. Use proc:// or proc://<id>.` |
| `read proc://agent_N` | Child row details: `Job:`, `Agent:`, optional `Team: team_N (member)`, `Model:`, `Session:`, `Status:`, `Details: agent://agent_N`, plus `Result:` for its latest undelivered message |

Reading a finished job's result here does not consume it; its delivery still follows. Promoted jobs, jobs retaining helpers, child sessions, and jobs rebuilt from the journal are listed; ordinary foreground commands leave no entry. `Axo::schemes` (outside an open session) has neither `proc://` nor `agent://`.

### `kill`

`kill` takes a required string `id` and no other properties: `kill {"id":"bg_1"}` or `kill {"id":"agent_1"}`. It stops a listed command and retained helpers, or aborts a child's current run without removing the row or preventing later messages. It returns `Stopping <id>.` when stopped, otherwise `<id> is not running.` (including a valid but unknown id). Invalid ids fail with `<id> is not a job id; use bg_N or agent_N`. The tool uses the default `Replay::Never` policy.

There are no proc action paths: reads of a `/kill` suffix are unsupported, and all proc writes fail with `<url> is read-only`.

### `agent://`

The session's read-only virtual `agent://` scheme reads children spawned by that session; list their ids with `read proc://`. Parent and teammate names are destinations for [send](#send), not URLs exposing their status or transcripts.

| Operation | Result |
| :--- | :--- |
| `read agent://agent_N` | Agent id, name (or “subagent”), optional `Team: team_N (member)`, model, session id, status, and assistant-turn count. Status is `queued` while waiting for a run slot, `running` while holding it, or `finished` / `failed`, as in `proc://`, with no idle label. Shows latest text while running, otherwise the last result, marked partial when its turn budget was exhausted |
| `read agent://agent_N/result` | Full latest completed-run result, even after delivery; errors if no result exists yet. A trailing slash is also accepted |
| `read agent://agent_N/result/<json-pointer>` | Selects from the persisted typed result value when present; otherwise parses the result text as JSON for older or untyped results. Adds the pointer's leading slash; for example `result/findings/0` selects the first finding. Strings return raw text, other values pretty-printed JSON. Missing pointers or non-JSON fallback text are errors. Typed string values remain strings, even if their text happens to look like JSON |
| `read agent://agent_N/transcript` | Projected conversation, including compaction summaries, not the raw journal. System messages are omitted; tool calls are abbreviated to 200 characters and tool results to their first line (up to 200 characters), with a count of additional lines. Page with the read tool's `offset` and `limit` |

Reads never consume pending deliveries. Every agent write fails with `<url> is read-only`. Messaging and stopping are tools, not scheme writes.

### `send`

`send` takes required strings `to` and `message`, plus optional boolean `steer` (default false); no other properties are accepted. Destinations are bare child ids (`agent_N`), `parent`, or a teammate's roster name, never URLs.

| Destination | Behavior |
| :--- | :--- |
| `send {"to":"agent_1","message":"…"}` | Uses `Session::follow_up`, reviving a parked child if needed. An idle child starts a run; an active one queues the message until it would otherwise finish. Returns `Sent to agent_1. Its reply arrives as a new result.` |
| `send {"to":"agent_1","message":"…","steer":true}` | Uses `Session::steer`, reviving a parked child if needed. Read after the current tool calls finish; an idle child starts a run. Returns `Steered agent_1: it reads your message after its current tool calls.` |
| `send {"to":"parent","message":"…"}` | Sends a mid-run note without ending the child's run; returns `Sent to your parent.`. Delivery is at the parent's next turn boundary or through `wait`, never a steer; an idle parent wakes subject to its Stop latch |
| `send {"to":"reviewer","message":"…"}` | Sends durable mail to that teammate in the sender's roster; returns `Sent to reviewer.`. Mail is read at the next turn boundary, wakes `wait`, and starts an idle receiver's run. It never steers or waits for completion |

Blank text fails with `message must not be blank`. Steering a non-child destination fails with `Only your subagents can be steered; send to <to> without steer.`. An unknown child id reports `No agent <id>`. Parent errors distinguish `This session has no parent.`, `Your parent session is not open; nothing was sent.`, and `Your parent no longer tracks this session; nothing was sent.`; roots and forks have no parent route. Self-mail fails with `You are <to>; send to a teammate or to parent.`. Other unknown destinations report `No agent named <to>. Your subagents are agent_N`, followed by the available parent and teammate routes. Non-team children cannot message siblings by name.

`send` uses the default `Replay::Never` policy, so crash recovery does not repeat a potentially completed send. Teammate mail arrives as `Message from <name> (reply with send to "<name>"):` followed by its content, with axo wrapper tags escaped.

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
