# Compaction triggers, reserve and keep-recent: how the agents compare, and defaults for axo

Short answer: pi, omp, OpenCode and Claude Code all fire at "window minus a fixed reserve". Codex and Gemini fire at a fixed share of the window, which is earlier on big windows. The tools converge on two numbers: about 20k tokens kept verbatim and a summary output cap of about 16–20k.

## Comparison

| Agent | Trigger | Reserve / buffer | Keep-recent | Summary output limit | Source |
|---|---|---|---|---|---|
| **pi v1.3.0** | `contextTokens > window − reserveTokens` | 16384; only decides when compaction starts | 20000 tokens, cut at a user or assistant message | The model's own output limit, clamped to the window (changed in #9512) | `coding-agent/src/core/compaction/compaction.ts:126-130,267-270`, `CHANGELOG.md:12,17`, `docs/compaction.md:32-35` |
| **omp** | `window − max(15%·window, reserve ?? 16384)`, so about 85% on windows ≥109k. Optional `thresholdPercent`, `thresholdTokens` or a per-model base window | 15% of the window, at least 16384 | 20000 | `min(0.8·reserve, 16384)`; turn prefix uses `0.5·reserve` | `agent/src/compaction/compaction.ts:201-224,322-418,904,2295` |
| **Claude Code** | `min(maxOut, 20k)` is held for the summary, then the trigger is 13k below the rest: `window − min(maxOut,20k) − 13k`. That is about 167k on a 200k window and about 967K on 1M | 20k summary + 13k buffer | Nothing kept verbatim. Re-reads up to 5 recent files (≤5k each), re-injects CLAUDE.md, plan and skills | 20,000 ("p99.99 of compact summary output being 17,387") | Docs: [model-config](https://code.claude.com/docs/en/model-config) ("about 967K"), [env-vars](https://code.claude.com/docs/en/env-vars), [context-window](https://code.claude.com/docs/en/context-window). Formula from a leaked-source mirror ([autoCompact.ts](https://github.com/alex000kim/claude-code/blob/main/src/services/compact/autoCompact.ts)) — reverse-engineered, treat with caution |
| **Codex CLI** | `auto_compact_token_limit` defaults to 90% of the raw window. A hard cap sits at `effective_context_window_percent` = 95% | 10%, or 5% at the hard cap | Local: user messages only, up to 20k (`COMPACT_USER_MESSAGE_MAX_TOKENS`). Remote: server-side `/responses/compact` returns an opaque item | Not set on the client for remote compaction | [openai_models.rs](https://github.com/openai/codex/blob/main/codex-rs/protocol/src/openai_models.rs), [context_window.rs](https://github.com/openai/codex/blob/main/codex-rs/core/src/session/context_window.rs), `compact.rs:63`, [API doc](https://developers.openai.com/api/reference/resources/responses/methods/compact). [#40095](https://github.com/openai/codex/issues/40095): 90% of 272k = 244.8k, which is 94.7% of the usable 258.4k |
| **OpenCode** | `count ≥ limit.input − reserved`, or `context − maxOutput` when the catalog has no input limit | `min(20k, min(limit.output, 32k))`; configurable as `compaction.reserved` | `clamp(25%·usable, 2k, 15k)` | Normal output cap, `OUTPUT_TOKEN_MAX` = 32000 [INFERENCE: no separate summary cap found] | [overflow.ts v1.18.31](https://github.com/anomalyco/opencode/blob/v1.18.31/packages/opencode/src/session/overflow.ts); `compaction.ts:28-33,115-120` and `transform.ts:18,1468`. Tool-output pruning (protect 40k, minimum 20k) is opt-in |
| **Gemini CLI** | `tokens ≥ threshold × tokenLimit`. Default is now **0.5**; it was 0.7 | 50% | Last 30% of history; older tool output collapsed to 2KB except the last 3 tool turns and read/grep tools | Not found | [chatCompressionService.ts](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/context/chatCompressionService.ts), [#12068](https://github.com/google-gemini/gemini-cli/issues/12068) |

### pi in detail

- **How context is measured.** pi takes the last assistant message's reported usage (`totalTokens`, or input+output+cacheRead+cacheWrite). It adds a chars/4 estimate for any messages after that one. Usage recorded before a later context edit or compaction is ignored and everything is re-estimated (`compaction.ts:140,196-262`).
- **When it checks.** Before a prompt, between tool results and the next response, and after each turn (`agent-session.ts:767-785,3109-3144`; fix #6879).
- **What the reserve does.** It is only the trigger; it buys no guaranteed room. pi-ai separately clamps each request's `max_tokens` to `window − estimate − 4096`, with a minimum of 1 (`ai/src/api/simple-options.ts:15-21`). Thinking budgets come out of `max_tokens`: high = 16384 (`simple-options.ts:71-108`). So right at the threshold the reply gets roughly 12k tokens, minus however wrong the estimate is.
- **Overflow recovery.** A provider overflow error, a "silent" overflow (input > window on a stop), or an early `length` stop each trigger one compact-and-retry. A second failure is surfaced to the user (`agent-session.ts:3063-3107`, `ai/src/utils/overflow.ts:142-176`).
- **History of the numbers.** 16k/20k have been the defaults since v0.12.7 (2025-12-04, `CHANGELOG.md:6109-6110`). Per-model overrides came with #8133. #9512 split the summary output limit off from the reserve.

### omp extras

- A per-turn pass drops stale tool results.
- Pruning runs before the threshold check: protect the last 40k, prune only if it saves ≥20k (`pruning.ts:77-82`).
- The trigger uses the larger of provider usage and the stored estimate (`compaction.ts:373`).
- Fallback order is `remote → snapcompact → handoff → shake → soft`. Snapcompact renders old history as images that a vision model reads back, with no LLM call (`compaction-methods.ts:12-51`).
- It can speculatively summarise in the background before the threshold, and can switch to a bigger model instead of compacting.

## Provider constraints

- **Anthropic.** Input, output and thinking all count toward the window; thinking comes out of `max_tokens`. On Claude 4.5 and newer, `input + max_tokens > window` is **accepted**, and the response stops with `model_context_window_exceeded`. Earlier models return a validation error unless you send a beta header. Input alone over the window is a 400. Current Opus, Sonnet and Haiku 5.x and 4.6+ models have a 1M window and up to 128k output ([context-windows](https://platform.claude.com/docs/en/build-with-claude/context-windows)).
- **OpenAI.** A separate input cap is documented:
  - gpt-5: 400k window, **272k max input**, 128k output.
  - gpt-5.6-sol: 1.05M window, 922k max input, 128k output.
  - In both cases max input = window − max output.
  - gpt-5.5 lists only 1.05M/128k.
  - Above 272k input, pricing is 2× input and 1.5× output ([models](https://developers.openai.com/api/docs/models/gpt-5.6-sol)).
  - [INFERENCE] I found no documentation on whether `max_output_tokens` is clamped. The binding limit appears to be the input cap.
- **vLLM (how Qwen is usually served locally).** It **rejects** `prompt + max_tokens > max_model_len` with a 400 ([vllm#42474](https://github.com/vllm-project/vllm/issues/42474), still reported in Oct 2026). Clamping `max_tokens` on the client is therefore required.
- **Qwen3 recommended output lengths.**
  - 235B Instruct-2507: 16,384 (262k native window).
  - Thinking-2507: 32,768, or 81,920 for hard tasks.
  - Coder-480B: 65,536.
  - Source: Hugging Face model cards.

## Quality evidence

- **Chroma, "Context Rot".** Across 18 models (GPT-4.1, Claude 4, Gemini 2.5, Qwen3), performance "grows increasingly unreliable as input length grows", even on trivial tasks ([report](https://research.trychroma.com/context-rot)).
- **Du et al., EMNLP 2025 Findings.** Even with perfect retrieval, accuracy drops 13.9–85% as input grows, while still within the advertised window ([paper](https://aclanthology.org/2025.findings-emnlp.1264/)).
- **Anthropic.** Its context-engineering post describes an "attention budget" with "diminishing marginal returns" and recommends compaction and clearing tool results ([post](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)). The Anthropic API compaction docs say quality degrades as context grows, and the API's own default trigger is 150k input tokens ([docs](https://platform.claude.com/docs/en/build-with-claude/compaction-threshold)).
- **Cost.** Codex #40095 measured that late compaction costs 6–14% more input throughput in tool loops.

## Recommendation for axo

**Effective window:**
```
W = min(context_window, max_input?)          // OpenAI-style separate input cap if the catalog has it
```

**Reserve, derived from the catalog:**
```
reply   = min(max_output, 32_768)            // one turn: thinking + text + tool args
reserve = min(reply + 4_096, W / 4)          // 4k = estimator slack (pi-ai's CONTEXT_SAFETY_TOKENS); W/4 guards tiny/local windows
```

**Trigger**, checked before every request on usage plus the estimated tail (pi's checkpoints):
```
compact if tokens > W − reserve
```

What this gives:

| Window | Trigger | Share of window |
|---|---|---|
| 200k | 163k | 82% |
| 272k | 235k | 86% |
| 1M | 963k | — (matches Claude Code's about 967K) |
| 32k local | 24k | 75% |

Why this shape:
- The reserve has to cover only the next request's output plus estimation error. Tool results get re-checked before the request after them.
- 32k covers Claude Code's measured p99.99 summary (17.4k) and high thinking (16k).
- pi's flat 16k is too tight once thinking is on — the cause of #9512 and `length` stops.
- omp's 15% wastes 150k on 1M windows.

**Request `max_tokens`:** always `min(max_output, W − estimate − 4096)`, as pi-ai does. Required for vLLM and for Claude models older than 4.5.

**Summary request:** use the model's output limit, capped at 32k (the same `reply` value), as pi now does. It fits automatically. The summarised span is at most `W − reserve − keep`, so input plus output fits whenever `keep + reserve ≥ reply + prompt`, which the defaults satisfy.

**Keep recent:** `min(20_000, (W − reserve) / 4)`. 20k is what pi, omp and Codex use; the /4 cap stops small windows from keeping 80% of history (OpenCode uses 25%).

**Recovery:** one compact-and-retry on an overflow error, a silent overflow, or `model_context_window_exceeded`, as pi does.

**Configurable:**
- `compaction.enabled`
- `reserve_tokens` (overrides the derivation)
- `keep_recent_tokens`
- per-model overrides keyed `provider/model`, as in pi
- one optional per-model `window` cap that applies to the trigger only

That last knob is the quality lever. It works like Claude Code's `autoCompactWindow` and omp's `baseWindowTokens`.

**Decision for you:**
- **Option A — no cap by default (my default):** it matches Claude Code and Codex and means fewer summaries.
- **Option B — cap 1M models at 400k:** the context-rot evidence and the >272k OpenAI pricing tier argue for it.

Percentage triggers aren't needed as a separate mechanism: the window cap covers that case and is one knob instead of two.