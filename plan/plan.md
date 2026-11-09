# axo plan: active work & open discussions

This is the live planning document for unbuilt work and active design discussions. It changes freely as we work.

- **Goals & settled architecture:** [`design.md`](design.md)
- **Implemented reference:** [`crates/axo/docs/README.md`](../crates/axo/docs/README.md)
- **Background studies:** [`pi/`](pi/README.md) and [`research/`](research/README.md)

## Overview of tracks

M1–M5 plus Tracks 2, 3 and 4 are complete. The remaining work splits into independent tracks (`axo-acp` is deferred):

```mermaid
flowchart LR
  subgraph T1["Track 1: Codemode & Script Sandbox"]
    N[Nested tool calls + structured output] --> C0[Codemode rungs 0–1]
    C0 --> DT[define_tool / script hooks]
  end
  T1 -.builds on shipped spawn.-> S2["Track 2: Shipped"]
  N -.reused by.-> MCP[Track 5: MCP & Extensions]
```

| Track | Scope | Status |
|---|---|---|
| **1. Codemode, script extensions & agent specs** | Embedded JS sandbox (`rquickjs`), nested tool calls, structured tool output, `codemode` tool (rungs 0–1), then `define_tool` and child script hooks (rungs 2–5) | **Active focus.** Rungs 0–1 decided; rungs 2+ in discussion |
| **5. Extensions (Bun host), MCP & `ast_edit`** | Extension contract, Bun stdio host & `@axo/sdk`, `.mcp.json` / host-supplied MCP tool adapter, optional `ast_edit` | Planned |
| **6. ACP binary (`axo-acp`, M6)** | Stdio JSON-RPC binary mapping ACP onto `Session` | Deferred: rethink whether axo itself should ship ACP or leave it to a host |

---

## Track 1: Codemode, script extensions and agent specs

Notes from the design discussion of 2026-10-22. Only items marked **Decided** are settled; items marked **Direction** remain open for discussion.

### Where we landed

- **Decided:** axo builds a codemode tool in pi's shape (rungs 0–1 below): a stateless, sandboxed JavaScript script per call whose only powers are tool calls. It is built in, behind a Cargo feature, on an embedded engine. It does not wait for the Bun extension host.
- **Direction:** state lives in resources that tools own, not in the script; everything addressable is reachable from scripts; and scripts may configure the harness of the subagents they spawn, but not change the session the user talks to.
- **Shape:** three separate systems, each useful alone. Codemode composes them but owns none of them:
  1. subagents, specified by an `AgentSpec`;
  2. the script sandbox;
  3. the hooks contract, with a script implementation among others.

### omp `eval` vs pi `codemode`

Both let the model write code that calls tools, built for different jobs. Sources: pi 1.3.0 (`packages/coding-agent/docs/codemode.md`, `packages/codemode/README.md`, `extensions/codemode/`), omp 18.12.2 (`docs/tools/eval.md`, `src/prompts/tools/eval.md`).

| | omp `eval` | pi `codemode` |
|---|---|---|
| Unit | One cell per call, notebook-style | One script per call |
| State | Kernel stays alive between calls; lost on restart ("Kernel-loss notice means reload setup") | Fresh VM per script; small JSON values via `store`/`load`, saved as session entries per branch |
| Runtime | Bun worker (`Bun`, `fs`, `fetch`, `process`, `require`) and a Python subprocess | QuickJS compiled to WebAssembly: no files, network, timers or modules |
| Packages | `%bun add`, `%pip install`, `%load ./script.py` | None |
| Reach | Anything bash can, plus `tool.<name>()` | Only `tools.<name>()` and `models.*` |
| Host helpers | `agent()`, `workpool()`, `wait()`, `judge()`/`judgeBatch()`, `completion()`, `@tool` for subagents, extension preludes such as `browser` | `searchTools`, `describeTool`, `describeNamespace`, `models.classify`/`generateImages` |
| Output | Printed text, images, tables, DataFrame views, figures | `text()`, `image()`, `return`, `console`; capped by an output-token budget |
| Safety | `approval="exec"`, bash's trust level | Sandbox: 256 MB heap, interrupt handler, at most two workers per process |
| Long runs | Optional auto-backgrounding into an async job | Hard `timeout_ms` |
| Portability | Tied to omp | No pi dependencies; also runs on Cloudflare Workers |

#### Intent

- **omp: an agent workbench and orchestration language.** The doc opens with "Do not shell out to `python -c`, `bun -e`, or `node -e` through `bash`." Eval is the better home for code the agent was already running through bash:
  - state across calls for notebook-style exploration (DataFrame views, figure capture, `%pip`);
  - orchestration, through `agent()`, `workpool()`, `wait()` and `judge()`, so the agent runs other agents from code;
  - a home for extensions: preludes add globals such as `browser`.

  It is bash with better ergonomics, so it gets bash's trust level.
- **pi: composing tool calls and saving context.** "Only the script's output reaches the model, so a script can run calls in parallel and filter large results before the model sees them."
  - tool results are structured values, and only the script's output reaches the model;
  - MCP tools default to script-only exposure, so large servers stay out of the prompt and are found with `searchTools()`;
  - every effect is a tool call, so hooks, blocking and validation apply;
  - no hidden state, which suits durability.

  [INFERENCE: the name and framing match Cloudflare's "Code Mode"; pi's docs do not cite it.]

#### What pi's codemode cannot do

Impossible in pi's codemode as shipped:

1. **Keep a large working set across calls.** Example: load every session JSONL file once, then answer follow-up questions. `store` holds at most 256 KiB per value and 1 MiB in total.
2. **Take in a large file in one go.** `tools.read` returns the same 2000-line / 50 KB slice the model sees (`read.ts:216` builds the script value from the truncated content); `tools.bash` returns at most 1 MiB.
3. **Call an LLM or run agents from code.** Chat models "cannot be run from scripts"; only non-LLM classifiers and image models. pi has no subagent tool in core.
4. **Hold a live connection across calls.** A logged-in browser tab, a DB connection, a websocket. Inside a script there are no sockets at all.
5. **Keep working after its call returns.** For example, a 20-minute benchmark sweep while the conversation continues.
6. **Define tools that subagents call back into** (omp's `@tool`).

Possible only by going through `bash`, at the cost of a process per step, text in and out, and often a second language:

| Task | omp eval | pi codemode |
|---|---|---|
| Collect dependency versions from 30 `Cargo.toml` files | A TOML parser via `%bun add`, or `tomllib` | Regex by hand, or `cargo metadata` through bash |
| Fetch 50 PRs from the GitHub API | `fetch` in a loop | `curl` through bash per request |
| Plot cost per day | pandas and matplotlib | Write a `.py` file, run it with bash, `tools.read` the PNG, then `image()` |
| Poll CI every 30 s | A sleep loop | No timers; `bash sleep 30` between checks |
| Fuzzy-dedupe 50k strings | V8 or Python speed | QuickJS interprets with a 256 MB heap; slow, or bash a real runtime |
| Hash files, diff text, resize images | Standard library or packages | `sha256sum`, `diff`, `magick` through bash |

Codemode plus bash covers everything eval does except state between calls and orchestration. Items 3 and 6 are not sandbox limits: `agent()` or `completion()` is another host function or tool call. What a sandbox cannot give on its own is state or a handle that outlives the script.

### Embedded, not the Bun host

**Decided.** Earlier notes linked eval to the Bun extension host as a reuse shortcut ("the Bun host is already a JS runtime"). The two have opposite requirements:

| Need | Extensions (why Bun) | Codemode scripts |
|---|---|---|
| Access | Full runtime: `fetch`, fs, processes, npm, OAuth servers | Only tool calls, so `before_tool_call` cannot be bypassed |
| Sandbox | Given up on purpose | The point |
| Lifetime | One long-lived process | One short script per call |
| Isolation | One process for all extensions | A runaway script must not take extensions down |
| Cost when unused | Bun must be installed | Nothing to install |

- **pi does the same.** Its extensions run in Node/Bun, yet codemode runs in a separate QuickJS-wasm VM in a worker with a 256 MiB cap (`execute.ts`, `config.ts:491`).
- **Fewer hops.** In the Bun host, every nested tool call would cross the stdio bridge twice: script → Bun → axo's tool pipeline → Bun → script. Embedded, a nested call is an in-process call into the tool pipeline.
- **The VM has no power of its own.** QuickJS starts with nothing. Every capability is a Rust host function that axo injects. Rust decides what is possible; JS lets the agent express runtime logic (loops, conditions, filtering).
- **Engine candidates** (to verify in a short spike before coding):

  | Engine | Notes |
  |---|---|
  | QuickJS (`rquickjs`) | Small; memory and interrupt limits; can await Rust futures. The default |
  | `boa` | Pure Rust, slower. The fallback |
  | V8 (`deno_core`) | Fast, but tens of MB in the binary; against the memory goal |

### The model: a ladder of power

#### Three ideas

- **A. Data lives in a tool or resource, not in the script.** The script stays stateless. Anything that lasts has an owner and a name; scripts refer to it by name. Each resource has its own crash behavior, unlike an eval kernel where everything dies together:

  | Resource | Owner | Name |
  |---|---|---|
  | A dataset | A query tool, e.g. SQLite in the session directory | `table://sessions` |
  | A browser tab | The browser tool | `browser://tab/3` |
  | A long command | The job registry | `proc://bg_2` |
  | A subagent | Child sessions | `agent://reviewer-4` |

- **B. Everything addressable is callable.** URL schemes supply the nouns, tools the verbs, subagents the workers. A script sees all of them as one object model, and every call passes through the tool pipeline.
- **C. Scripts extend axo.** Once scripts can call everything, the next step is letting them add things.

#### The ladder

| Rung | The script can… | Example |
|---|---|---|
| 0 | call tools | pi's codemode |
| 1 | call named resources (A, B) | query `sessions`, wait on `agent://…` |
| 2 | define functions that later scripts reuse | a `parseCargoLock()` helper for this session |
| 3 | register tools the model or subagents can call | a `pr_status` tool wrapping `gh` |
| 4 | register hooks | after each edit to a `.rs` file, run `cargo check` and attach errors |
| 5 | change the prompt or tool set | add a section; hide tools for a focused task |

**Decided:** rungs 0–1 are the immediate build target. Rungs 2 and up need the design below.

- **Rungs 2 and 3 merge.** A saved function is a defined tool that only scripts can call.
- **Tools and hooks differ in kind.** A tool runs only when called; the call and its result are visible, and a bad tool is one the model can stop using. A hook runs on every matching event without being called, and can block, rewrite or redirect other calls or decide whether the loop continues. A tool adds a new verb; a hook changes how the existing ones behave.
- **Skills can carry scripts.** A markdown skill can come with a script that sets up the hooks and tools its task needs. The agent can also write such a script mid-task and save it as a skill afterward.
- **Seams, not internals.** Scripts only plug into published seams: before/after tool call, prepare request, finish turn, the steer and follow-up queues, context edits, prompt sections, tool registration.

#### Parent and children

**Direction:**

> A script may **add** to the parent: functions and script-callable tools (rungs 2–3). It may **change behavior** (hooks, prompt sections, tool set, loop control; rungs 4–5) only in children it spawns.

The session the user talks to is changed only by the user. Children are disposable agents a script builds for one job; their hooks die with them.

- **The invariant.** Children inherit the parent's policy: user settings, user and project extensions, project trust, the exec environment. Script hooks run **before** that inherited policy, so the policy always sees the final arguments that will run. A script can tighten a child's rules, never loosen them past the user's.
- **What it fixes:**
  - persistent prompt injection cannot take over the parent;
  - silent interference, and hooks outliving their reason, are confined to a child with one task;
  - the parent's tool set, and so its prompt cache, never changes;
  - provenance comes naturally ("child `reviewer-3`, built by the script at entry #N, with hooks X and Y");
  - durability is mostly free: child sessions are linked to the call that spawned them, and the child's harness setup is system messages in its own transcript.
- **What it costs:**
  - delegation overhead: a child needs a briefing and its own context, so it uses more tokens and takes longer;
  - children should return small schema-shaped results, not transcripts;
  - changes to the parent session move to user-level config.

### Three systems, one job each

```mermaid
flowchart LR
  R[agent files] --> T[spawn tool]
  C[codemode script] -->|"tools.spawn(spec)"| T
  H[Axome / Rust host] --> A
  T --> A["subagents<br/>AgentSpec → child session"]
  A --> K["hooks contract"]
  K --- K1[Rust impl]
  K --- K2[Bun/TS impl]
  K --- K3[script impl<br/>QuickJS]
```

1. **Subagents (Rust core).** An `AgentSpec` is plain data (`name`, `description`, `model`, `tools`, `contextFiles`, `sections`, `output`, `maxTurns`, body); inline specs in `spawn` restrict prompt, tools, model, and output. Spawning it returns a handle (`agent://<id>`) to wait on. It knows nothing about JS.
2. **The script sandbox.** Runs JS with injected host functions under limits. Codemode is a thin tool on top of it; `define_tool` sources and script hooks reuse the same sandbox.
3. **Hooks.** One Rust contract with several implementations: Rust (Axome, first-party features), TypeScript (Bun extensions), and a script implementation that runs stored JS in the sandbox when the hook fires.

#### Which tool grants each rung

| Rung | Tool | Applies to |
|---|---|---|
| 2–3 | `define_tool` (name TBD) | The current session, as an addition |
| 4 | `spawn`, through a proposed `hooks` field on the spec | Only the spawned child |
| 5 | `spawn`, through the spec's `prompt` and `tools` fields | Only the spawned child |

`define_tool`:

```json
{
  "name": "cargo_errors",
  "description": "Run cargo check on one crate; return only error lines",
  "parameters": { "type": "object", "properties": { "crate": { "type": "string" } }, "required": ["crate"] },
  "source": "const r = await tools.bash({ command: `cargo check -p ${args.crate} --message-format short` }); return r.output.split('\\n').filter(l => l.startsWith('error'));"
}
```

- `source` runs in the sandbox, so the tool can only call other tools.
- Saved as a session entry: replayed on resume, scoped to its branch, kept through compaction.
- Script-only exposure by default, so the parent's declared tool set does not change. Children can be given it by name.

`spawn` with a child harness:

```json
{
  "spec": {
    "prompt": "Refactor compaction/summary.rs to …",
    "tools": ["read", "edit", "bash", "cargo_errors"],
    "hooks": [{
      "on": "after_tool_call",
      "tools": ["edit", "write"],
      "source": "if (call.args.path.endsWith('.rs')) result.append(await tools.cargo_errors({ crate: 'axo' }));"
    }],
    "output": { "type": "object", "properties": { "summary": { "type": "string" } } }
  },
  "task": "Refactor compaction/summary.rs to …"
}
```

**Codemode is only as powerful as the tools it can call.** Two caveats:
- the ceiling is the most powerful callable tool (`bash`); the sandbox guarantees that effects pass through the tool pipeline and its hooks, not that they are restricted;
- scale is new: a script can make 500 calls where the model would make 5, so call-count limits and timeouts belong on codemode itself.

### Scenarios and risks

| User says | What gets set up | Uses | Under the parent/child rule |
|---|---|---|---|
| "Refactor the compaction module, keep it compiling" | After each edit to `crates/axo/**/*.rs`, run `cargo check -p axo` and attach the first errors | after-tool hook | ✅ Runs in a child; the parent orchestrates |
| "Don't touch generated files, never run `cargo update`" | Block edits under `**/generated/**` and bash matching `cargo update` | before-tool hook | ❌ for the parent (user settings/policy); ✅ imposed on children |
| "Plan first, change nothing until I say go" | Block `write`, `edit` and mutating bash; add a "planning" prompt section; remove both on "go" | before-tool hook, prompt section | ❌ for the parent (a user command); ✅ as a read-only planner child |
| "Keep fixing until the tests pass, at most 5 tries" | When the model would stop, run tests; if they fail and tries remain, continue with failures as a follow-up | finish-turn hook, follow-up queue | ✅ A child with the loop returns the final result |
| "Check my PRs' CI" (repeatedly) | A `pr_status` tool defined once (`gh api` + parsing) | `define_tool` | ✅ An addition |
| `/review:adversarial` | 6 scouts with output schemas, a skeptic per finding, triage, report | agents, resources | ✅ Already children |
| "Debug this flaky test" (huge logs) | For `cargo test` output, keep only failing tests and panics; full output in an artifact | after-tool hook rewriting results | ✅ A child runs and filters the tests |
| "Analyze my axo sessions" | Load sessions into a table once, define helper queries, reuse across turns | query resource, saved functions | ✅ An addition |

Key risks to design around when building step 3 (script hooks):
1. **Persistent prompt injection:** contained to children by the parent/child rule and inherited-policy-last ordering; hooks may also get a narrower set of host functions than scripts.
2. **Silent interference:** every block or rewrite must carry provenance ("blocked by hook `no-update`, defined at entry #123"), and active hooks/tools must be inspectable.
3. **Bugs and stalls on the hot path:** per-hook time budget, auto-disable after errors with a notice, no recursive self-triggering, and cancellation propagation.
4. **Prompt cache churn:** new tools default to script-only exposure so the declared tool set stays stable.
5. **VM lifetime for hooks:** measure a fresh QuickJS context per hook invocation (stateless, durable) against one VM per session.

### Step 1 build scope: Codemode rungs 0–1

1. **`rquickjs` spike:** verify async/tokio bridging, memory and interrupt limits, and per-context memory overhead against the PSS target (`boa` fallback).
2. **Core prerequisites (`axo-agent` & `axo`):**
   - **Nested tool calls:** extend `ToolContext` with a nested-call runner that goes through prepare, validate, `ToolHooks::before_tool_call`, and `ToolHooks::after_tool_call`. Assign `<parent>/<n>` IDs, tag `AgentEvent::ToolExecution*` with `parent_call_id`, and record nested calls on the parent `ToolResultMessage` so compaction still extracts file operations.
   - **Structured tool output:** add `output_schema` and `structured_content: Option<Value>` on `ToolOutput` (for `bash`: `{ output, truncated, full_output_path, exit_code, wall_time_seconds }` with a 1 MiB output cap; for `read`: text string or image object).
3. **Sandbox module (`axo::script`):** standalone module (not buried inside the tool) that runs JS on a blocking worker thread with a memory cap, interrupt budget/timeout, concurrency semaphore, and channel bridge for async host functions.
4. **`codemode` tool (`axo::tools::codemode`):**
   - Globals: `tools.*`, `ALL_TOOLS`, `text()`, `image()`, `return`, `console.*`, `exit()`, `store(k, v)` / `load(k)` (backed by branch-scoped `custom` session entries committed only when the script succeeds), and `// @options: {"max_output_tokens", "timeout_ms"}`.
   - Renders TypeScript declarations from callable tools' JSON schemas in its description.
   - `Replay::Never`: crash recovery reports the script as interrupted and lists completed nested calls.

---

## Shipped tracks

- **Track 2 (Agents, `spawn`, teams):** Landed in code; settled architecture decisions in [`design.md`](design.md#agents-and-child-sessions-axo) and runtime reference in [`crates/axo/docs/prompts-and-context.md`](../crates/axo/docs/prompts-and-context.md#agent-definitions), [`crates/axo/docs/sessions-and-compaction.md`](../crates/axo/docs/sessions-and-compaction.md#child-sessions), and [`crates/axo/docs/extending.md`](../crates/axo/docs/extending.md#spawning-child-sessions). Grounding research in [`research/subagents.md`](research/subagents.md).
- **Track 3 (Async `bash`, background jobs & `wait`):** Landed in code; settled decisions in [`design.md`](design.md#settled-core-decisions) and runtime reference in [`crates/axo/docs/configuration.md`](../crates/axo/docs/configuration.md#command-supervision-axosupervision) and [`crates/axo/docs/sessions-and-compaction.md`](../crates/axo/docs/sessions-and-compaction.md#background-job-durability). Historical verification log preserved in [`research/track3-supervision.md`](research/track3-supervision.md).
- **Track 4 (URL scheme registry):** Landed in code; settled decisions in [`design.md`](design.md#settled-core-decisions) and runtime reference in [`crates/axo/docs/extending.md`](../crates/axo/docs/extending.md#3-rust-extension-traits).
---

## Track 5: Extensions (Bun host), MCP & `ast_edit`

### Extension contract & Bun host

- **One shared Bun process (`design.md`).** Started only when at least one TS extension is enabled in a host-supplied user or project extension root (project extensions require `ProjectAccess::Trust`).
- **Bidirectional JSON over stdio.** axo invokes extension tools and lifecycle hooks; extensions call back into axo (tools, session queries, child sessions). TypeScript types in `@axo/sdk` are generated from the Rust contract (`ts-rs` or `specta`).
- **Provider & OAuth split.** Extensions can register providers and own OAuth login/refresh, handing `ModelAuth { api_key, headers, base_url }` to Rust so token streaming stays in `axo-ai` for standard wire formats. Custom wire formats stream through the extension.
- **Tool description overrides.** The extension contract includes a hook over each tool's prompt-facing definition (description and guidelines) before requests are sent.

### MCP tool adapter

- **Configuration files.** Read the first existing ecosystem project-root file of `.mcp.json`, then `mcp.json`, plus host-supplied user MCP configuration (separate from settings), using the `{ "mcpServers": { name: { command, args, env } | { url, headers } } }` format.
- **Trust gate.** Project MCP servers require `ProjectAccess::Trust`; at `Read` access they emit a diagnostic rather than starting. Add `project.files.mcp` (default `true`) when MCP lands.
- **Tool exposure.** Introduce per-tool exposure (`direct`, `codemode` / script-only, `deferred`, `hidden`) so large MCP servers can be callable from `codemode` scripts without flooding the system prompt's tool declarations.

### `ast_edit`

- Optional extension or Cargo feature providing structural search-and-rewrite via `ast-grep` patterns (`research/edit-formats.md` §7, `research/pi-edit.md`). Kept out of the default build to avoid pulling `tree-sitter` C grammars unless requested.

---

## Track 6: ACP binary (`axo-acp`, M6 — deferred)

Deferred pending a rethink of whether axo itself should ship ACP or leave it to a host. The notes below record the mapping if it is resumed.

Maps the [Agent Client Protocol](https://agentclientprotocol.com) over stdio onto `axo::Session`:

| ACP | `Session` |
|---|---|
| `session/new` and `session/load` | `create_session` and `open_session` + `attach` |
| `session/prompt` | `Command::parse` + `run_command` for `/<builtin>` text (ACP clients send commands as prompt text), otherwise `prompt` (and `steer` / `follow_up` mapping) |
| `session/cancel` | `abort` |
| `session/update` | Streams `AgentEvent`s through `Accumulator` |
| `availableCommands` | Publishes `Session::commands()`: built-ins with their `parameters` schema and skills with `argumentHint` (→ `input.hint`) |

Decisions when resumed:
1. Use the `agent-client-protocol` crate or handwritten serde types (<1k lines).
2. Default to `RecoveryPolicy::MarkInterrupted` on interactive open so the user decides whether to resume an interrupted run.
3. Optional ACP `terminal/create` backend for `bash`.

---

## Deferred studies

- **Compaction follow-ups (`research/compaction-methods.md`):** background precomputed summaries before the threshold; optional provider server-side compaction; Factory-style probe-based prompt evaluation harness.
- **Token-efficiency / output minimizer:** evaluate source-time output filtering for noisy build/test commands (with full output recoverable via spool paths).
- **Additional OAuth logins:** Anthropic subscription login (`ModelAuth` flag for preamble/casing/beta headers) and GitHub Copilot.

---

## Open questions

1. **Are codemode rungs 2+ in scope?** From rung 2 up (`define_tool`, script hooks), the script API is a slice of the extension contract, making codemode its first consumer.
2. **How do the embedded sandbox and the Bun host relate?** Is the embedded sandbox for session/role scripts and Bun for full-runtime extensions, or one extension contract with two runtimes?
3. **Parent-and-children rule** (Track 1, codemode rungs 2+). Adopt the rule strictly (scripts may add tools to the parent, but may only attach hooks/prompt changes to spawned children), or allow some parent hooks? Track 2 enforces the strict form by shape; see its "Deferred to Track 1" note.
4. **Where model-written tools/scripts live and how they promote.** Session-scoped (`custom` entries) by default; what is the file format and workflow for promoting a useful tool to a host-provided location?
5. **Hook safety without approvals** (Track 1, codemode rungs 2+). Since axo has no approval system (`design.md`), should child script hooks be restricted to a narrower host-function set (e.g. no network/write outside cwd), or rely strictly on the parent's inherited policy hooks running last?
6. **Which stateful resources ship built-in:** a SQLite-backed `table` tool, subagents, browser? (Background `bash` jobs ship with Track 3.)
7. **VM lifetime for script hooks:** fresh QuickJS context per hook invocation vs one retained context per child session (measure PSS impact).
8. **Naming:** tool names (`codemode` vs `eval` vs `script`, `define_tool`) and module path (`axo::script`).
9. **Orchestration in scripts** (Track 1, codemode rungs 2+): expose subagents and one-shot completions strictly through `tools.spawn(...)` (preserving "no powers beyond tools"), or also as top-level helpers?
10. ~~**Per-host identity.**~~ Resolved in Track 2: `axo` by default, renamed per host through `AxoBuilder::name`.
