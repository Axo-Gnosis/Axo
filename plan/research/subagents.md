# Subagents: prior art for Track 2

Study made on 2026-11-03 for the Track 2 design discussion (`task` tool, `AgentSpec`, child sessions). Every claim below was read from source or official docs by six research passes; file references are to the versions named here. Claims marked [INFERENCE] were not verified directly.

| Source | Version read | Where |
|---|---|---|
| omp (oh-my-pi) | 18.15.0 | `/nix/store/lml8bzv4vy2k9fxfizqxfv95fldi5f5b-source/packages/coding-agent/src/` (`OMP/` below) |
| dsh (DeepSeek harness, `deepseek-ai/deepseek-harness`) | 0.2.0-rc.2 | installed `@deepseek-ai/dsh-*` packages (`lib/*.js`, `lib/types/*.d.ts`, package READMEs) |
| Codex CLI | `openai/codex` main, ~0.166 | `codex-rs/core/src/agent/`, `tools/handlers/multi_agents_*`, app-server protocol; developers.openai.com/codex/{subagents,hooks} |
| Claude Code + Agent SDK | docs as of 2026-11-03 | code.claude.com/docs/en/{sub-agents,agent-teams,hooks}, platform.claude.com/docs/en/agent-sdk/*; Toph's `~/.claude/projects/.../subagents/` |
| pi | `earendil-works/pi` main | `packages/coding-agent/examples/extensions/subagent/` (core has no subagent tool) |
| OpenCode | v1.19.3 | `packages/opencode/src/tool/task.ts`, opencode.ai/docs/agents |
| Gemini CLI | docs | geminicli.com/docs/core/subagents |
| Amp | docs | ampcode.com/docs/{models-and-subagents,plugin-api,orbs/agent-to-agent} |
| ACP | RFD "subagents" | agentclientprotocol.com/rfds/subagents |

## 1. What subagents are for

Every source states the same three purposes, in this order:

1. **Context isolation.** Side work that would flood the main context (search results, logs, test output) runs in its own window and returns only a summary. Codex calls the failure mode "context pollution / context rot"; Claude: "the subagent does that work in its own context and returns only the summary".
2. **Constraint and specialization.** A role with a narrower tool set, a different prompt, a cheaper or stronger model (omp `scout` read-only with read-summarization off; Claude `Explore`; Codex `explorer`; Gemini `codebase_investigator`).
3. **Parallelism**, explicitly recommended for read-heavy work and discouraged for parallel writes (Codex docs; omp `parallel.ts`; Claude "Run parallel research").

Beyond these, some sources add: **worktree isolation** for risky edits (omp `isolated`, Claude `isolation: worktree`), **historical-session consulting** (omp `seance` forks an old transcript read-only), and **peer teams** (Claude agent teams, dsh agent-team experiment, Codex V2 mailboxes), which all three treat as a separate, experimental layer on top of plain subagents.

## 2. Comparison

| | omp | dsh | Codex | Claude Code | pi (ext.) | OpenCode | Gemini | Amp |
|---|---|---|---|---|---|---|---|---|
| Child runs | in-process `AgentSession` | in-process (`spawn`/`fork`) or out-of-process (`acp`/`subprocess`) via provider seam | in-process `CodexThread` (tokio) | separate CLI subprocess per session; subagents in that process | subprocess `pi -p --no-session` | in-process | in-process (`kind: local`), A2A for remote | in-process `Task`; cloud "orbs" |
| Blocking default | per agent (`blocking` frontmatter); else async job | one-shot: blocking; continuable: async | **async** (`spawn_agent` returns at once) | blocking; `run_in_background` | blocking | blocking; `background: true` experimental | blocking | blocking `Task`; async orbs |
| Definition file | `.omp/agents/*.md`, `~/.omp/agent/agents/*.md` (md + YAML) | none; Cordis YAML "agent presets" + ad-hoc persona | `.codex/agents/*.toml` | `.claude/agents/*.md` | `.pi/agents/*.md`, `~/.pi/agent/agents/*.md` | `.opencode/agents/*.md` or `opencode.json` | `.gemini/agents/*.md` | TS plugin `amp.createAgent` |
| Programmatic definition | SDK `runStructuredSubagent` | yes (`SubagentStartRequest.persona`) | ad hoc via `message` + `default` role | SDK `agents:` map, CLI `--agents` JSON | no | JSON config | settings overrides | yes |
| Model may write its own system prompt | no (only a task for a bundled general agent) | yes (`persona`) | effectively yes (`message` to `default`) | no (task prompt to `general-purpose`) | no | no | no | yes (`Task` prompt) |
| Child persisted | yes, `<parent>/<id>.jsonl` | yes, own session + descriptor events | yes, own rollout + SQLite spawn edge | yes, `<session>/subagents/agent-<id>.jsonl` | **no** | yes, session with `parentID` | no | yes (threads) |
| Steer running child | TUI focus, RPC `steer_subagent`, `write agent://` | `send_message`, user via nested chat | `send_message`, `followup_task`, `interrupt_agent` | panel + `SendMessage`, `TaskStop` | no | `task_id` re-invoke, TUI jump | no | yes, mid-flight |
| Sibling messaging | yes (IRC bus, `agent://all`) | **no** (strict parent↔child); teams only | yes (`AgentPath`) | yes (`SendMessage`) | no | no | no | no |
| Child→parent while running | `write agent://<parent>` | `sendToParent` (parent must be live) | mailbox | `SendMessage(main)` | no | no | no | no |
| Addressing | `agent://<id>`, `history://<id>`, `proc://` | `SubagentAddress` tuple, UI resource URI | `AgentPath` `/root/worker` + nickname + `ThreadId` | `agentId`, `name`, `@agent-<name>` | n/a | `SessionID` | tool slug | thread URL |
| Depth default | 2 (`task.maxRecursionDepth`) | 1 (`maxDepth`) | configurable | 3 (`CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`) | none | 1 (`subagent_depth`) | 0 (forbidden) | 0 |
| Concurrency cap | 32/session + per-provider semaphore around the HTTP call | 8 (`maxActiveSubagents`), no queueing | `agents.max_concurrent_threads_per_session` | 20 | 4 of 8 | — | — | — |
| Structured output | `yield` tool, 3 retries | `structured_output` tool, monotonic commit | typed tool results; text result | `StructuredOutput` tool, retries | text, 50 KB cap | XML envelope + JSON schema, 2 retries | text | text |
| AGENTS.md to child | **never** (`excludeAgentsMd: true`, `OMP/task/executor.ts:4798`) | preset-defined | yes (walks from child cwd) | yes (CLAUDE.md + AGENTS.md; `omitClaudeMd` opt-out) | from disk | project context | settings | no |
| Parent transcript to child | no | `spawn`: no; `fork`: completed-turn prefix | `fork_turns: none|all` (filtered) | no; `fork` type: everything | no | no | no | no |
| Host observability | 3 event channels; RPC `off/progress/events` + byte-range transcript paging; TUI focus retarget | session projections; nested chat view | child is a first-class thread (`thread/started`, `SubAgentActivityItem`) | SDK task messages + `parent_tool_use_id`; panel; `/tasks` | stdout JSON → tool details | first-class session; TUI tree keys | tool card | thread UI |
| Lifecycle hooks | `before_subagent_spawn` (block/override model); extensions rebound per child | child Cordis scope; scoped listeners | `SubagentStart`/`SubagentStop`; `Interrupt`/`SessionEnd` skipped | `SubagentStart`/`SubagentStop`; settings hooks apply; frontmatter hooks | — | permission merge | policy rules per subagent | global `agent.start/end` |

## 3. Spawn and lifecycle

**States.** omp `AgentRegistry`: `running → idle → parked`, terminal `aborted` (`OMP/registry/agent-registry.ts`). Codex `AgentStatus`: `PendingInit, Running, Interrupted, Completed(text), Errored(msg), Shutdown`; `Interrupted` is not final, so an interrupted child stays addressable (`codex-rs/core/src/agent/status.rs`). Claude: `pending, running, completed, failed, stopped, killed`; a user-stopped agent refuses further messages. dsh: runtime `idle|running` plus stop reasons `completed|aborted|error|max-tokens|refusal`.

**Completion delivery to the parent.**
- Blocking: the tool result (everyone).
- Async: omp registers a job of kind `task` in the same `AsyncJobManager` as bash, delivered by the same yield-queue receipt and idle-wake turn as `bg_N` results. Claude: a "task-notification" synthetic user message in a later turn; subagents and background bash share one task registry (`task_type: local_agent | local_bash`). OpenCode: synthetic prompt with a `<task state=…>` XML envelope. dsh one-shot background: generic `ctx.jobs` (`job_output <id>`); dsh continuable: a settlement notice user message, outside the job registry. Codex V2: a completion watcher posts an `InterAgentCommunication` into the parent's mailbox with `trigger_turn: false`; the parent reads it with `wait_agent`.

**Budgets.** omp `task.maxRuntimeMs` (default off) and a soft request budget (200; wrap-up prompt at 1.0×, forced yield at 1.5×). Claude `maxTurns` returns output marked partial and resumable; SDK `maxBudgetUsd`. Gemini `max_turns: 30`, `timeout_mins: 10`. Codex children count against the root `RolloutBudget`.

**Parent abort / new session / exit.**
- omp: parent abort cancels the in-flight blocking call; detached jobs continue. `/new` cancels all owned jobs. Dispose drains the job manager with a 3 s timeout and treats shutdown aborts as non-terminal (no tombstone), so the child revives from disk later (`OMP/task/executor.ts:3558-3626`).
- Codex: a tree-wide `CancellationToken` plus a `TaskTracker`; `close_agent` shuts down descendants.
- Claude: background work cannot outlive the process; on restart tasks get `status: "stopped", reason: "worker_restart"`.
- dsh: `workspace/session-stop` cancels live descendants child-first; in-memory inbox messages not yet journaled are lost.

**Memory.** omp parks idle children after 7 min (`task.agentIdleTtlMs`), dropping the session object and keeping the JSONL; messaging a parked child revives it. Codex evicts idle child threads and rehydrates from the rollout on the next message. dsh evicts settled continuable children. All three keep an in-memory registry row per child.

**Prompt cache.** omp appends per-spawn text (context, worktree, peer roster) after the static blocks so siblings of one role share a prefix. dsh forbids changing the model on `fork` children to keep the KV prefix. Claude forks share the parent's cache prefix.

## 4. Agent definitions

Markdown with YAML frontmatter, body = system prompt, is the consensus (omp, Claude, pi, OpenCode, Gemini). Codex uses TOML (`developer_instructions`); dsh has no files at all (agents are plugin compositions called presets, and the spawn request carries an ad-hoc `persona`); Amp uses TypeScript.

Common frontmatter: `name`, `description` (the model picks a role from it; Claude warns when descriptions exceed 15k tokens), `tools`, `model`. Beyond that:

| Field | omp | Claude | Codex | OpenCode | Gemini |
|---|---|---|---|---|---|
| deny list | — | `disallowedTools` wins over `tools` | `[features] shell_tool=false …` | `permission` ruleset | — |
| child spawning | `spawns` | `Agent(worker, researcher)` in `tools` | — | `permission.task` patterns | forbidden |
| thinking/effort | `thinkingLevel` | `effort` | `model_reasoning_effort` | — | `temperature` |
| output schema | `output`, `structured` | (SDK `outputFormat`) | — | JSON schema | — |
| execution | `blocking`, `isolated`, `prewalk`, `advisor`, `readSummarize`, `autoloadSkills` | `background`, `isolation: worktree`, `maxTurns`, `autoCompactWindow`, `skills`, `mcpServers`, `hooks`, `memory`, `omitClaudeMd`, `permissionMode` | `personality`, `skills.config` | `mode: primary|subagent|all`, `steps`, `hidden` | `kind: local|remote`, `mcpServers`, `max_turns`, `timeout_mins` |

Discovery precedence is project → user → plugin/bundled everywhere; Claude adds managed settings above all and CLI/SDK definitions above project files. omp rejects `.claude/agents` because the field semantics differ (`tools` names, `model` aliases); Claude plugins may not set `hooks`, `mcpServers`, `permissionMode`.

**Can the model invent an agent?** Two camps. Codex and dsh: yes, the parent's spawn message / `persona` is the child's mission, with a built-in `default` role supplying the rest. omp, Claude, OpenCode, Gemini, pi: no, `agent`/`subagent_type` must name a known role, and the escape hatch is a bundled general-purpose role plus an arbitrary task prompt. Both camps let hosts/SDKs define roles programmatically (Claude `agents:` option, omp SDK, Amp `createAgent`).

## 5. Models and roles

Every harness resolves the child's model in the harness, from host-supplied settings, with the same cascade shape: per-call argument → agent definition → global subagent default → parent's current model.

- omp: `modelRoles` (`@task`, `@smol`, `@advisor`, …) expand from settings; precedence host override → approved per-call `model` → `task.agentModelOverrides[name]` → frontmatter list → parent model. Per-call `model` is **off by default** and gated by `model-approval.ts`: only operator-level settings, the agent's own frontmatter (bundled/user, not project), or the parent's live model can grant a selector; project config cannot, because the model can write it.
- Codex: tool arg → `agents.default_subagent_model` → role TOML → parent → catalog default effort.
- Claude: tool arg → frontmatter (`inherit`) → `CLAUDE_CODE_SUBAGENT_MODEL` → main model; `CLAUDE_CODE_SUBAGENT_MODEL_FORCE=1` pins everything; thinking is always inherited.
- dsh: an administrator-approved route list is sampled into the session at start and inherited immutably; `fork` children may not change route.
- Gemini/Amp: `inherit` by default; Amp's built-in `Task` always runs the parent's model.

So "roles" split into two things: a **model alias table** (settings) and **agent definitions** that reference aliases. Only omp names the alias table "roles"; the mechanism exists everywhere.

## 6. Host observability

Three designs:

1. **Child is a first-class session the host attaches to.** Codex (`thread/started`, `thread/list sourceKinds=["subAgentThreadSpawn"]`, `thread/read`), OpenCode (session with `parentID`, TUI keys to enter/cycle children), dsh (nested chat view bound to the child session; one-shot read-only, continuable with composer and Stop), ACP RFD (`subagent_update` announces the child session id; child `session/update`s flow on the same connection with their own `sessionId`). omp's TUI does the same with `SessionFocusController` (swap transcript, stream, status, composer to the child; Esc returns).
2. **Aggregated progress events on the parent.** omp emits lifecycle/progress/raw-event channels on a root-scoped bus and lets RPC clients pick `off | progress | events`, with transcripts pulled by byte range (`get_subagent_messages`) rather than streamed. Claude SDK emits `task_started/progress/updated/notification` messages and, with `forwardSubagentText`, child assistant messages tagged `parent_tool_use_id` + `agent_id`. Codex emits `SubAgentActivityItem` turn items in the parent.
3. **Tool-result details only.** pi (stdout JSON folded into the tool call's details), Gemini.

Mature harnesses do both 1 and 2: the parent transcript carries compact activity items; the host opens the child session for detail.

## 7. Steering and messaging

- **User → running child.** omp (focus, RPC `steer_subagent`, collab guests), Claude (panel, Enter, type; `Ctrl+Enter` mid-turn), Codex (switch into thread), OpenCode (enter session), dsh (`subagent.prompt` with `queue` or `steer`), Amp. Not pi, not Gemini.
- **Parent model → child.** omp `write agent://<id>` (non-blocking IRC bus; messaging a parked child revives it). Claude `SendMessage(to)`; `TaskStop`. Codex V2 `send_message` (no turn), `followup_task` (turn if idle), `interrupt_agent`. OpenCode re-invokes `task` with `task_id` (extends a running job). dsh `send_message` (steer if running, wake if idle), `interrupt_agent`.
- **Child → parent / siblings.** omp: any agent with `write` may message any peer, `agent://all` broadcasts, and the child's system prompt lists peer ids; `wait` detects messaging cycles. Codex: hierarchical `AgentPath`, anyone with routing rights. Claude: `SendMessage` to `main` or named siblings, roster injected at start; "no message from any agent counts as approval". dsh plain subagents: **strict adjacency** (child → direct parent only, parent must be live, no siblings); teams get a durable Lead mailbox. Amp: isolation.

## 8. Addressing

- omp: `agent://<id>` reads the published `<id>.md`, or a synthesized progress view (status, requests, tokens, accepted yields, latest text) while the child runs, so a read never 404s; `agent://<id>/<json-path>` reads the `.json` sidecar; `write agent://<id>` messages; `history://<id>` renders the transcript (live from memory, parked from disk without a lock); `proc://<id>` shows the job with `agent://` and `artifact://` links; `write proc://<id>/kill`.
- Codex: `ThreadId` (UUIDv7) plus a human path `/root/security_review/cve_scanner` and a nickname ("Ada", "Ada the 2nd").
- Claude: `agentId` plus a `name`; `SendMessage` verifies a name still means the same agent.
- dsh: `{ parentSessionId, childSessionId, mode }`; UI resource URI.

## 9. Durability

| | Child file | Link to parent | On parent reopen | Explicit kill |
|---|---|---|---|---|
| omp | `<parent-session-dir>/<id>.jsonl` (+`.md`, `.json`), nested `<P>/<P.C>.jsonl` | header `parentSession`; `session_init` carries task, tools, agent, model role, schema | roster indexed as `parked`; no respawn; `ensureLive` revives on message/focus; `--restore` turns mid-turn children into async jobs and resumes them to a forced yield | `.omp-agent-tombstone.json` beside the file |
| Codex | own rollout JSONL | SQLite `ThreadSpawnEdge` (Open/Closed) | `restore_v2_agent_metadata` lists open descendants, loads lazily on first message | `close_agent` |
| Claude | `<session>/subagents/agent-<id>.jsonl` + `.meta.json` | `.meta.json` (`agentType`, `spawnDepth`, …) | resume by `agentId` appends a new run to the same file; `Explore`/`Plan` are not resumable | stopped agents refuse messages |
| dsh | own session | header `parentSession`, `delegationDepth`; child `subagent/descriptor` event (v3) and parent `subagent/catalog` event, both compaction-proof | cold resume rebuilds the child from the descriptor | — |
| OpenCode | DB session | `parentID` | resume via `task_id` | — |
| pi | none (`--no-session`) | — | — | — |

Two recurring ideas: write the spawn contract into **both** logs so each survives the other's compaction (dsh descriptor/catalog; omp `session_init`; Claude `.meta.json`), and **never respawn on reopen**: the completed tool result stands, unfinished children are either left parked or explicitly restored. Claude's docs state the compaction rule directly: subagent transcripts are separate files and unaffected by main-conversation compaction.

## 10. Tools, depth, permissions

- **Tool sets** come from the definition's allow (and sometimes deny) list over the parent's catalog; MCP and host tools are inherited unless restricted (omp, Claude, Codex). Harnesses strip parent-only tools from children: omp `todo`; Codex `request_user_input`; Claude strips `AskUserQuestion`, plan-mode and `EndConversation` from background children; OpenCode denies `todowrite` and recursive `task`. omp auto-adds `wait` whenever a child has `task` or `bash`, and `task` only when `spawns` allows and depth permits; plan mode clamps children to read-only tools.
- **Depth** is enforced by withholding the spawn tool at the limit (omp, Claude non-fork) or failing the call (OpenCode, dsh `SubagentDepthError`). dsh persists `delegationDepth` in the child header so a cold-resumed child cannot reset to depth 0.
- **Permissions never widen.** Codex: a role may reduce features, change prompt/model, but not loosen sandbox, approvals, cwd or providers. dsh: child `approvalPolicy` pinned to `never`, with a fixed prompt statement that its scope cannot be widened from inside. Claude: no agent message counts as approval or changes config. OpenCode merges parent rules with subagent overrides.

## 11. Hooks and extensions

- omp: each child gets its own `ExtensionRunner` bound from the parent's prepared extension factories, so extensions run inside children with per-child state; `ctx.agent = { kind: "sub", id, name, depth, parentId }`; `session_stop` and input interception never fire in children; the parent fires `before_subagent_spawn` (block, note, or override model).
- Codex: `SubagentStart`/`SubagentStop`; `PreToolUse` on `spawn_agent` can intercept; `Interrupt` and `SessionEnd` do not run for subagents; hook input `session_id` is the parent's, `transcript_path` the child's.
- Claude: settings hooks run in subagents with `agent_id`/`agent_type`; `SubagentStart` may inject `additionalContext` but cannot block; `SubagentStop` may block termination and feed text back as the next prompt; frontmatter hooks are scoped to the child's lifetime.
- dsh: child-scoped registrations unwind on disposal; event dispatch is scope-filtered.

## 12. External actors

- omp RPC: `set_subagent_subscription`, `get_subagents`, `get_subagent_messages(fromByte)`, `steer_subagent`, `cancel_subagent`; `/collab` guests can steer; SDK `runStructuredSubagent`, `createAgentSession({ subagentEventBus })`.
- Codex app-server: children are threads; `thread/list`, `thread/read`, `turn/start`, `turn/interrupt` on the child id; `InProcessAppServerClient` for embedded Rust use; Python SDK.
- Claude: SDK `agents:` option and task messages; `claude agents --json`; a per-session Unix socket (`CLAUDE_CODE_MESSAGING_SOCKET`) scripts can post into.
- dsh: `ctx.subagents` service API in-process (`start`, `startContinuable`, `sendMessage`, `interrupt`, `listChildren`, `listDescendants`); ACP is automation-only and ACP children are one-shot.
- OpenCode: REST `/session` with `parentID`. ACP RFD: child sessions multiplexed on one connection; client prompting of children is off unless a capability says otherwise.

## 13. Structured output

Consensus: a **tool the child must call** (omp `yield`, dsh `structured_output`, Claude `StructuredOutput`, OpenCode's schema path), validated against JSON Schema, with validation errors returned to the model and a small retry ladder (omp 3, OpenCode 2); not provider JSON mode. omp also accepts incremental sections and distinguishes permissive (warn) from strict (fail) exhaustion; the parsed value is written to a `.json` sidecar readable through `agent://<id>/<path>`. dsh guards against a second `structured_output` call. Unstructured runs return the final assistant text. Output is capped for the parent (pi 50 KB per task; omp truncates and points at `agent://<id>`) and, in Claude, sanitized against `<system-reminder>`/turn-marker imitation before it reaches the parent.

## 14. Context inheritance

| | cwd | Project instructions | Skills | Parent transcript | Memory |
|---|---|---|---|---|---|
| omp | yes (or worktree) | `RULES.md` yes, **`AGENTS.md` never** | yes, `autoloadSkills` preloads | no | hindsight/mnemosyne refs forwarded |
| Codex | yes, role cannot change it | AGENTS.md walked from child cwd | role may disable | `fork_turns: all` keeps user/final-answer messages, drops tool calls, reasoning, usage | — |
| Claude | yes (or worktree) | CLAUDE.md + AGENTS.md unless `omitClaudeMd` | `skills:` preloads up to 32 | no (except `fork`) | not the parent's; own `memory:` dir |
| dsh | yes | via preset | via preset | `spawn` no; `fork` completed-turn prefix | — |
| OpenCode / Gemini / Amp / pi | yes | project context / settings | — | no | — |

Every child also receives an explicit "you are a delegated subagent" framing: omp's `subagent-system-prompt.md` (completion rules, peer roster), dsh's delegation-scope statement, Claude's environment details, Codex's role `developer_instructions`.

## 15. What embedding changes for axo

What the sources do when the harness is a library (Codex core, dsh, OpenCode, omp SDK) versus a CLI (Claude, pi):

- **Children are sessions, not processes.** Every library-shaped harness runs children in-process as the same session type as the root; only pi (an extension with no in-process API) and Claude (whose unit is a CLI process) spawn processes. omp, Codex and dsh all still **evict idle children** from memory and rehydrate from the log, which axo gets for free: dropping a `Session` is parking; `Axo::open_session` is revival.
- **The host needs a registry of live sessions.** Codex `ThreadManagerState`, omp `AgentRegistry`, dsh `SubagentContinuationManager`. axo currently has none: `Axo::open_session` returns an owned `Session`, `Axo` keeps no table, and `SessionTools` has no handle back to `Axo`. This is the `SessionHost` that `durable.md` deferred; Track 2 needs it inside axo (the `task` tool must create and find children) rather than in Axome.
- **Observation is attachment, not subscription tiers.** omp's `off/progress/events` RPC levels and Claude's SDK task messages exist because the host is on the other side of a pipe. An in-process host can call `child.attach()` and get the same `SessionEvent` stream the root has; what the parent session needs to emit is only the announcement (Codex `thread/started`, ACP `subagent_update`, dsh `subagent/catalog`) and compact activity (Codex `SubAgentActivityItem`). Byte-range transcript paging and progress aggregation become host concerns (Axome's RPC), not axo's.
- **One job registry is the majority design, but not unanimous.** omp (job kind `task`), Claude (`task_type: local_agent | local_bash`) and OpenCode (`BackgroundJob`) put subagents and background commands in one registry with one delivery path and one `wait`. dsh splits one-shot background runs (jobs) from continuable children (continuation manager, settlement notices); Codex keeps an agent registry with mailboxes separate from exec sessions. axo's `JobRegistry` already owns admission, receipts, idle-wake and `proc://` for bash; its job record is bash-shaped (command, spool, artifact, process control).
- **Hooks are per-session objects.** omp rebinds extension factories per child; dsh scopes a child context; Codex and Claude run the same configured hooks with a child id. In axo, `SessionOptions` already carries host `tools`, `schemes` and `sections` per session and `ToolFactory` is called per fork; a child needs the same derivation from an `AgentSpec`, and `LoopHooks::before_tool_call` is where a host blocks or rewrites a spawn (omp `before_subagent_spawn`).
- **No approvals means pinning matters more.** dsh and Codex make the delegation boundary non-escalating because they have approval systems to bypass; axo has none, so the equivalent is: a child inherits the parent's `ProjectAccess`, `ExecEnv`, supervision and host hooks and cannot select wider ones; `ask` is a parent-only tool unless a host decides otherwise.
- **Supervision composes.** Child bash commands must run through the same `ProcessSupervisor`; with cgroup supervision a child session's jobs should nest under the child so `shutdown` on the child kills its tree without touching siblings. Track 3's lifecycle policies (navigate keeps jobs, fork gets a new scope, Stop keeps background work) already assume child owners with their own ids.
- **Durability is already half-built.** `Header` has `parent_session` and `forked_from`; a child is `parent_session` plus a spawning call id (and, following dsh, the depth) with no `forked_from`. The parent's `ToolCall`/`ToolResult` pair and the child's header are the two-sided spawn record every durable source keeps; the parent-side result must be reproducible on reopen without respawning (omp, Codex, Claude all agree), which is exactly the `Replay::Safe` plan in `durable.md`.

## 16. Decisions the design has to make

1. **Registry shape:** generalize `JobRegistry` into kinds (command, child session) sharing ids, receipts and `wait` (omp/Claude/OpenCode), or a separate child registry with its own delivery (dsh/Codex).
2. **Blocking default:** block (Claude, OpenCode, Gemini, pi, dsh one-shot), async (Codex, dsh continuable), or per-role (`blocking` in omp).
3. **Ad-hoc roles:** may the model supply a system prompt (Codex, dsh), or only a task for a general-purpose role (omp, Claude, OpenCode, Gemini, pi)? Programmatic `AgentSpec` from hosts is universal.
4. **Model selection:** which cascade, whether per-call `model` is on by default, and whether project-level files may name models (omp says no, because the model can write them).
5. **Messaging topology:** parent↔child only (dsh plain, Amp), or peers (omp, Claude, Codex); whether `write agent://` is the mechanism; `wait` cycle detection if peers.
6. **Continuable children:** one-shot only, or resumable/steerable with a stable id (every source except pi and Gemini supports it).
7. **Context:** include project `AGENTS.md` (Codex, Claude) or exclude it (omp); whether to offer a fork mode seeded with the parent's completed turns (Codex, Claude, dsh).
8. **Depth and concurrency defaults** (1–3 and 8–32 across sources; provider semaphores around the HTTP call, not the child's lifetime).
9. **Structured output:** a required `yield`-style tool with validation retries and a sidecar readable by path (consensus), plus an output cap and sanitization for the parent.
10. **Observation surface:** what the parent emits (spawn announcement, activity items) versus what hosts take from `child.attach()`; whether axo offers `history://`/transcript rendering or leaves it to hosts.
