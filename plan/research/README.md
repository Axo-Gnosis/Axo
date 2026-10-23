# Research behind axo

Findings gathered on 2026-10-21, before any code was written.

- **Sources:** source reading and small measurements against pi 1.3.0 (`earendil-works/pi` at commit `02726086`) and omp 18.12.2. omp is oh-my-pi, a pi fork.
- **Labels:** claims marked [INFERENCE] were not verified directly.

Focused studies, made on 2026-10-22. Their line references into `core.md` point at the version of that day.

| File | Question |
|---|---|
| [`message-models.md`](message-models.md) | Why pi's message model rather than genai's, Rig's, the Vercel AI SDK's, or a raw wire format; which providers pi supports |
| [`edit-formats.md`](edit-formats.md) | How agents let models edit files, and what the evidence says about each format |
| [`pi-edit.md`](pi-edit.md) | omp's `pi-edit` crate: the hashline protocol, its dependencies, and the options for using it |
| [`durable.md`](durable.md) | What pi-durable is, and which of its ideas axo needs for an always-on agent |
| [`agents-dir.md`](agents-dir.md) | What lives in `.agents/` and its neighbors across tools, and what axo reads |
| [`shell-tool.md`](shell-tool.md) | pi's and omp's bash tools compared; whether the tool can run fish |
| [`nix-shell.md`](nix-shell.md) | Running agent commands inside Nix dev shells: techniques, timings, design |
| [`compaction.md`](compaction.md) | When agents compact, how much reply headroom they reserve, and what they keep verbatim; provider limits and context-rot evidence |
| [`compaction-methods.md`](compaction-methods.md) | How pi, omp, Claude Code, Codex, the Anthropic and OpenAI APIs and others actually compact; prompts, carried state, failure lessons, evaluation, ideas for axo |

## Why a new core

Sworm needed an agent it could own: drive richly, extend freely, and keep light. The options examined, and where each fell short:

| Option | Shortfall |
|---|---|
| Any harness over ACP | ACP carries a fixed, minimal surface. No per-message history metadata, no forking from a message, no extension surface. |
| omp over its RPC | Heavy: ~220 MiB PSS per process even with an empty profile, plus any MCP servers from `mcp.json` running once per process. Its dialect changes often. Owned by someone else. |
| pi as a library (TypeScript SDK) | Feasible and light, but Sworm's backend is Rust, so it needs a JS sidecar. Porting the omp features in use onto pi is roughly 8–14 weeks (see [Porting omp features onto pi](#porting-omp-features-onto-pi)). And it still means adapting to someone else's core. |
| An existing Rust harness | None is mature, minimal, embeddable and extensible at once. See [Rust harness landscape](#rust-harness-landscape). |

## pi

- **Interfaces:**
  - There is no native ACP. The third-party [`pi-acp`](https://github.com/svkozak/pi-acp) adapter wraps `pi --mode rpc`.
  - **RPC mode:** strict JSONL over stdio, a closed command set, and one active session per process. There is no version handshake, and a breaking wire change happened at 0.84.0.
  - **SDK (`createAgentSession`):** runs in Node or Bun. Hosts control tools (`tools`, `noTools`, `excludeTools`, `customTools`) and extensions (inline `extensionFactories`, or a custom `ResourceLoader`).
- **Extensions:**
  - Extensions can add tools, commands, flags, providers (`registerProvider`), MCP servers and virtual models.
  - They can hook lifecycle events. A `tool_call` hook can return `{ block, reason }`.
  - They can persist custom entries (`appendEntry`), which RPC clients receive as `entry_appended`.
  - They cannot add RPC commands. `pi.events` stays inside the process.
- **Packages:** extensions are distributed as npm or git packages (`pi install npm:…`).
- **Permissions:** none. The README says pi runs with the permissions of the user who launched it.
- **Sessions:** a JSONL tree (header version 3) under `~/.pi/agent/sessions/`. Each entry has `id`, `parentId` and a timestamp. Assistant messages store the model, usage, cost and an optional `durationMs`. RPC exposes `get_entries` and `get_tree`.
- **pi-durable and Chord** are a separate, experimental durable runtime and object protocol. The pi CLI doesn't use them, and pi 1.2.0 removed the Chord dependency. Not relevant to axo.
- **pi-web** ([agegr/pi-web](https://github.com/agegr/pi-web)):
  - It embeds the SDK in its Next.js server: one Node process holds every session, and idle sessions are evicted after 10 minutes.
  - It imports private pi files by path to get MCP working (`lib/pi-sdk-internals.ts`) and sets a private field. This is upgrade-fragile.

## omp

- **SDK:** Bun-only (`@oh-my-pi/pi-coding-agent`, Bun 1.3.14 or newer). Same shape as pi's, with a much wider surface.
- **RPC:** a dialect that is **not** wire-compatible with pi. It has a `ready` frame, version 1 and 2 negotiation, and chunked frames. It also has host tools (`set_host_tools` / `host_tool_call`), host URI schemes, and subagent subscriptions.
- **Clients:** official RPC clients in Rust, Go and Python (`sdk/rust/omp-rpc`, with wire types generated from a schema).
- **Rust code:** an N-API add-on (`crates/pi-*`, `pi-natives`) that provides shell, search, AST, diff, edit and VCS functions to the Bun process. The agent itself is TypeScript.
- **Reusable crates:** `pi-edit` (hashline, ~25k lines of Rust, with no TypeScript fallback in omp), `pi-shell`, `pi-walker`, `pi-diff`, `pi-ast` and `pi-vcs`. They are MIT-licensed and usable from Rust directly.

## Memory measurements

PSS, idle, with empty profiles and no model calls, so these are lower bounds:

| Setup | PSS (MiB) |
|---|---:|
| `omp acp`, idle / 1 session / 3 sessions | 174 / 330 / 355 |
| `omp --mode rpc`, empty profile, ×3 | ~660 |
| `omp --mode rpc` with a real `mcp.json` (`rea` + `context-mode`), ×3 | ~2,159 |
| `pi --mode rpc`, ×1 | 85–102 |
| `pi --mode rpc`, ×3 | 160–209 |
| pi SDK, 3 sessions in one Node process | 84–119 |

### axo

| Mode / sampling point | 1 session PSS (MiB) | 3 sessions PSS (MiB) |
|---|---:|---:|
| Scripted, idle | 13.7 | 15.6 |
| Scripted, mid-turn | 14.5 | 16.9 |
| Live, idle | 91.7 | 91.7 |
| Live, mid-turn | 92.1 | Unavailable |

Measured on 2026-10-22 with `cargo run --release -p axo --example memory`: PSS from `/proc/self/smaps_rollup`, one process per mode/session-count group, idle after 500 ms and mid-turn after every session's first text or thinking delta; the live model was `zebes/qwen3.8-27b`.

The live three-session run timed out after 120 seconds: the server reports `--parallel 1`, so it cannot supply three simultaneous mid-turn streams. Its idle reading is retained; the missing mid-turn value is not estimated.

All measured values are clearly under three `pi --mode rpc` processes (160–209 MiB), meeting the [core design's memory target](../design.md#goals) for the measured cases; three simultaneously streaming live sessions remain unmeasured.

## Porting omp features onto pi

Estimates from reading omp's source. Line counts exclude tests.

| Feature | omp source (lines) | To port (lines) | Coupling | Estimate |
|---|---:|---:|---|---|
| Subagents and `@roles` | 21.6k | 2–3k basic, 4–6k full | Deep | 2–3 weeks |
| Plan mode | 0.7k | 0.6–0.8k | Moderate | 2–3 days |
| `orchestrate` keyword | 0.2k | 0.2k | Self-contained; needs parallel subagents | < 1 day |
| Todo tool | 1.3k | 0.8–1.1k | Moderate | 2–3 days |
| URL schemes | 8.5k | ~2k core, 8.5k all 19 schemes | Moderate | 3–5 days core, 2–3 weeks all |
| GitHub tool, `issue://` and `pr://` | 6.3k | ~4.2k | Moderate | 4–6 days |
| Browser / CDP | 35.8k | ~18k without terminal-only backends | Moderate; lives inside eval | 1–2 weeks |
| Eval (persistent JS + Python) | 21k | ~12.5k core, +2.5k subagent helpers | Core clean; helpers deep | 1 week core, +1–2 weeks helpers |
| Hindsight memory | 3.7k | 1.5–2k | Self-contained HTTP client | 2–3 days |
| Hashline edit | 25k Rust + 1.6k TS | 1.6k TS + reuse the crate | `read`, `edit` and `grep` share state | 3–5 days |

Two takeaways for axo:
- In Rust, `pi-edit` and the other omp crates are plain dependencies.
- Subagents become simpler when sessions are in-process objects: a subagent is a child session, and nothing needs parking or reviving.

## Rust harness landscape

No candidate is mature, minimal, embeddable and extensible at once. Many Rust "ports of pi" appeared in 2026; most are young single-maintainer projects or very large.

| Candidate | What it offers | Size | Health | Verdict |
|---|---|---|---|---|
| [genai](https://github.com/jeremychone/rust-genai) | Provider clients only; no agent loop | ~35k lines | 914★, active, one main maintainer | Leading choice for the provider layer |
| [Rig](https://github.com/0xPlaygrounds/rig) (`rig-agent`) | Host-driven, step-by-step agent run; pluggable stores; ChatGPT OAuth | Core ~139k, agent ~38k | 8.9k★, several maintainers | Fallback provider/loop foundation; wide, fast-moving API |
| [goose-agent](https://github.com/aaif-goose/goose) + `goose-providers` | Small state-machine loop (~3.7k lines), mature providers | ~49k with providers | Goose has 55k★, but the crate is alpha (since August) | Worth a look for the provider layer |
| [bigfish1913/pi-rust](https://github.com/bigfish1913/pi-rust) (`rpi-*`) | Closest to pi's shape: async hooks, storage trait, native plugin ABI | Loop 4.8k, providers 16k | 130★, two months old, one developer | Reference only |
| [nwyin/tau](https://github.com/nwyin/tau) | Smallest pi-style loop (604-line loop file) | ~30k total | One developer, inactive since April | Good reading; incomplete lifecycle API |
| [c4pt0r/pie](https://github.com/c4pt0r/pie) | Native OAuth implementations (Anthropic, Copilot, Codex) | ~75k | Unpublished, dormant since August | OAuth reference only |
| codex-rs | Embeddable in-process | Very large | Mature | Built around OpenAI Responses; wrong shape |
| pi_agent_rust | Real SDK | ~1.3M lines | Active | Nonstandard license rider excluding OpenAI and Anthropic; rejected |

The conclusion behind axo: the loop itself is small (tau's is 604 lines, goose-agent's ~3.7k). The hard parts are provider quirks, and sessions with compaction. So axo owns the loop and sessions, and uses a library only for providers.

## Plugin mechanisms considered

| Mechanism | Tools | Hooks | State / spawn | Installable | Isolated | Verdict |
|---|---|---|---|---|---|---|
| Compiled-in Rust | ✓ | ✓ | ✓ | ✗ | ✗ | Yes, for hosts and first-party features |
| Dynamic libraries (`.so`) | ✓ | ✓ | ✓ | ✓ | ✗ | No: no stable ABI, panics, crashes |
| Separate process over stdio | ✓ | ✓ | ✓ | ✓ | ✓ | Yes, as the Bun extension host |
| MCP | ✓ | ✗ | ✗ | ✓ | ✓ | Yes, as a tools-only adapter |
| WASM (wasmtime / Extism) | ✓ | ✓, async is clumsy | ✓ | ✓ | ✓ sandboxed | Not now |
| Embedded scripting (Lua, Rhai, QuickJS, `deno_core`) | ✓ | ✓ | ✓ | ✓ | ✗ | Not now; embedded JS remains possible later |
| Declarative files | Partly | Shell hooks only | ✗ | ✓ | ✓ | Yes, through the `.agents/` convention |

A stdio round trip costs well under a millisecond, while model calls take seconds. So an out-of-process host is no bottleneck for turn- and tool-level hooks. [INFERENCE: no per-token hooks are planned.]
