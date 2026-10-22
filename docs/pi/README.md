# pi reference notes

These are port references for axo's core. They are studies of pi's source, made on 2026-10-21 from [earendil-works/pi](https://github.com/earendil-works/pi) at commit `02726086d11c2911aa872959e308b90d69182392` (v1.3.0). `file:line` citations point into that commit.

| File | Covers | pi package |
|---|---|---|
| [`ai.md`](ai.md) | message model, streaming events, the Anthropic Messages and OpenAI Chat Completions adapters, auth, retry, overflow, cost | `packages/ai` |
| [`loop.md`](loop.md) | the agent loop, tool execution, steering and follow-up, abort, events, hook points, session orchestration | `packages/agent`, `coding-agent/src/core/agent-session.ts` |
| [`session.md`](session.md) | the JSONL session tree, context projection, branching and forking, compaction, branch summaries | `coding-agent/src/core/session-manager.ts`, `compaction/` |
| [`context.md`](context.md) | system prompt, context files, skills, prompt templates, config directories, built-in tools | `coding-agent/src/core`, `core/tools/` |

Each note has the same six sections: concepts, types, behavior, skip list, Rust port notes, and size. Claims marked `[INFERENCE]` were not verified against source.

[`../core.md`](../core.md) records what axo takes from these notes and where it deliberately differs.
