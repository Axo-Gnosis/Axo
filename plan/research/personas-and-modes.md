# Harness survey: personas and modes (2026-11-09)

|Harness|Root persona|Switch persona mid-session?|Plan/mode object|How a mode reaches the model|Restriction|
|---|---|---|---|---|---|
|Claude Code|`--agent X` / `agent` setting; the body replaces the system prompt; persisted on resume|no|permission mode, separate from agents|appended `<system-reminder>` messages; docs say mode changes keep prompt and tools, so they are cache-safe|permission check at call time; tools stay|
|Codex CLI|none (config / instructions file)|n/a|collaboration-mode mask {model, effort, developer_instructions}|hashed "world state" diffs appended as developer messages; `<model_switch>` notice|mostly prompt; a few tools error at call time|
|omp|none (SYSTEM.md / personality); agents are subagent-only|n/a|session mode (plan/goal/vibe/loop), journaled|per-prompt developer message; system prompt untouched|guard inside `write`/`edit`; plan model role on entry, restored on exit|
|DSH (DeepSeek Harness)|agent preset picked at new session, then fixed|no|plan-mode plugin|system-prompt section present only while on|none ("guidance, not enforcement")|
|OpenCode V2 / Kilo|agent with `mode: primary\|subagent\|all`, stored on the session|yes (Tab)|plan is a built-in agent with no `system` override|reminders appended near the tail, re-checked each request so compaction cannot drop them|permission rules at call time; a tool is removed only when wholly denied|
|Amp|mode = agent (model + prompt + tools)|no; fixed per thread for cache and cross-model reasons|none|n/a|n/a|
|Roo Code|mode (roleDefinition, tool groups, sticky model)|yes|Architect mode|system prompt and tools rebuilt each request (full miss)|tools removed and checked at call time|
|Cline (new), Crush, Gemini CLI, Aider|none or fixed|n/a|plan/act, coder/plan, approval mode|session or system prompt rebuilt (full miss)|tools removed (Crush, Gemini) or none|

Patterns:
- **Personas are a start-of-chat choice.** Almost everywhere the root persona is chosen when the chat starts and persisted with it.
- **Modes are a separate object in most harnesses.**
- **Cache-safe modes share one design:** append enter and leave messages, gate tools at call time, never change the system prompt or tool list, and re-check the reminder after compaction.
- **A model swap on entering a mode is the one accepted cache break.**
- **Mode state is journaled with the session.**
