# pi core: context assembly, resources, built-in tools (v1.3.0)

Paths are relative to `/tmp/pi-src/packages/coding-agent/src/` unless prefixed. `CA` = `core/agent-session.ts`, `RL` = `core/resource-loader.ts`, `SP` = `core/system-prompt.ts`, `T/` = `core/tools/`.

## 1. Concepts

- **System prompt is structured, not a string.** It is built as an ordered map of named *sections* (`preamble`, `tools`, `rules`, `docs`, `addendum`, `project_context`, `skills`, `cwd`, plus extension sections). Every section except `preamble` is wrapped in `<name>…</name>` so later *patches* can name it (SP:52-57, 185-189). The transcript stores it as a `SystemMessage{content:"", sections}`. Later turns emit a diff `SystemMessage` whose `sections` map holds only the changed sections, with `null` meaning removed (SP:214-229, CA:1758-1770). Providers render the leading one with `getSystemMessageText` (content + non-null sections joined by `\n\n`, `/tmp/pi-src/packages/ai/src/utils/text.ts:15-21`). Mid-conversation updates use `renderSystemMessageUpdate` ("Updated system prompt section "x":…", text.ts:28-40).
- **The ResourceLoader** discovers, once per `reload()`, everything that shapes context: context files (AGENTS.md/CLAUDE.md), skills, prompt templates, themes, extensions, SYSTEM.md and APPEND_SYSTEM.md (RL:153-165, 513-678). AgentSession reads these getters when it rebuilds the prompt (CA:1720-1746).
- **Skills** are markdown files with YAML frontmatter. Only a name, description and path are listed in the prompt; the model loads the body itself with `read` (or `bash`). The user can also inline one with `/skill:name` (core/skills.ts:357-389, CA:2211-2235).
- **Prompt templates** are user-side slash macros (`/name args`) with bash-style `$1`/`$@` substitution. They are expanded before the text reaches the agent and are never shown to the model as a list (core/prompt-templates.ts:309-325).
- **Tools** are `ToolDefinition`s: name, description, TypeBox schema, optional `promptSnippet`/`promptGuidelines`, `prepareArguments`, `execute(id, params, signal, onUpdate, ctx)`. The snippet and guidelines feed the `tools` and `rules` prompt sections. Every built-in tool takes a pluggable `*Operations` interface (fs/exec) so it can be redirected (e.g. to SSH) (T/read.ts:48-55, T/bash.ts:73-95, etc.). `wrapToolDefinition` turns a definition into an `AgentTool` for the loop (T/tool-definition-wrapper.ts:8-29).
- **Project trust** gates project-local `.pi/*` resources and ancestor `.agents/skills`, but **not** AGENTS.md/CLAUDE.md context files (core/trust-manager.ts:30-39, 186-208; RL:232-274 has no trust check).

## 2. Types (trimmed)

```ts
// SP:8-37
interface BuildSystemPromptOptions {
  customPrompt?: string;          // replaces default preamble AND drops tools/rules/docs sections
  forceSystemPrompt?: string;     // opaque full replacement (before_agent_start), not stored as sections
  selectedTools?: string[];       // default ["read","bash","edit","write"] (SP:64)
  hiddenTools?: string[];         // selected but not declared to the model
  toolSnippets?: Record<string,string>;
  toolGuidelines?: Record<string,string[]>;
  promptGuidelines?: string[];
  appendSystemPrompt?: string;
  sections?: Record<string,string>; // extension sections; name /^[a-z][a-z0-9_-]*$/, not "preamble" (SP:58,144-148)
  cwd: string;
  contextFiles?: Array<{ path: string; content: string }>;
  skills?: Skill[];
}
type SystemPromptSections = Record<string, string>; // ordered; insertion order = render order

// core/skills.ts:66-81
interface SkillFrontmatter { name?: string; description?: string; "disable-model-invocation"?: boolean; [k: string]: unknown }
interface Skill { name: string; description: string; filePath: string; baseDir: string; sourceInfo: SourceInfo; disableModelInvocation: boolean }

// core/prompt-templates.ts:12-19
interface PromptTemplate { name: string; description: string; argumentHint?: string; content: string; sourceInfo: SourceInfo; filePath: string }

// core/resource-loader.ts:153-165
interface ResourceLoader {
  getSkills(): { skills: Skill[]; diagnostics: ResourceDiagnostic[] };
  getPrompts(): { prompts: PromptTemplate[]; diagnostics };
  getAgentsFiles(): { agentsFiles: Array<{ path: string; content: string }> };
  getSystemPrompt(): string | undefined;
  getAppendSystemPrompt(): string[];
  extendResources(paths): void; reload(opts?): Promise<void>;
  // + getExtensions/getThemes/…Sources (skip)
}

// T/truncate.ts:15-38
interface TruncationResult { content: string; truncated: boolean; truncatedBy: "lines"|"bytes"|null;
  totalLines: number; totalBytes: number; outputLines: number; outputBytes: number;
  lastLinePartial: boolean; firstLineExceedsLimit: boolean; maxLines: number; maxBytes: number }

// Tool details
interface ReadToolDetails { truncation?: TruncationResult }                      // T/read.ts:40-42
interface EditToolDetails { diff: string; patch: string; firstChangedLine?: number } // T/edit.ts:72-79
interface BashToolDetails { truncation?: TruncationResult; fullOutputPath?: string } // T/bash.ts:67-70
interface GrepToolDetails { truncation?; matchLimitReached?: number; linesTruncated?: boolean } // T/grep.ts:43-47
interface FindToolDetails { truncation?; resultLimitReached?: number }            // T/find.ts:43-46
interface LsToolDetails   { truncation?; entryLimitReached?: number }             // T/ls.ts:25-28

// Pluggable backends (one per tool)
interface ReadOperations  { readFile(p): Promise<Buffer>; access(p): Promise<void>; detectImageMimeType?(p): Promise<string|null|undefined> } // T/read.ts:48-55
interface WriteOperations { writeFile(p, s): Promise<void>; mkdir(dir): Promise<void> }                 // T/write.ts:27-32
interface EditOperations  { readFile(p): Promise<Buffer>; writeFile(p, s): Promise<void>; access(p): Promise<void> } // T/edit.ts:85-92
interface BashOperations  { exec(cmd, cwd, { onData(Buffer), signal?, timeout?, env? }): Promise<{ exitCode: number|null }> } // T/bash.ts:73-95
interface GrepOperations  { isDirectory(p): bool; readFile(p): string }  // rg itself is not pluggable (T/grep.ts:53-58)
interface FindOperations  { exists(p): bool; glob(pattern, cwd, { ignore, limit }): string[] } // T/find.ts:52-57
interface LsOperations    { exists; stat(p): { isDirectory() }; readdir(p): string[] }         // T/ls.ts:34-41

// models.json (core/model-config.ts:48-118), credentials (packages/ai/src/auth/types.ts:17-37)
type Credential = { type: "api_key"; key?: string; env?: Record<string,string> }
                | { type: "oauth"; refresh: string; access: string; expires: number };
```

## 3. Behavior

### 3.1 System prompt structure (SP:128-190)

The sections are inserted in this order:

1. **`preamble`** (untagged). Either `customPrompt` (SYSTEM.md or `--system-prompt`) or the default: "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files." (SP:152-156)
2. Only when there is **no** customPrompt:
   - **`tools`**: `- name: snippet` for each *declared* tool (selected minus hidden) that has a snippet, else `(none)`, followed by "In addition to the tools above, you may have access to other custom tools depending on the project." Tools without a snippet are not listed (SP:157-160, CA:1722-1727). Snippets are collapsed to one line (CA:1696-1703).
   - **`rules`**: a deduplicated, trimmed bullet list (SP:88-125). If bash/powershell is present without grep/find/ls, it starts with "Use bash for file operations like ls, rg, find". Then each selected tool's guidelines in tool order, then `promptGuidelines`, then always "Be concise in your responses" and "Show file paths clearly when working with files".
   - **`docs`**: pi's own README/docs/examples paths (SP:162-169). This is pi-specific branding.
3. **`addendum`**: appendSystemPrompt (APPEND_SYSTEM.md / `--append-system-prompt`, multiple sources joined with `\n\n`, CA:1731) (SP:172).
4. **`project_context`**: "Project-specific instructions and guidelines:" followed by `<project_instructions path="…">\ncontent\n</project_instructions>` per file, joined with `\n\n` (SP:79-86, 173).
5. **`skills`**: only if `read` or `bash` is declared (or selected but hidden, which gives the "indirect" wording) and at least one skill is visible (SP:174-181).
6. **`cwd`**: the cwd with backslashes turned into `/` (SP:182).
7. Extension `sections` in insertion order, skipping empty ones (SP:183-185).

- **There is no date/time in pi's prompt.** Only cwd. [INFERENCE: date was removed so the prompt stays cache-stable; grep for date strings in core finds none.]
- With **forceSystemPrompt**, `{content: forced}` has no sections (SP:199-205). AgentSession still diffs the structured sections into the transcript and projects the forced text onto the request head only (CA:1754-1781).
- **Rebuild triggers**: tool set changes, extension resource extension, and reload (CA:1550, 3365). Each request diffs the current sections against the latest transcript system message, so unchanged prompts add nothing (CA:1758-1770).

### 3.2 Context-file discovery (RL:184-274)

- Candidates per directory, **first match wins**: `AGENTS.override.md`, `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD`. Non-files are skipped and a UTF-8 BOM is stripped (RL:184-203).
- Order:
  1. The global file from `agentDir` (`~/.pi/agent/AGENTS.md`, …).
  2. Walk from cwd up to the filesystem root, collecting one file per directory. `unshift` makes the final order **root-most first, cwd last** (RL:248-271).
  3. Paths are deduplicated, so the global file is never repeated.
- Worktree rule: in a linked git worktree nested under its main repo, the main repo's same-named context file is skipped as a duplicate of the worktree's own copy (RL:205-230, 255-260).
- Loaded only at `reload()`. `noContextFiles` / `agentsFilesOverride` hooks exist (RL:646-655). **Not trust-gated.**
- pi does **not** read `.agents/AGENTS.md`; omp does (design.md:84).

### 3.3 SYSTEM.md / APPEND_SYSTEM.md (RL:657-676, 1213-1240)

- Explicit option first. Otherwise `<cwd>/.pi/SYSTEM.md` **if the project is trusted**, else `<agentDir>/SYSTEM.md`. APPEND_SYSTEM.md works the same way.
- `resolvePromptInput`: if the value is an existing path, read the file; otherwise treat it as literal text (RL:167-182).

### 3.4 Skills

- **File format** (core/skills.ts:283-345, utils/frontmatter.ts:11-38):
  - YAML frontmatter between a leading `---` and the next `\n---`. The body is trimmed.
  - `name` defaults to the parent directory name. Validation: ≤64 chars, `[a-z0-9-]`, no leading/trailing hyphen, no `--`. Violations are **warnings; the skill still loads**.
  - `description` is required (the skill is dropped if it is empty) and may be ≤1024 chars (warning only).
  - `disable-model-invocation: true` hides the skill from the prompt; only `/skill:name` can use it.
- **Directory scan** (core/skills.ts:171-281, package-manager.ts:377-454):
  - A directory containing `SKILL.md` is a skill root: load it and stop recursing.
  - Otherwise recurse into subdirectories, skipping dotfiles, `node_modules`, and paths ignored by `.gitignore`/`.ignore`/`.fdignore` (rules accumulate down the tree and are prefixed per directory, core/skills.ts:24-64).
  - Loose `.md` files count as skills at the root only in "pi" dirs, or only below the root in ".agents" dirs (package-manager.ts:434-438).
  - Symlinks are followed, and duplicate real paths are skipped silently.
  - Name collision: first wins, and a `collision` diagnostic is recorded (core/skills.ts:434-461).
- **Locations**, highest precedence first (package-manager.ts:186-204, 2419-2575):
  1. Project: `<cwd>/.pi/skills` and `.agents/skills` in cwd and each ancestor up to the git root. **Trusted projects only** (2466-2468, 2484-2519).
  2. User: `~/.pi/agent/skills` and `~/.agents/skills`.
  3. Packages.
  4. Settings `skills` entries / CLI paths add more. Local entries outrank auto-discovered ones within a scope.
- **Prompt listing** (core/skills.ts:357-389). Exact text:
  ```
  \n\nThe following skills provide specialized instructions for specific tasks.
  Use the read tool to load a skill's file when the task matches its description.   (bash/indirect variants)
  When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.

  <available_skills>
    <skill>
      <name>…</name>
      <description>…</description>
      <location>/abs/SKILL.md</location>
    </skill>
  </available_skills>
  ```
  Values are XML-escaped (`& < > " '`). The result is `.trim()`med by the caller (SP:179).
- **`/skill:name args`**: reads the file, strips frontmatter, and produces `<skill name="N" location="P">\nReferences are relative to BASEDIR.\n\nBODY\n</skill>`, followed by `\n\nargs` if args are given. An unknown skill passes through unchanged. A read error emits an extension error and returns the original text (CA:2211-2235). Controlled by `enableSkillCommands` (default true, core/settings-defaults.ts:35). The transcript parser regex is at CA:177-178.

### 3.5 Prompt templates (core/prompt-templates.ts)

- Sources: `~/.pi/agent/prompts/*.md` and `<cwd>/.pi/prompts/*.md` (project only if trusted), non-recursive, plus explicit paths (226-300; auto-discovery is in package-manager.ts:2521-2575).
- `name` is the file basename without `.md`. `description` comes from frontmatter, else the first non-empty body line cut to 60 chars plus `...` (135-145). `argument-hint` is optional.
- Name collisions: first wins, with a diagnostic (RL:1160-1184).
- Expansion: `^/(\S+)(?:\s+(.*))?$`. Arguments are parsed with quote-aware splitting (`"`/`'`, quoted empty strings kept) (25-60, 309-325).
- Substitution (75-108): `$1…`, `$@`/`$ARGUMENTS`, `${N:-def}`, `${@:-def}`, `${@:N}`, `${@:N:L}`. Substitution is not recursive. Missing positional arguments become `""`.
- Order: `/skill:` expansion runs first, then templates (CA:2129-2134, 2255-2256).

### 3.6 Config dirs and precedence (config.ts:577-656)

- `CONFIG_DIR_NAME = ".pi"`. `agentDir = $PI_CODING_AGENT_DIR || ~/.pi/agent` (585, 604-611).
- `agentDir` layout:

  | Path | Purpose | Ref |
  |---|---|---|
  | `settings.json` | Global settings | — |
  | `models.json` | Custom providers/models | — |
  | `auth.json` | Credentials, mode 0600, proper-lockfile | core/auth-storage.ts:37, 61-130 |
  | `trust.json` | Project trust decisions | core/trust-manager.ts:214 |
  | `bin/` | Auto-downloaded `rg`/`fd`; prepended to the bash PATH | utils/shell.ts:182-194, utils/tools-manager.ts:11, 350 |
  | `sessions/` | Sessions | — |
  | `skills/`, `prompts/`, `themes/`, `extensions/`, `tools/` | Resources | — |
  | `AGENTS.md`, `SYSTEM.md`, `APPEND_SYSTEM.md` | Global prompt inputs | — |

- Project dir `<cwd>/.pi/` holds `settings.json`, `mcp.json`, `extensions`, `skills`, `prompts`, `themes`, `SYSTEM.md`, `APPEND_SYSTEM.md`. **All of these need trust** (core/trust-manager.ts:30-39).
- **Settings**:
  - Effective settings are a deep merge of global then project, so project wins. Nested objects merge recursively; arrays replace.
  - Exception: `defaultTools`, where a list of only `+x`/`-x` entries modifies the inherited list (core/settings-manager.ts:65-147).
  - An untrusted project's settings load as `{}`, and writing project settings throws (366-369, 555-559).
  - Writes are serialized with proper-lockfile.
  - Core-relevant keys (core/settings-schema.ts:209-449): `defaultProvider`, `defaultModel`, `defaultThinkingLevel` (default `medium`, core/defaults.ts:3), `modelThinkingLevels`, `compaction{enabled,reserveTokens:16384,keepRecentTokens:20000}`, `retry{…}`, `steeringMode`/`followUpMode` (`one-at-a-time`), `shellPath`, `shellCommandPrefix`, `defaultProjectTrust` (`ask`), `skills`, `prompts`, `extensions`, `packages`, `enableSkillCommands`, `images{autoResize:true,blockImages:false}`, `enabledModels`, `defaultTools`, `thinkingBudgets`, `sessionDir`, `httpProxy`, `httpIdleTimeoutMs` (defaults in core/settings-defaults.ts:4-67).
- **Project trust resolution** (core/project-trust.ts:47-97), in order:
  1. Explicit override.
  2. No trust-requiring resources present → trusted.
  3. A `project_trust` hook from extensions.
  4. The stored decision for the nearest ancestor in `trust.json`.
  5. The `defaultProjectTrust` setting (`always`/`never`/`ask`).
  6. A UI prompt; with no UI, the answer is false.
  
  Resource loading first runs a bootstrap pass with the project untrusted (to load user extensions that may answer the hook), then reloads with the resolved trust (RL:505-528).
- **models.json** (core/model-config.ts):
  - JSONC (comments stripped), BOM stripped, validated with TypeBox. A missing file means no custom providers; a malformed file means no custom providers plus an error string. **It never throws** (152-201).
  - Shape: `{providers:{[id]:{name?,baseUrl?,apiKey?,api?,oauth?:"radius",headers?,compat?,authHeader?,models?:[{id,name?,api?,baseUrl?,reasoning?=false,thinkingLevelMap?,input?=["text"],inputLimits?,cost?,promptCache?,contextWindow?=128000,maxTokens?=16384,samplingParams?,samplingParamsByThinkingLevel?,headers?,compat?}],modelOverrides?:{[modelId]:partial}}}}`.
- **Config value syntax** for apiKey and header values (core/resolve-config-value.ts:28-151):
  - `!cmd` runs a shell command; stdout is used and cached for the process lifetime.
  - `$NAME` and `${NAME}` interpolate environment variables.
  - `$$` escapes `$`, and `$!` escapes `!`.
  - Anything else is literal.
- **Auth precedence per provider** (core/runtime-credentials.ts:24-28, core/provider-composer.ts:457-479): runtime `--api-key` override, then the stored `auth.json` credential, then models.json/extension `apiKey`, then the built-in provider's own resolution (env var) [INFERENCE: the built-in env lookup lives in pi-ai `inherited.resolve`]. When `authHeader` is true, the resolved key is sent as `Authorization: Bearer …` (provider-composer.ts:381-384).
- **Initial model** (core/model-resolver.ts:537-610):
  1. The first scoped model (`--models` patterns), unless resuming.
  2. Settings `defaultProvider/defaultModel`, if auth is configured.
  3. The first available model matching `defaultModelPerProvider`.
  4. The first available model.
  
  Thinking level: pattern suffix, then `modelThinkingLevels["p/id"]`, then `defaultThinkingLevel`, then `medium`.
- **Model reference syntax**: `provider/id[:thinking]`, case-insensitive. A `provider/` prefix is only a hint (the whole text may be an id on another provider). Fuzzy matching is substring on id/name, preferring aliases over `-YYYYMMDD` ids, then the lexically latest. An invalid `:suffix` gives a warning and the default level (core/model-resolver.ts:120-294).

### 3.7 Built-in tools

**Default-on**: `read`, `bash`, `edit`, `write` (core/settings-manager.ts:84-85, sdk.ts:282-294, CA:3712).
- `grep`, `find` and `ls` are built in but off by default (`createReadOnlyTools` bundles read/grep/find/ls, T/index.ts:178-185). `powershell` is Windows-only.
- The `defaultTools` setting and `--tools` take plain names (replace the list) or `+x`/`-x` (modify it) (core/settings-manager.ts:123-140).
- Built-ins set no `executionMode`, so they run **in parallel** unless the loop is configured sequential (/tmp/pi-src/packages/agent/src/agent-loop.ts:526-530). Same-file mutations are serialized by the mutation queue.
- **Common**:
  - Paths are resolved against `ctx.cwd || cwd` via `resolveToCwd`: strip a leading `@`, normalize unicode spaces to ` `, expand `~`/`~/`, accept `file://` URLs, and resolve relative paths against cwd (T/path-utils.ts:47-49, utils/paths.ts:76-107).
  - **A thrown Error becomes `{content:[{type:"text",text:message}], details:{}}, isError:true`** in the loop. The loop never rejects for tool failures (agent-loop.ts:783, 818-819, 860, 909, 922-926).
  - Abort produces an "Operation aborted" error.
  - `prepareArguments` runs before validation (agent-loop.ts:704-711).

#### read (T/read.ts)

- **Schema**: `{path: string, offset?: number (1-indexed), limit?: number}` (14-18). Snippet "Read file contents"; guideline "Use read to examine files instead of cat or sed." (20-23).
- **Path resolution** (`resolveReadPathAsync`, T/path-utils.ts:85-118): if the exact path is missing, try macOS variants in order: `" AM."`/`" PM."` with U+202F before AM/PM, NFD, `'`→U+2019, then NFD + curly quote. Fall back to the resolved path. Then `access(R_OK)` runs, and its error propagates (e.g. `ENOENT: no such file…`).
- **Image detection** sniffs the first 4100 bytes by magic number (utils/mime.ts:3-34):
  - JPEG (but not `FF D8 FF F7` JPEG-LS), PNG (needs an IHDR; **APNG is rejected** and read as text), GIF87a/89a, RIFF…WEBP, and BMP (header sanity checks).
  - The extension is ignored.
- **Image path** (T/read.ts:133-153, utils/image-process.ts:72-119):
  - png/jpeg/gif/webp pass through; anything else (BMP) is converted to PNG.
  - With `autoResize` (default true), the image must fit 2000×2000 and 4.5MB of base64 (utils/image-resize-core.ts:4-39). The resizer tries PNG and JPEG and keeps the smaller, then lowers JPEG quality (80, 85, 70, 55, 40), then shrinks dimensions down to 1×1. `ctx.model.inputLimits.images.resize` overrides the limits.
  - Output on success: `[text "Read image file [mime]" + hints, image{data b64, mimeType}]`. Possible hints: `[Image converted from X to Y.]` and `[Image: original WxH, displayed at wxh. Multiply coordinates by S to map to original image.]`.
  - If the model lacks image input, the note `[Current model does not support images. The image will be omitted from this request.]` is added (79-84). The image block is still returned; the provider layer drops it.
  - On failure the result is text only: `[Image omitted: could not be converted…]` or `[…could not be resized below the inline image size limit.]`.
- **Text path** (155-206):
  - The buffer is decoded as UTF-8 with **no binary detection**; lines are split on `\n`.
  - `startLine = offset ? max(0, offset-1) : 0`. If `startLine >= lines.length`, it throws `Offset N is beyond end of file (M lines total)`.
  - A user `limit` slices first, then `truncateHead` applies (2000 lines / 50KB).
  - Notices:
    - First line > 50KB: `[Line N is SIZE, exceeds 50.0KB limit. Use bash: sed -n 'Np' PATH | head -c 51200]`
    - Truncated by lines: `\n\n[Showing lines A-B of TOTAL. Use offset=B+1 to continue.]`
    - Truncated by bytes: `… of TOTAL (50.0KB limit). Use offset=…`
    - User limit stopped early: `\n\n[K more lines in file. Use offset=X to continue.]`
  - `details.truncation` is set when truncated. `structuredContent` (for codemode) is a string or an `{type:"image",…}` object (skip).
- **Abort**: checked between awaits; rejects with "Operation aborted" (110-123).

#### write (T/write.ts)

- **Schema**: `{path: string, content: string}` (11-14). Snippet "Create or overwrite files"; guideline "Use write only for new files or complete rewrites."
- Inside `withFileMutationQueue`: `mkdir -p dirname`, then `writeFile(utf-8)`. Abort is checked after each await. **The code deliberately does not reject from the abort listener**, so the queue stays locked until the filesystem operation settles (67-90).
- Returns `Successfully wrote to ${path}` with no details. No atomic rename and no BOM/EOL handling.

#### edit (T/edit.ts + T/edit-diff.ts)

- **Schema**: `{path: string, edits: [{oldText: string, newText: string}]}` (21-41). Each edit is matched against the **original** file, must be unique, and must not overlap another.
- `prepareArguments` repairs common model mistakes (103-134): `edits` sent as a JSON string is parsed; a single object is wrapped in an array; legacy top-level `oldText`/`newText` is appended to `edits`.
- An empty `edits` array throws `Edit tool input is invalid. edits must contain at least one replacement.` (136-141).
- **Flow, inside the mutation queue** (163-212):
  1. `access(R_OK|W_OK)`; failure throws `Could not edit file: PATH. Error code: ENOENT.`
  2. Read the file as UTF-8 and strip the BOM (kept for re-adding).
  3. Detect the EOL from whichever of `\r\n` or `\n` appears first (edit-diff.ts:11-17).
  4. Normalize everything to LF. oldText/newText are LF-normalized too.
  5. Apply the edits, restore the EOL, prepend the BOM, and write.
- **Matching** (`applyEditsToNormalizedContent`, edit-diff.ts:300-361):
  - Empty oldText throws `oldText must not be empty in PATH.` (or `edits[i].oldText…`).
  - Each edit tries an exact `indexOf` first, then fuzzy matching. Fuzzy normalization: NFKC, trim trailing whitespace per line, smart quotes → ASCII, the dashes U+2010–2015 and U+2212 → `-`, and special spaces → ` ` (edit-diff.ts:34-55, 210-251).
  - If **any** edit needed fuzzy matching, **all** edits are re-matched in fuzzy-normalized space. The touched lines are rewritten from the normalized text and untouched lines are copied byte-for-byte from the original (`applyReplacementsPreservingUnchangedLines`, 125-176).
  - Uniqueness is counted in fuzzy space: more than one hit throws `Found N occurrences of the text in PATH. The text must be unique. Please provide more context to make it unique.`
  - Not found: `Could not find the exact text in PATH. The old text must match exactly including all whitespace and newlines.` (or the `edits[i]` variant).
  - Overlap after sorting: `edits[i] and edits[j] overlap in PATH. Merge them into one edit or target disjoint regions.`
  - An identical result throws `No changes made to PATH. The replacement produced identical content…`
  - Replacements are applied in reverse offset order.
- **Result**: `Successfully replaced N block(s) in PATH.` with `details{diff (line-numbered display diff, 4 lines of context), patch (unified, via the jsdiff createTwoFilesPatch), firstChangedLine}` (edit-diff.ts:363-512, T/edit.ts:202-209).

#### bash (T/bash.ts + utils/shell.ts + utils/child-process.ts + T/output-accumulator.ts)

- **Schema**: `{command: string, timeout?: number (seconds, no default)}` (40-43). Snippet "Execute bash commands (ls, grep, find, etc.)"; guideline about `PI_*` env vars (only if `exposeSessionEnvironment`) (45-48, 257).
- **Timeout validation**: the value must be finite and > 0, else `Invalid timeout: must be a finite number of seconds`. The maximum is 2147483.647 s (22-38).
- **Shell resolution** (utils/shell.ts:85-139): `shellPath` setting (missing → throw `Custom shell path not found`), else `/bin/bash`, else `which bash`, else `sh`. Args are `["-c", command]`. Legacy WSL bash.exe receives the command on stdin instead.
- **Environment**:
  - `process.env` with `~/.pi/agent/bin` prepended to PATH (utils/shell.ts:182-194).
  - `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL` and `PI_REASONING_LEVEL` are always deleted first, then set from ctx when exposure is on (188-210).
  - The optional `spawnHook(ctx)` can rewrite command/cwd/env. `commandPrefix` (the `shellCommandPrefix` setting) is prepended with `\n` (268).
- **Spawn** (97-171):
  - Before spawning, check that cwd exists, else `Working directory does not exist: CWD\nCannot execute bash commands.`
  - `spawn(shell, args, {detached: true on unix → own process group, stdio [ignore, pipe, pipe]})`. stdout and stderr are merged into one `onData` stream, in arrival order.
  - The pid is tracked so all children are killed on parent SIGHUP/SIGTERM (utils/shell.ts:207-222).
- **Kill**: `killProcessTree` sends `SIGKILL` to the process group (`kill(-pid)`), falling back to the pid alone (utils/shell.ts:227-251). This runs on abort and on timeout. **There is no SIGTERM grace.**
- **Wait**: after `exit`, wait for both pipes to end, re-arming a 100ms idle timer on every chunk (so a detached grandchild holding the pipe cannot hang the call), then destroy the streams. `close` finalizes immediately (utils/child-process.ts:16, 39-137).
- **Exit code**: `exitCode ?? 128+signal ?? 1` (158).
- **Streaming**:
  - An `OutputAccumulator` decodes UTF-8 incrementally and keeps a rolling tail of about 4×maxBytes (T/output-accumulator.ts:60-63, 197-238).
  - Once output exceeds 50KB or 2000 lines, it spools the **raw** bytes to `$TMPDIR/pi-bash-<16hex>.log` (mode 0600, flag `wx`) (utils/output-files.ts:15-34, T/output-accumulator.ts:240-256).
  - `onUpdate` is called with `{content:[]}` first, then tail snapshots throttled to 100ms (T/renderers/bash.ts:19, T/bash.ts:282-331).
- **Final output** (`truncateTail`: last 2000 lines / 50KB; T/bash.ts:339-363):
  - Empty output becomes `(no output)`.
  - Truncation notices: `\n\n[Showing lines A-B of N. Full output: PATH]`, the `(50.0KB limit)` variant, and `[Showing last X of line N (line is Y). Full output: PATH]` for a partial single line.
  - Non-zero exit: the text gets `\n\nCommand exited with code N`, with `isError: true` and `details` (397-407). This is **returned**, not thrown.
  - Abort: throws `<output>\n\nCommand aborted`. Timeout: throws `<output>\n\nCommand timed out after N seconds` (366-382). A null exit code throws `Command terminated without an exit code`.
  - `structuredContent{output (≤1MB head+tail with "[... N bytes omitted ...]"), truncated, full_output_path?, exit_code, wall_time_seconds}` is for codemode (24, 56-62, 389-396; T/output-accumulator.ts:150-180).
- **Output cleaning**: no ANSI stripping and no sanitizing in the tool path (raw text). The user-`!` path (`core/bash-executor.ts`) strips ANSI and control characters and drops `\r` (bash-executor.ts:74-79).

#### grep (T/grep.ts; off by default)

- **Schema**: `{pattern, path?, glob?, ignoreCase?, literal?, context?, limit? (default 100)}` (21-33).
- `rg` comes from PATH or is downloaded to `~/.pi/agent/bin` (T/grep.ts:119-123, utils/tools-manager.ts:350). A path that does not exist throws `Path not found: P`.
- Args: `rg --no-config --json --line-number --color=never --hidden [--ignore-case] [--fixed-strings] [--glob G] -- PATTERN PATH`. The `.gitignore` rules apply through rg's defaults (162-168).
- rg is killed once `limit` matches arrive. Exit codes other than 0/1 (when not killed for the limit) reject with rg's stderr (232-257).
- Line format: `rel/path:N: text`; context lines use `rel/path-N- text`. Context lines are re-read from the file. Each line is cut at 500 chars with `... [truncated]` (T/truncate.ts:13, 278-286).
- Paths are relative to a directory search root, or the basename when the target is a single file (138-146).
- The output is head-truncated at 50KB with no line cap. Notices joined with `. ` in `[…]`: `100 matches limit reached. Use limit=200 for more, or refine pattern`, `50.0KB limit reached`, `Some lines truncated to 500 chars. Use read tool to see full lines` (278-305).
- No matches gives `No matches found`.

#### find (T/find.ts; off by default)

- **Schema**: `{pattern (glob), path?, limit? (default 1000)}` (26-32).
- Uses `fd --glob --color=never --hidden [--no-require-git if not inside a git repo] --max-results L`. A pattern containing `/` adds `--full-path` and a `**/` prefix (172-211).
- Output paths are relative and use posix separators (13-24).
- Custom `ops.glob` replaces fd entirely and is given the ignore list `**/node_modules/**` and `**/.git/**` (112-170).
- No results gives `No files found matching pattern`. Notices: `N results limit reached. Use limit=2N for more, or refine pattern` and `50.0KB limit reached`.
- A non-zero fd exit is only an error when there is no output.

#### ls (T/ls.ts; off by default)

- **Schema**: `{path?, limit? (default 500)}` (11-14).
- Errors: `Path not found: P`, `Not a directory: P`, `Cannot read directory: msg`.
- Entries are sorted case-insensitively, directories get a `/` suffix, dotfiles are included, and entries that cannot be stat'ed are skipped (84-131).
- An empty directory gives `(empty directory)`. Notices: `500 entries limit reached. Use limit=1000 for more` and `50.0KB limit`.

#### truncate.ts semantics

- `DEFAULT_MAX_LINES = 2000`, `DEFAULT_MAX_BYTES = 51200` (11-12).
- Line counting ignores a trailing `\n` (46-55).
- `truncateHead` never emits partial lines; if the first line alone is larger than maxBytes, it returns `content:""` with `firstLineExceedsLimit` set (77-163).
- `truncateTail` may emit a partial last line, cut at a UTF-8 boundary (`lastLinePartial`) (165-265).
- `formatSize` produces `B`/`KB`/`MB` with 1 decimal place.

#### File mutation queue (T/file-mutation-queue.ts)

- A global `Map<key, Promise>`. The key is `realpath(resolve(p))`, or the resolved path when the file does not exist yet (16-26).
- A global `registrationQueue` serializes key computation and chain insertion, so two calls on aliases of one file cannot race to register (31-47).
- Each operation awaits the previous one for its key, runs, then releases. The map entry is deleted when it is the tail (48-60).
- It is used by write and edit only; bash is not serialized. Different files still run in parallel.

## 4. Skip list

- `docs` prompt section (pi README/docs/examples paths) and the pi-branded preamble wording.
- Themes (`loadThemes`, `themesOverride`, terminal color mode), RL:891-916, 1043-1133.
- `T/renderers/*`, `T/render-utils.ts`, `renderShell`, `BashRenderState`: TUI only.
- `structuredContent`/`outputSchema`/`constrainedSampling` on tools (codemode/QuickJS scripting) [INFERENCE: keep `constrainedSampling` only if axo wants strict JSON-schema tool calls].
- `powershell` tool and Windows shell paths (Git Bash discovery, WSL stdin transport, taskkill, NFD/AM-PM macOS path fixes). Keep the macOS variants only if cheap.
- `tools-manager.ts` auto-download of rg/fd from GitHub. axo should link ripgrep crates or require binaries (see §5).
- `package-manager.ts` (npm/git packages, 2766 lines), extension loader/replaceable/builtin extensions, `HOST_PROVIDED_EXTENSION_PACKAGES` warnings.
- Settings keys for the TUI (`theme`, `terminal`, `editorPaddingX`, `fullscreen*`, `doubleEscapeAction`, `treeFilterMode`, `markdown`, `autocompleteMaxVisible`), telemetry (`enableInstallTelemetry`, `enableAnalytics`, `trackingId`, `deviceId`), `lastChangelogVersion`, `collapseChangelog`, `npmCommand`, `sessionShareDestination`.
- Project-trust UI prompt (`selectProjectTrustOption`). axo has no approvals; trust becomes a config/host decision.
- `forceSystemPrompt` projection machinery and hidden declarations (`prepareLoadout`) until extensions exist; note them as hook points.
- `ModelRegistry` sync facade, virtual models, classifier/image model types, radius OAuth, the `!cmd` config-value cache (maybe keep), Bedrock/Vertex provider zoo.
- `bash-executor.ts` (user `!cmd` in the TUI/RPC), except as a reference if ACP exposes user shell commands.

## 5. Rust port notes

**Prompt**
- `struct SystemPrompt { preamble: String, sections: IndexMap<SectionName, String> }`, rendered as `preamble + "\n\n" + "<name>\n{body}\n</name>"…`.
- `fn diff(prev: &IndexMap<_, String>, cur) -> Option<IndexMap<_, Option<String>>>` mirrors SP:214-229.
- Keep the ordering exactly, and keep cwd as the last core section, for cache stability.
- Crate: `indexmap`.

**ResourceLoader**
- Return one plain struct: `Resources { context_files: Vec<ContextFile>, skills: Vec<Skill>, prompts: Vec<PromptTemplate>, system: Option<String>, append: Vec<String>, diagnostics: Vec<Diagnostic> }`, built by one `load(cwd, agent_dir, trust) -> Resources`.
- Reload rebuilds it whole. No getters.
- Hooks (`*Override` fns) become extension hook points later.
- Diagnostics are an enum `{Warning, Error, Collision{kind,name,winner,loser}}`.

**Context files**
- Same candidate list.
- Axo plans `.agents/` support (design.md:83-89). Adopt pi's walk (global, then root→cwd) plus `.agents/AGENTS.md`.
- [INFERENCE] Stop the walk at the git root or `$HOME` (omp-like) rather than `/`. pi walks to `/`, a conscious difference worth deciding.

**Skills**
- `serde_yaml` (or `serde_norway`/`saphyr`) for frontmatter.
- `ignore` crate (`ignore::gitignore::GitignoreBuilder`, or `WalkBuilder` with `.add_custom_ignore_filename(".fdignore")`) for the `.gitignore`/`.ignore`/`.fdignore` semantics. It is a natural fit and also used by omp's `pi-walker`.
- Deduplicate by `std::fs::canonicalize`.
- The XML listing is a pure function; keep it byte-exact.

**Templates**
- A small hand-written arg parser plus a `regex` substitution with a closure. Port the regex at prompt-templates.ts:79 verbatim.

**Config**
- `dirs`/`etcetera` for home.
- `serde_json` + `json_comments` (or `jsonc-parser`) for models.json, with `schemars`/`serde` validation via `#[serde(deny_unknown_fields)]`?
  - [INFERENCE] pi's TypeBox schema does not set `additionalProperties: false`, so do not deny unknown fields.
- Deep merge: a custom `fn merge(base: Value, over: Value)` with the `defaultTools` special case.
- Auth file: `fs4`/`fd-lock` for locking, created with mode 0600 (`OpenOptionsExt::mode`).
- Axo should choose its own dir names (design.md open question 3). pi's `~/.pi/agent` layout maps to e.g. `~/.axo/`, plus `.agents/` for declarative files.

**Tool trait**
```rust
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> &serde_json::Value;
    fn prompt_snippet(&self) -> Option<&str>;
    fn prompt_guidelines(&self) -> &[&str];
    fn prepare_args(&self, v: Value) -> Value { v }
    async fn execute(&self, call_id, args: Value, cx: ToolCx, cancel: CancellationToken,
                     updates: Option<mpsc::Sender<ToolUpdate>>) -> Result<ToolOutput, ToolError>;
}
```
- `ToolOutput { content: Vec<Content>, details: Option<Value>, is_error: bool }`.
- `Err(e)` becomes an `is_error` text result in the loop, matching pi's throw semantics. Bash's non-zero exit returns `Ok` with `is_error: true`, not `Err`.
- `schemars` derives the schemas.
- The pluggable `*Operations` collapse into axo's planned **URL scheme registry** (design.md:21, 51): read/grep/ls resolve through `trait Resolver`, and `bash` keeps a `trait Exec` for remote backends.

**Truncation**
- Port `truncate_head`/`truncate_tail` on `&str`/`&[u8]`, using `str::is_char_boundary` / `floor_char_boundary` for the UTF-8 cuts.
- These are the critical invariant (never a partial line from head truncation; UTF-8-safe tail). They deserve one small regression test.

**read**
- `tokio::fs`, magic-byte sniffing via `infer` or a port of mime.ts (APNG rejection and BMP checks matter).
- `image` crate for decode/resize/encode (replaces Photon WASM); `base64`.
- Do it on `spawn_blocking`.
- [INFERENCE] Add binary-file detection (NUL bytes); pi lacks it, and dumping binary garbage is a known weak spot.

**edit**: **this is where omp's hashline `pi-edit` replaces pi's edit.**
- Drop the whole `edit.ts` + `edit-diff.ts` matching layer: oldText/newText schema, fuzzy NFKC normalization, overlap and uniqueness errors, `applyReplacementsPreservingUnchangedLines`, and `prepareEditArguments` repairs.
- `read` must then emit hashline-anchored output (line `N#TAG` prefixes), since anchors come from reads. That changes read's text format and its truncation notices (`offset` continuation stays).
- Keep around it:
  - BOM stripping/restoring.
  - EOL detection/restoration. [INFERENCE: check whether pi-edit already handles this.]
  - The file mutation queue.
  - Abort-after-await semantics.
  - `details { diff, patch, first_changed_line }` for hosts (use omp's `pi-diff` or the `similar` crate for unified patches).
- The tool's `promptSnippet`/guidelines must be rewritten for hashline semantics.
- `write` remains pi-style.

**Mutation queue**
- `DashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>` keyed by canonical path, falling back to the absolute path for new files.
- Pitfall: pi's global `registrationQueue` exists because the key computation is async. In Rust, compute the key first and take the lock via `entry().or_default()` atomically. Clean up entries when `Arc::strong_count == 1`.
- Hold the lock across the whole read-modify-write, and **check cancellation only between steps**. Never drop the guard mid-write.

**bash**
- `tokio::process::Command` with `process_group(0)` (unix) so `killpg(SIGKILL)` (`nix::sys::signal::killpg`) kills the tree. Track pids in a global set for shutdown cleanup.
- Merge stdout/stderr with two reader tasks into one `mpsc`.
- Port the post-exit idle-grace logic: after `wait()`, keep reading until EOF or 100ms of idle. This is the real pitfall: a detached grandchild keeps the pipe open.
- `tokio::time::timeout`; error strings matching pi's shapes; spool to `tempfile::Builder::new().prefix("axo-bash-").suffix(".log")` with 0600 once over the limits.
- Consider omp's `pi-shell` (design.md:161) instead of hand-rolling. [INFERENCE: it likely includes a persistent shell/PTY; check that it supports the per-call kill-tree semantics.]
- [INFERENCE] Decide on SIGTERM-then-SIGKILL; pi goes straight to SIGKILL.

**grep/glob**
- Axo's tool list names `grep` and `glob` (design.md:50). In-process `grep-searcher` + `grep-regex` + `ignore::WalkBuilder` (the ripgrep crates), or omp's `pi-walker`. This removes the external rg/fd download.
- Keep pi's output contract: `path:N: text`, `path-N- text` for context, 500-char line cut, limits 100/1000, and the notice strings.
- `--hidden` means `WalkBuilder::hidden(false)`. Respect `.gitignore`, with `require_git(false)` outside repos to match fd's `--no-require-git` behavior.
- `glob` ≈ pi's `find`. Use `globset`, matching on the basename unless the pattern contains `/`, in which case prefix `**/` (T/find.ts:201-209).

**Pitfalls**
- ctx.cwd overrides the creation-time cwd per call.
- Snippets must be one line.
- Rules dedupe on trimmed text.
- Skills are hidden when neither read nor bash is selected.
- An untrusted project means project settings are `{}`, but AGENTS.md is still loaded.

## 6. Size (non-test TS lines)

| File | Lines |
|---|---|
| core/system-prompt.ts | 229 |
| core/resource-loader.ts | 1287 (≈450 relevant: context files, prompt discovery, skills/prompt plumbing; rest is themes/extensions) |
| core/skills.ts | 514 |
| core/prompt-templates.ts | 325 |
| utils/frontmatter.ts | 40 |
| core/defaults.ts | 12 |
| config.ts | 656 (≈60 relevant: dirs) |
| core/settings-manager.ts / settings-schema.ts / settings-defaults.ts | 1443 / 482 / 67 |
| core/project-trust.ts / trust-manager.ts | 97 / 246 |
| core/model-config.ts / model-registry.ts / model-resolver.ts | 219 / 244 / 684 |
| core/model-runtime.ts / provider-composer.ts / resolve-config-value.ts / runtime-credentials.ts | 1049 / 732 / 287 / 52 |
| core/auth-storage.ts | 507 |
| core/package-manager.ts | 2766 (auto-discovery ≈200 relevant) |
| T/read.ts / write.ts / edit.ts / edit-diff.ts | 224 / 97 / 220 / 556 |
| T/bash.ts / output-accumulator.ts / core/bash-executor.ts | 442 / 257 / 146 |
| T/grep.ts / find.ts / ls.ts | 325 / 318 / 175 |
| T/truncate.ts / path-utils.ts / file-mutation-queue.ts / tool-definition-wrapper.ts / index.ts | 314 / 118 / 61 / 58 / 224 |
| utils/shell.ts / child-process.ts / output-files.ts / paths.ts | 251 / 137 / 34 / 140 |
| utils/mime.ts / image-process.ts / image-resize.ts / image-resize-core.ts / image-convert.ts | 116 / 119 / 122 / 174 / 89 |
| T/powershell.ts, T/renderers/* (skip) | 67, ≈1000 |

`packages/env` (pi-env, 3002 lines) is a remote SSH execution daemon for "Pi Durable". **No built-in tool routes through it**: no import of `pi-env` anywhere in coding-agent/src. Remote execution goes only through the per-tool `*Operations` injection.
