# The `.agents/` convention and its neighbors: research for axo

omp = `/nix/store/lb6rqm446k7xw01z0lmp2mjz73hkpl4p-source` (paths below are relative to it). pi = `/tmp/pi-src/packages/coding-agent/src`. Web sources were fetched on 2026-10-22.

## TL;DR

- **`.agents/skills/<name>/SKILL.md` is the one real cross-tool standard.** Codex, Cursor, OpenCode, Gemini CLI, Amp, Devin/Windsurf, pi and omp all scan it at both project and `~/` level. The Agent Skills implementer guide recommends it by name. **Claude Code is the exception**: it says outright that it reads nothing under `.agents/`.
- **`AGENTS.md` (plain file, walked by directory) is the other real standard.** Codex, Amp, OpenCode, Cursor, Windsurf, Gemini (via config) and Claude Code (as a fallback) all read it. **`.agents/AGENTS.md` is omp-only.**
- **Every other kind (commands, rules, subagent definitions, hooks, MCP, workflows) still lives in tool-specific dirs** (`.claude/`, `.codex/`, `.opencode/`, `.cursor/`, `.gemini/`, `.devin/`). The only tool that reads `.agents/{commands,prompts,rules}` is omp. Nobody reads `.agents/agents/`, `.agents/hooks/` or `.agents/mcp.json`, omp included.
- **The trend is to fold commands into skills.** Claude Code merged commands into skills. Codex deprecated custom prompts in favor of skills. Windsurf/Devin is migrating workflows to skills.
- **Toph's real setup:** `~/.agents/skills/` holds skills. His commands (`/cleanup`, `/review:adversarial`, …) and 16 subagent definitions live in `~/.omp/agent/{commands,agents}/`, managed by home-manager, and use omp's agent frontmatter (`tools`, `model: "@advisor"`, `blocking`, `structured`, `output` schema).
- **Two corrections for core.md:**
  - Line 325 says "Roles are data files under `.agents/`", but no tool (omp included) reads agent roles from there.
  - Line 289 puts templates under `.agents/commands/`, which only omp reads; pi uses `.pi/prompts/`.

## 1. What omp discovers from `.agent/` and `.agents/`

**Provider:** `packages/coding-agent/src/discovery/agents.ts`.
- `AGENT_DIR_CANDIDATES = [".agent", ".agents"]` (:30), at priority 70 (:29).
- **Project roots** (:140-155): every ancestor from cwd up to the repo root (or `$HOME` when there is no repo), skipping `$HOME` itself.
- **User roots** (:124-128): `~/.agent/` and `~/.agents/`, plus the Windows profile under WSL.
- **Order:** the nearest project root comes first, `.agent` before `.agents`, and project before user (`docs/slash-command-internals.md:131`).
- `.agents` user sources are on by default, while foreign user dirs (`~/.claude`, `~/.codex`, …) are opt-in (`src/capability/index.ts:357-361`).

| Kind | Path under `.agent[s]/` | Format | How it reaches the model or user | Ref |
|---|---|---|---|---|
| Skills | `skills/<name>/SKILL.md` (one level) | YAML frontmatter `name?`, `description?`, `globs?`, `alwaysApply?`, `hide?`, `disable-model-invocation?`, `enabled?`; other keys kept. The `agents` provider accepts a missing description. | Name and description are listed in the system prompt if a `read`-capable tool exists. The body is read on demand via `skill://name` or `read`. `/skill:name` inlines the body with the skill dir and user args. | agents.ts:158-180; docs/skills.md:50-73, 139-176 |
| Rules | `rules/*.md` and `*.mdc` | Frontmatter `description`, `globs`, `alwaysApply`, `condition` (regex), `astCondition`, `question`, `scope`, `agents`, `interruptMode`, `enabled` | Each rule lands in one of three buckets. **alwaysApply**: the full body goes into the system prompt. **Has description**: listed as `- name (globs): description`, body via `rule://name` (advisory; globs are not enforced). **Has condition**: TTSR, injected when the model's output stream matches. | agents.ts:183-208; docs/rulebook-matching-pipeline.md:95-102, 215-256 |
| Commands | `commands/*.md` (non-recursive) | Optional frontmatter `description` (otherwise the first body line, cut to 60 chars) and `argument-hint`. The body is the template. | User-typed `/name args` is expanded before the model sees it: `$1`, `$@`, `$ARGUMENTS`, `$@[s:l]`. Commands are listed in TUI autocomplete and ACP `availableCommands`. | agents.ts:243-273; docs/slash-command-internals.md:131-147, 223-248 |
| Prompts | `prompts/*.md` | Same as commands (pi's "prompt templates") | Expanded after file commands. Names already taken by commands, skills or built-ins are dropped. | agents.ts:211-240; slash-command-internals.md:176, 213 |
| Context file | `AGENTS.md` | Plain markdown | Joins the project-context section; its depth is that of the directory containing `.agents/`. | agents.ts:276-300; docs/context-files.md:77 |
| System prompt | `SYSTEM.md`, `SYSTEM_TEMPLATE.md` | Plain text, or a template | Replaces the preamble or the whole prompt | agents.ts:303-330 |

**Not in `.agents/` in omp:**
- **Subagent definitions:** only `.omp/agents/*.md` and `~/.omp/agent/agents/*.md`, plus plugin `agents/` dirs. Claude's `.claude/agents` is skipped on purpose because "their frontmatter schema is not the OMP task-agent contract" (`src/task/discovery.ts:1-19, 93-111`).
  - Frontmatter: `name`, `description`, `tools`, `spawns`, `model` (string or list, `@role` selectors), `thinkingLevel`, `blocking`, `prewalk`, `advisor` (`src/task/agents.ts:21-31`).
  - Toph's files also use `structured` and an `output` schema.
- **Hooks:** `.omp/hooks/{pre,post}/*.ts` (TS modules), plus `.claude/hooks` and `.codex/hooks` (`src/discovery/builtin.ts:720`, `claude.ts:383`, `codex.ts:403-408`).
- **MCP:** `.omp/mcp.json`, plus root-level `mcp.json` or `.mcp.json` (`src/discovery/mcp-json.ts:171-173`).
- **Sticky rules:** `RULES.md` in `.omp/` (rulebook doc :78-79).

## 2. What pi discovers

pi is much narrower. The only `.agents/` path it knows is `skills/` (`core/package-manager.ts:481, 2464-2575`).

- **Skills:**
  - Project roots are `<cwd>/.pi/skills` and `.agents/skills` in cwd and every ancestor up to the git root, **for trusted projects only** (`core/trust-manager.ts:186-200`).
  - User roots are `~/.pi/agent/skills` and `~/.agents/skills`.
  - The scan recurses until it finds a `SKILL.md`.
  - Frontmatter: `name`, `description` (required), `disable-model-invocation`.
  - Name rules from the spec produce warnings only.
  - See axo `plan/pi/context.md` §3.4.
- **Prompt templates:** `~/.pi/agent/prompts/*.md` and `<cwd>/.pi/prompts/*.md` (the latter trusted only). **pi does not read `.agents/commands` or `.agents/prompts`** (§3.5).
- **Context files:** `AGENTS.override.md`, `AGENTS.md` or `CLAUDE.md` per directory, walking up to the filesystem root, plus `~/.pi/agent/AGENTS.md`. pi does not read `.agents/AGENTS.md` (§3.2).
- **Not supported:** rules, subagents, hooks-as-files and MCP (the last is in `.pi/mcp.json`, and only through extensions).

## 3. The wider ecosystem

### 3.1 Who reads what

✓ = reads it natively. "own" = the tool reads the same kind of thing from its own directory.

| Tool | `AGENTS.md` | `.agents/skills` (project / `~`) | Commands | Subagent defs | Rules | Hooks | Source |
|---|---|---|---|---|---|---|---|
| Codex | ✓ `AGENTS.override.md` → `AGENTS.md` → fallbacks, root→cwd, 32 KiB cap, `~/.codex` | ✓ / ✓ (cwd→repo root, plus `/etc/codex`) | `~/.codex/prompts/*.md`, **deprecated → skills** | `.codex/agents/*.toml` (`name`, `description`, `developer_instructions`, `model`, …) | — | `.codex/hooks.json` / config.toml | developers.openai.com/codex/{guides/agents-md,skills,custom-prompts,subagents,hooks} |
| Claude Code | fallback only when there is no CLAUDE.md; also `.claude/AGENTS.md`; "**not read: anything under a `.agents/` directory**" | ✗ (`.claude/skills`) | `.claude/commands/*.md`, **merged into skills** | `.claude/agents/*.md` (`name`, `description`, `tools`, `disallowedTools`, `model`, `permissionMode`, `skills`, `mcpServers`, `hooks`, `isolation`, …) | `.claude/rules/*.md` (`paths:` only) | settings.json `hooks` | code.claude.com/docs/en/{memory,skills,sub-agents,hooks} |
| OpenCode | ✓ (walks up; falls back to CLAUDE.md) | ✓ / ✓ (+ `.claude/skills`) | `.opencode/commands/*.md` (`description`, `agent`, `model`, `subtask`; `$ARGUMENTS`, `$1`) | `.opencode/agents/*.md` (`description`, `mode: primary/subagent`, `model`, `temperature`, `permission`) | AGENTS.md + `instructions` globs | plugins (JS) | opencode.ai/docs/{rules,skills,commands,agents} |
| Gemini CLI | via `context.fileName` (default GEMINI.md) | ✓ / ✓ as an alias, outranking `.gemini/skills` | `.gemini/commands/**/*.toml` (`prompt`, `description`, `{{args}}`, `!{sh}`, `@{file}`) | `.gemini/agents/*.md` (`name`, `description`, `kind`, `tools`, `model`, `temperature`, `max_turns`, `timeout_mins`) | — | settings.json `hooks` | geminicli.com/docs/{cli/skills,cli/custom-commands,core/subagents,hooks} |
| Cursor | ✓ | ✓ / ✓ (+ `.claude`/`.codex` skills; nested project dirs) | `.cursor/commands` [INFERENCE: still exists; skills are the push] | `.cursor/agents/`, also `.claude/agents/` and `.codex/agents/` | `.cursor/rules/*.mdc` (`description`, `globs`, `alwaysApply`) | `hooks.json` | cursor.com/docs/context/{skills,rules,subagents}, /agent/hooks |
| Amp | ✓ cwd→`$HOME`, subtrees lazily; `~/.config/amp/AGENTS.md`; AGENT.md/CLAUDE.md fallback | ✓ / ✓ (+ `~/.config/agents/skills`, `.claude/skills`; recursive to depth 5) | — (skills) | — | — | `.amp/plugins` (TS) | ampcode.com/manual/agent-skills.md, docs/…/agents-md, plugins |
| Windsurf / Devin Desktop | ✓ (rules inferred from AGENTS.md) | ✓ / ✓ for "cross-agent compatibility" (+ `.devin/skills`) | Workflows `.devin/workflows/*.md` (legacy `.windsurf/`), **being migrated → skills** | — | `.devin/rules/*.md` (`trigger: always_on / model_decision / glob / manual`) | — | docs.windsurf.com/windsurf/cascade/{skills,workflows,memories} |
| omp | ✓ (+ `.agents/AGENTS.md`) | ✓ / ✓ (+ `.agent/`) | `.agents/commands`, `.agents/prompts`, `.omp`, `.claude`, `.codex`, `.opencode` | `.omp/agents` only | `.agents/rules`, `.omp`, `.cursor`, `.windsurf`, `.cline`, `.github` | `.omp/hooks` (TS) | §1 |
| pi | ✓ | ✓ / ✓ (trust-gated) | `.pi/prompts` | — | — | extensions | §2 |

### 3.2 Which formats are de-facto standard

- **AGENTS.md** is the most universal: "a README for agents", used by 60k+ projects (agents.md).
  - There are no fields; it is plain markdown.
  - The variance is only in the walk: root→cwd (Codex, axo), cwd→`$HOME` (Amp), or including subtrees on read (Amp, Claude).
  - `AGENTS.override.md` is Codex's (pi and axo copy it).
- **SKILL.md** follows the Agent Skills spec (agentskills.io/specification).
  - Frontmatter: `name` (required; ≤64 chars, `[a-z0-9-]`, no edge or double hyphen, **must equal the directory name**), `description` (required, ≤1024 chars), `license`, `compatibility` (≤500), `metadata` (string map), `allowed-tools` (experimental).
  - Optional `scripts/`, `references/` and `assets/` dirs.
  - Progressive disclosure: about 100 tokens of metadata at startup, the body (<5k tokens) on activation, resources as needed.
- **The implementer guide** (agentskills.io/client-implementation/adding-skills-support.md) recommends:
  - **Paths:** scan `.<client>/skills` **and** `.agents/skills` at project and user level (:47-56).
  - **Validation:** be lenient (warn, don't drop).
  - **Collisions:** resolve deterministically, project over user, with a warning (:83-89).
  - **Trust:** "consider gating project-level skills on a trust check" (:91-93).
  - **Catalog:** an `<available_skills>` block with `name`, `description` and `location` (pi's exact shape, :160-184).
  - **Activation:** file-read activation is the simple path (:246); user-explicit activation via `/name` or `$name` (:260-262).
  - **Compaction:** exempt skill content from pruning (:320-327); skip duplicate activations (:329-331).
  - **Hidden skills:** drop `disable-model-invocation` skills from the catalog entirely (:228-230).
- **Commands** have no standard, but a common core: markdown, optional `description` and `argument-hint`, with `$ARGUMENTS`/`$1…`.
  - This core works in Claude, OpenCode, omp, pi and Codex prompts.
  - Gemini uses TOML with `{{args}}`.
  - The industry direction is "a command is a skill the user invokes": Claude Code makes `.claude/commands/x.md` and `.claude/skills/x/SKILL.md` equivalent, and Codex deprecated its prompts.
- **Subagent definitions** have converged on **markdown + YAML frontmatter with `name`, `description`, `tools`, `model`, body = system prompt** (Claude, Gemini, OpenCode, Cursor, omp).
  - Codex is the outlier (TOML with `developer_instructions`).
  - Field semantics differ: Claude tool names (`Read`, `Bash(git:*)`), OpenCode `permission`, omp `@role` models. That is why omp refuses `.claude/agents`.
  - **No `.agents/agents` convention exists.**
- **Rules** are fragmented:
  - Cursor `.mdc`: `description`/`globs`/`alwaysApply`.
  - Windsurf: `trigger`.
  - Claude: `paths`.
  - Copilot: `applyTo`.
  - omp's superset adds TTSR.
- **Hooks** have one shape across Claude, Codex, Gemini and Cursor: a JSON config mapping lifecycle events (PreToolUse/BeforeTool, PostToolUse, SessionStart, Stop, …) to shell commands that exchange JSON on stdio and can block or rewrite. They live in `settings.json` or `hooks.json`, never in `.agents/`. omp and pi use in-process TS modules instead.
- **MCP:** `.mcp.json` (Claude), `.cursor/mcp.json`, `.vscode/mcp.json` and `opencode.json`. No `.agents` variant exists.

## 4. Each kind in plain terms, and what it would give axo

- **Context file (AGENTS.md).** Standing orders, always in the prompt: "how to build/test here, house style."
  - Gives axo: zero-config project knowledge that every other agent already writes. **Already planned.**
- **Skill (SKILL.md folder).** A how-to manual on a shelf. The model sees only its title and blurb, and opens it with `read` when a task matches; the user can force it with `/skill:name`. It can bundle scripts and reference docs.
  - Gives axo: the largest existing library (Toph's `~/.agents/skills`, public skill repos) for about 100 tokens per skill.
  - It needs no new tool: `read` is enough. **Already planned for M3.**
- **Command / prompt template.** A macro the *user* types (`/commit fix the auth bug`). The harness pastes the file with arguments substituted and sends it as the user message. The model never sees a list of commands.
  - Gives axo: Toph's `/cleanup`, `/review:adversarial`, `/commit`, `/pr` workflows, exposed through ACP `availableCommands` to Zed and Axome. **Already planned for M3.**
- **Rule.** A conditional context file: "when touching `*.sql`, follow this."
  - The variants are always-in-prompt, listed-by-description (the model reads it if relevant, much like a skill), or triggered by a pattern in the model's output (omp TTSR, a mid-stream interrupt).
  - Gives axo: little beyond what AGENTS.md plus skills already cover. The only distinct behavior is TTSR, which is a loop feature, not a file format.
- **Agent definition (subagent role).** A named persona for the `task` tool: a system-prompt body plus a tool allowlist, a model or role, and an output schema. The parent model picks one by name and description.
  - Gives axo: the data half of the `task` tool. It is only useful once `task` exists (it is in core.md's "later" table). Toph has 16 of these in omp's format.
- **Workflow.** Not a separate kind anywhere except Windsurf, which is migrating it into skills. In practice it is a command whose body orchestrates agent roles; Toph's `/cleanup` is exactly that.
  - Gives axo: nothing beyond commands plus agent definitions.
- **Hook.** Code run on lifecycle events (before a tool call, after an edit, at session start) that can block or modify.
  - axo already has the `LoopHooks` trait (core.md:171) for hosts.
  - File-declared shell hooks (Claude/Codex/Gemini style) would give non-Rust users a policy point. That is executable project config, so it belongs behind the extension trust gate (core.md:374).
- **MCP config.** A list of external tool servers. It belongs to whichever milestone adds MCP; not a `.agents` concern.

## 5. Risks and gotchas

- **Trust.**
  - axo loads project files without asking (core.md:374).
  - Skills and commands are instructions, not code, so the agentskills guidance and pi's gating are about prompt injection from a cloned repo.
  - That is acceptable for Toph's own repos. A host like Axome may want a `trust_project: bool` on the context struct so it can skip project `.agents/` (cheap to add now). [INFERENCE: no current axo struct for this; check `axo` config types.]
- **Walk direction.** core.md walks context files up to the git root. pi, Codex and omp do the same for `.agents/skills`, so the same root-bounded walk can serve every `.agents/*` kind.
- **`.agent/` singular.** omp reads it; nobody else does (Amp reads `AGENT.md`, the file). Skip it.
- **Name strictness.** The spec requires `name` == directory name. pi warns only, and omp accepts no description in the `agents` provider. Use lenient parsing: drop a skill only if the description is missing.
- **Recursion.** pi and Amp recurse below `skills/` (Amp to depth 5, Cursor nested); omp scans one level. Recursing until the first `SKILL.md` (pi's rule) is the most compatible.
- **Claude Code ignores `.agents/`.** Skills shared with Claude need a symlink (`.claude/skills -> ../.agents/skills`). That is a user-side concern; axo should not read `.claude/` (pi doesn't either).

## Recommendation for axo

**Read only the two standards plus commands from `.agents/`. Keep axo-native state in `~/.axo`. Defer agent definitions to the `task` milestone and leave rules and hooks out.**

| Kind | Read from (precedence high→low) | Accept | Milestone |
|---|---|---|---|
| Context files | As core.md:283 (`AGENTS.override.md` / `AGENTS.md` / `CLAUDE.md` / `.agents/AGENTS.md` per directory, git root→cwd; global `~/.axo/AGENTS.md`) | Plain markdown | **M3** (planned) |
| Skills | Project `.agents/skills` from cwd up to the git root (nearest first), then `~/.axo/skills`, then `~/.agents/skills` | Agent Skills spec, leniently: `name` (defaults to dir name, warn on spec violations), `description` (required, else drop with diagnostic), `disable-model-invocation` (hide from catalog, keep `/skill:`); keep unknown keys; recurse to the first `SKILL.md`; dedupe by realpath; first name wins, with a diagnostic | **M3** (planned). Catalog exactly pi's `<available_skills>`. |
| Commands | Project `.agents/commands/*.md` (nearest ancestor first), then `~/.axo/commands`, then `~/.agents/commands` | pi/omp template format: optional `description` and `argument-hint`, `$1`, `$@`, `$ARGUMENTS`, `${@:N[:L]}`, `${N:-def}`; non-recursive; a `:` in the filename is allowed (Toph's `review:adversarial.md`) | **M3** (planned). Publish via ACP `availableCommands` at M6. |
| `.agents/prompts/` | Same roots, as an alias of commands | Same | M3, about one line: it is omp's second name for the same thing, and pi users call them "prompts". Commands win on name collision. |
| Agent definitions (roles) | `.agents/agents/*.md` and `~/.axo/agents/*.md`, plus `~/.agents/agents` for symmetry | Markdown + frontmatter `name`, `description`, `tools`, `model`, `thinking`, `output` (schema); body = system prompt; tolerate omp keys (`blocking`, `spawns`) as ignored | **With the `task` tool (post-M6).** Not M3. |
| Rules | Not read | — | Later or never. `alwaysApply` duplicates AGENTS.md, description-listed rules duplicate skills, and TTSR is a loop feature to design separately. |
| Hooks (file-declared shell) | Not read | — | With extensions and the trust gate. Format then: the Claude/Codex JSON event→command shape in `.axo/settings.jsonc`, not `.agents/`. |
| MCP | — | — | With MCP support, in `.axo/settings.jsonc` (optionally import `.mcp.json`) |
| SYSTEM.md / APPEND_SYSTEM.md | `~/.axo/`, `<cwd>/.axo/` (core.md:335-336). Do **not** read `.agents/SYSTEM.md` (omp-only; a project silently replacing the whole prompt is a footgun). | Plain text | M3 (planned) |

**Notes on the table.**
- **Agent definitions.** This is where a `.agents/agents` standard is most likely to appear, and it would match the markdown-frontmatter consensus.
- **Migrating Toph's files.** His home-manager config can symlink `~/.omp/agent/agents` → `~/.axo/agents`. omp's `model: "@advisor"` selectors need a mapping or must be ignored, the same way omp ignores Claude model dialects (`task/discovery.ts:138-155`).

**Why this split.**
- `.agents/` holds only **portable, declarative, instruction-only** content that other tools also read (skills, AGENTS.md) or that is plain markdown with an obvious meaning (commands).
- `.axo/` holds axo-specific or executable config (settings, models, auth, sessions, SYSTEM.md, hooks, MCP, and `~/.axo/{skills,commands,agents}` for things Toph wants axo-only).
- This matches how Gemini, Codex and Cursor pair `.<client>/` with `.agents/`, as the agentskills guide recommends.
- It honors core.md's "declarative files: `.agents/`" (core.md:338) without inventing paths nobody else reads.

**Core seams to add now (M3), all small:**
1. **One discovery helper**, `agents_dirs(cwd, git_root, home) -> Vec<(PathBuf, Scope)>`. It walks cwd→git root, appends `~/.axo` and `~/.agents`, and is reused by skills, commands, and later agent definitions. That keeps the walk rule in one place, as omp's `getProjectPathCandidates` does (agents.ts:124-155).
2. **Plain host-buildable structs** `Skill{name, description, path, base_dir, model_invocable}` and `Template{name, description, argument_hint, body, path}` (core.md:342). Every loader returns diagnostics, not errors.
3. **`skill_content` boundary.** Wrap `/skill:` expansions in `<skill name location>` tags (pi already does). M5 compaction can then recognize and protect that text, as the agentskills guide advises.
4. **A `project_resources: bool` (or trust) switch** on the loader input, so hosts like Axome can disable project `.agents/` for untrusted repos before a real trust gate exists.
5. **Fix the two core.md lines** above: line 325, roles are "under `.agents/agents/` (axo convention, no cross-tool standard yet)"; line 289, add `~/.axo/commands` and the `prompts/` alias.

Not recommended: `.agent/` (singular), `.claude/*` or `.codex/*` discovery, `.agents/rules`, `.agents/SYSTEM.md`, or TOML commands/agents. Each adds a format nobody in Toph's workflow writes.
