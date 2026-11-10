# Recurring mode reminders: cache mechanics and harness evidence

## Bottom line

**A tail reminder is cache-friendly only if the prefix that was actually cached survives the next request.** Append-only durable reminders preserve that property. An ephemeral reminder moved from yesterday’s tail to today’s tail removes content inside yesterday’s cached prefix: reuse stops before the old reminder, not merely before the new reminder. On Anthropic, if the only write was after that now-removed reminder, there may be **no matching conversation cache entry at all**—the 20-block lookup does not manufacture entries at arbitrary stable prefixes.

**Cheapest design for “present in every request, including after compaction”:** persist a mode-entry developer/instruction message once; preserve it in the live context; on compaction reconstruct it from separately persisted mode state and append it to the new context. Append an explicit exit message on leave. This is variant (d), not an instruction that must be duplicated every request. If fresh tail salience is a separate requirement, append **durable**, sparse reminders, or use an ephemeral suffix **after an explicit stable-history cache breakpoint**. Do not change tools/system text for mode gating. This recommendation is a derivation from the documented mechanics, not a measured benchmark.

## 1. Provider mechanics

### Anthropic Messages

Primary reference: [Prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching), especially `#automatic-caching`, `#how-automatic-prefix-checking-works`, `#cache-limitations`, and `#what-invalidates-the-cache`.

* Cache identity is the **entire exact prefix**, ordered `tools → system → messages`, through the marked content block. No independently reusable islands after a changed earlier token.
* **Explicit:** at most **four** `cache_control` breakpoints. A marker writes an entry **only at that endpoint**, not at all preceding block boundaries.
* **Automatic:** a top-level `cache_control: {type: "ephemeral"}` automatically places a moving breakpoint on the last cacheable block. This is opt-in automatic *placement*, not OpenAI-style always-on caching. Same thresholds, pricing, and lookup limits as explicit caching. The automatic breakpoint uses a slot within the four-breakpoint limit; mixing modes is permitted subject to documented conflict/limit rules.
* For reads, check the requested breakpoint and walk backward through at most **20 positions including the breakpoint** looking for a prefix a prior request **actually wrote**. A previously written endpoint must still exist unchanged and be in range, or be reachable from another explicit breakpoint. Current Claude API docs count a run of consecutive tool-use blocks as one position, and likewise a run of tool-result blocks. This is provider-specific; do not generalize that grouping to every cloud endpoint/version.
* **Current minimums are model-dependent**, not a universal 1,024: current docs list 512 for several newest models, 1,024 for Sonnet 4.6/4.5 and Opus 4.8 (among others), 2,048 for Opus 4.7/Mythos Preview, and 4,096 for Opus 4.6/4.5 and Haiku 4.5. Use the per-model table, especially when routing across providers. Below minimum, markers silently do not cache.
* Default TTL **5 minutes**, refreshed by reads; optional **1 hour**. Standard writes are **1.25× input** for 5m and **2×** for 1h; standard reads **0.1×**, with lower read multipliers on some newer models. TTL expiry/routing/model changes can defeat reuse independently of reminder placement.
* Changing tool definitions invalidates tools/system/messages; changing top-level system text invalidates that system prefix and subsequent messages; changing conversation content invalidates from that change onward. Other cache-affecting inputs include tool choice, presence of images, thinking/effort settings, speed, web-search/citation settings, and model-specific handling of thinking blocks. Preserve signed thinking blocks according to model rules.
* Important distinction: API `type: "ephemeral"` names the **cache lifetime policy**, not whether your application persists a reminder in conversation history.

**Tail vs middle vs system:** append-only tail content leaves all previous input untouched. Middle insertion changes every cumulative prefix after the insertion. System toggles put every conversation message behind a different prefix. None means the older cache entry is deleted; switching back can reuse an older compatible entry if still alive and reachable.

### OpenAI Chat Completions / Responses: historical rule versus current docs

Primary reference: [Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching); historical official description: [Prompt caching in the API, October 2024](https://openai.com/index/api-prompt-caching/).

**The familiar rule requested in the assignment is the historical automatic-caching contract:** exact common-prefix matching; cache begins at **1,024 tokens**, grows in **128-token increments**; supported Chat Completions and Responses calls get automatic caching without Anthropic-style `cache_control`. Stable instructions/tools first, varying content last. `prompt_cache_key` helps route related requests; it is not an independent cache ID that allows changed prefixes to match. A different message role or serialized wrapper can also change the prefix.

**Do not present 1,024/128 as universal current behavior.** The live official guide now distinguishes:

| Model family | Current documented behavior |
|---|---|
| GPT-5.6 and later | Minimum **1,024 visible input tokens**; implicit endpoint is latest eligible user message, last tool response of a group, or end of initial developer-message group; explicit block breakpoints also supported, at most four writes/request. Explicit-only mode writes nothing without markers. Implicit lookup includes up to 20 earlier eligible message endings plus initial developer block; explicit lookup covers first 2/latest 50 explicit breakpoints. Exact eligible-boundary reporting, not 128-token rounding. |
| Earlier models | Implicit only; internal breakpoint spacing and minimum now documented as model/settings-dependent; reported cached tokens rounded down to 128. GPT-5.5 specifically has 2,048-token breakpoint intervals. |

The current guide documents cache-write pricing/TTL for newer models (default 30m, optional Responses 12h) that differ from older automatic caching without write premium. Current `prompt_cache_key` is for separate cache accounting on GPT-5.6+; stable keys optimize routing on earlier models. Keys/routing never rescue a changed prefix. Availability of advanced parameters must be checked per endpoint/model; the explicit examples in the guide use Responses.

Thus the design conclusion is unchanged, but do not hardcode the historical granularity into axo’s provider-agnostic reasoning. For historical interval caches, common-prefix reuse can stop up to one interval before the first changed token. For endpoint-based current models, eligible/written endpoints matter, much like Anthropic.

### Gemini implicit caching (brief)

[Google’s official caching guide](https://ai.google.dev/gemini-api/docs/caching) (now redirects to AI Studio) says implicit caching is enabled by default on Gemini 2.5+, recommends putting large common content **at the beginning** and sending similar-prefix requests close together, and reports cached tokens in usage. Current table lists 2,048 tokens for Gemini 2.5 Flash/Pro and 4,096 for several newer models. It does not promise the same 128-token granularity or 20-block lookback as other providers. Current Gemini 4 guidance additionally says small additions under 2,050 tokens cause the previous request’s last 2,050 tokens to be reprocessed; do not assume suffix cost is exactly reminder length across providers.

## 2. Placement variants

### Precise accounting

Let **N** = total input tokens this request, **B** = longest still-identical **available and eligible previously written** prefix, **R** = reminder tokens, and **Δ** = new assistant/tool/user material since the prior request. Uncached/reprocessed fraction is **(N−B)/N**; if no eligible hit exists, it is **1**. Cache-read tokens still consume context and have a discounted, nonzero price. “Reprocessed” includes tokens written anew at cache-write rates; it does not mean the cache-read portion is free or absent from attention.

All costs below are **[INFERENCE] from the documented prefix algorithm**, assuming same model/settings, warm caches, eligible length, stable serialization, and no other history rewrite.

| Variant | Persistence | Reprocessing with suitable history checkpoints | Failure mode with only a tail checkpoint | Compaction survival |
|---|---|---|---|---|
| **a. Append reminder to latest user message and persist each copy** | Old reminders remain byte-identical in history. | Ordinary new suffix, roughly `Δ + R` when adding a fresh reminder; `B` can reach previous request endpoint. No reminder-induced loss of earlier history. Context grows by copies. | Normally works with automatic/moving tail marker if earlier endpoint remains in lookup range. Avoid modifying an already-sent message repeatedly: persist once at its original send position. | Not guaranteed by durability alone: summarization may omit it. Reinsert from mode state/preserve explicitly. |
| **b. Append to latest message per request, not persisted** | Last request’s insertion disappears or moves when the selected latest message changes. | Reuse stops **before the old insertion**. Reprocess any portion of that message after the last reusable checkpoint, subsequent material, and fresh reminder. If same latest user message remains selected through tool steps and identical reminder is regenerated at the same position, there is **no new divergence there** until that placement moves/changes. | If every write includes the removed reminder and no stable pre-reminder prefix was ever written, **all conversation caching can miss**, even though almost all text is identical. Static tools/system may still hit if separately marked. | Regenerated from mode state, so survives context rewrite operationally; history alone does not preserve it. |
| **c. Separate trailing ephemeral message every request** | Request shape `Hₜ · R`; next request `Hₜ · outputₜ · newₜ · R`. Old `R` is gone before output. | With a written endpoint at `Hₜ`, reuse all `Hₜ`; process approximately `Δ + R` plus wrappers/granularity loss. Cleaner to checkpoint than altering a message’s last text block. | Sole write at `Hₜ · R` is **not a prefix** of next request. Anthropic lookback cannot find an unwritten entry at `Hₜ`; may fall back to much older static checkpoint or none. | Regenerated from persisted mode state, independent of summary. |
| **d. Durable entry message, re-post after compaction** | One copy in current history; new copy only after context reset or actual state change. | Best ordinary append-only behavior: approximately `Δ`; reminder tokens are read from cache, with only one copy occupying context. Entry itself processes `R` once. | Moving tail checkpoint behaves normally, subject to lookback and threshold. | **Yes if the harness reconstructs it from durable mode state after compaction**. One message alone is not sufficient. |
| **e. Toggle system-prompt section** | Derived system instruction remains while active; no conversation copy required. | **Entry/exit:** loses every reusable suffix after changed system position; nearly whole conversation can be rewritten. **Subsequent unchanged active turns:** normal `Δ`-only behavior, not a whole miss each turn. | Entire message cache is behind changed system. Earlier tools/static-system checkpoint may survive. Reverting to still-warm old system can recover an old entry. | Yes if rebuilt from durable mode state, but mode transitions have broad cache cost. |

### Why breakpoint placement changes the answer

* **Last user message only:** if the reminder is part of that user message, the cached endpoint includes it. Persistent copies preserve hits; removing the old copy before the next user turn destroys that endpoint. On tool-loop requests the same latest user message may remain unchanged, making temporary append-to-user cheaper than append-to-*latest-message*.
* **Last two messages:** useful only if one marker lands on a prefix that will remain identical on the **next** request. Marking the preceding **persistent** history block before a separate ephemeral reminder can save it. Marking two endpoints both after the old transient insertion does not help. Message count is not content-block count.
* **Recommended ephemeral arrangement:** write/checkpoint the last persistent block first, append the ephemeral reminder after it. Keep that checkpoint discoverable on the next request and advance it with the durable history. A separate static tools/system checkpoint is a fallback, not a substitute for conversation caching.
* Example: prior input `S · U₁ · R`, current input `S · U₁ · A₁ · U₂ · R`. Exact common prefix ends at `U₁`, but an Anthropic marker only on `R` wrote `S·U₁·R`, **not** `S·U₁`. A marker on `U₁` from the prior request is what makes the larger hit possible. If `R` was durably preserved instead, current input is `S·U₁·R·A₁·U₂·R`, and the prior tail endpoint remains reusable.

## 3. What harnesses actually do

### Claude Code: official distributed JavaScript, pinned 2.1.1

The public GitHub repository does not provide the complete unminified core. I inspected Anthropic’s **official published package**, not third-party leaked/reconstructed source: [`@anthropic-ai/claude-code@2.1.1/cli.js`](https://unpkg.com/@anthropic-ai/claude-code@2.1.1/cli.js). This is minified; function identifiers below are locators for this pinned version, not stable APIs.

* `xm5` computes attachments, including `bm5(history, context)` for `plan_mode`; `RDA` yields them as `o9(attachment)`, which creates transcript items `{type:"attachment", attachment, uuid, timestamp}`.
* `vm5` scans history backward to a plan-mode/reentry attachment, counting qualifying assistant messages. `km5` counts plan attachments since the last plan-mode exit. These are **retained attachment items**, not a single suffix silently removed and regenerated at a different position on each request.
* Constants `_x2`: **`TURNS_BETWEEN_ATTACHMENTS: 5`**, **`FULL_REMINDER_EVERY_N_ATTACHMENTS: 5`**. First eligible attachment is full, next four sparse, then full again (attachments 1, 6, 11…). Thus “full every five turns” is inaccurate: spacing is five qualifying assistant messages per attachment, and full every fifth *attachment*. Subagents use separate rendering logic.
* `RF7` dispatches to `_F7` full / `jF7` sparse / `TF7` subagent reminders. Full text explicitly says plan mode is active and forbids modifications except the allowed plan file.
* API message normalization handles `case "attachment"` by `PF7(attachment)` and merges generated user content with the preceding user message when possible (`qF7`); otherwise it appends generated user messages. Therefore reminder text may accompany a user/tool-result turn, even though its internal transcript representation is a separate attachment. It is not a native API system role merely because it uses `<system-reminder>` tags.
* **Persistence qualification:** retained in the model/transcript history is verified by attachment generation, normalization, and backward history scans. I did not separately prove every disk journal serialization/resume path for this pinned bundle; do not overclaim raw JSONL format.
* Current official [Claude Code prompt-caching docs](https://code.claude.com/docs/en/prompt-caching) independently state that plan-mode instructions append as conversation messages, permission changes preserve the system/tools prefix, and `opusplan` is an exception because it changes models. They explicitly describe full prefix reuse and cache loss after compaction. [Glossary](https://code.claude.com/docs/en/glossary#system-reminder) identifies `<system-reminder>` as harness-inserted context.

Historical cross-check: official package [2.0.35/cli.js](https://unpkg.com/@anthropic-ai/claude-code@2.0.35/cli.js), `Gu6`, unconditionally returns a plan attachment whenever active. **Frequency is version-dependent.** Do not infer current cadence from an older prompt dump.

### OpenCode: old implementation differs materially from current main

**Historical v1.1.1, commit `dc25669b6e14ee7730ae9ab5d3d5e876e2111fb0`:**

* [`packages/opencode/src/session/prompt.ts#L1158-L1183`](https://github.com/anomalyco/opencode/blob/dc25669b6e14ee7730ae9ab5d3d5e876e2111fb0/packages/opencode/src/session/prompt.ts#L1158-L1183), `insertReminders`: finds **last user message** and pushes synthetic text `PROMPT_PLAN` into its parts; no `Session.updatePart` persistence call there.
* Loop reloads history via `MessageV2.filterCompacted(MessageV2.stream(sessionID))` (~line 274), invokes `insertReminders` (~line 512), then converts to provider messages. **Ephemeral request reconstruction**, variant **(b)**, not a durable newly written part. Repeated tool steps keep selecting the same last user message; new user turns move the injection point.
* Text is [`packages/opencode/src/session/prompt/plan.txt`](https://github.com/anomalyco/opencode/blob/dc25669b6e14ee7730ae9ab5d3d5e876e2111fb0/packages/opencode/src/session/prompt/plan.txt), `<system-reminder>` with strict read-only rules.

**Current dev commit inspected: `9c1ee9d22c3354287ac8f0289afc517f87fe9bd7`:**

* Reminder ownership moved to [`packages/core/src/plugin/plan.ts`](https://github.com/anomalyco/opencode/blob/9c1ee9d22c3354287ac8f0289afc517f87fe9bd7/packages/core/src/plugin/plan.ts#L14-L117).
* Agent creation/selection writes entry/exit reminders using `ctx.session.synthetic({text, resume:false})`.
* Per-request context hook scans the existing messages; injects **only if missing or stale**, places the message near the tail before the last user prompt, and **persists** the synthetic reminder. Source comment explicitly names compaction/reverts and keeping cached prefix warm. This is essentially **(d)**, not per-request duplication.

### Codex

Current commit `e1cc96712f9ad4f5404fe821b9eba7ff29598816`:

* [`codex-rs/core/src/context/world_state/collaboration_mode.rs`](https://github.com/openai/codex/blob/e1cc96712f9ad4f5404fe821b9eba7ff29598816/codex-rs/core/src/context/world_state/collaboration_mode.rs#L98-L177) renders collaboration instructions as a **developer** fragment, wrapped in collaboration-mode markers; `render_diff` emits nothing when the mode/model/instructions snapshot is unchanged, and emits an update when changed/unknown.
* [`codex-rs/core/tests/suite/collaboration_instructions.rs`](https://github.com/openai/codex/blob/e1cc96712f9ad4f5404fe821b9eba7ff29598816/codex-rs/core/tests/suite/collaboration_instructions.rs#L677-L803) asserts the **second request contains both old and new developer instructions** after a change, and exactly **one** copy after a no-op update. Later restart test (~lines 940–1005) asserts plan mode and exactly one instruction survive restart. These are source assertions I read, **not tests I ran**.
* Therefore not a recurring ephemeral tail message every API request; instructions persist in conversational context and changes append new instructions. Snapshot reconciliation prevents duplicate no-op instructions. Current implementation tracks retained fragments and unknown state explicitly; this is richer than just a static prepended developer string.

### omp / oh-my-pi

Current commit `98eacbbc8d1b98eb6582302f407c647c0fb38f6e`:

* [`packages/coding-agent/src/session/agent-session.ts`](https://github.com/can1357/oh-my-pi/blob/98eacbbc8d1b98eb6582302f407c647c0fb38f6e/packages/coding-agent/src/session/agent-session.ts#L8393-L8430), `#buildPlanModeMessage`, creates hidden custom `plan-mode-context` while enabled.
* Dispatch builds it anew and pushes it into the **messages delivered to `agent.prompt`** (~lines 9510–9514, 9685–9695), before the initiating prompt. This is per **host prompt dispatch**, not necessarily every tool-loop provider request.
* [`packages/agent/src/agent.ts`](https://github.com/can1357/oh-my-pi/blob/98eacbbc8d1b98eb6582302f407c647c0fb38f6e/packages/agent/src/agent.ts#L1290-L1300) appends message-end events to state; `appendMessage` pushes into conversation state (~line 1445). Thus these are durable-in-live-history messages rather than a moving transient request suffix.
* [`packages/coding-agent/src/session/messages.ts`](https://github.com/can1357/oh-my-pi/blob/98eacbbc8d1b98eb6582302f407c647c0fb38f6e/packages/coding-agent/src/session/messages.ts#L1185-L1205) converts custom messages to LLM messages; ordinary hidden host context is delivered through developer-message conversion. Additional plan-decision enforcement reminders explicitly append a developer message to **both** `agent` and `sessionManager` (~lines 13118–13130 in agent-session.ts).
* Compaction followed by a new host dispatch rebuilds active mode context from state. This supports the prior survey’s “per-prompt developer reminder,” but **not** an assertion that omp removes the old copy every request.

## 4. Instruction-following placement evidence

Anthropic’s official [Prompting best practices → Long context prompting](https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/claude-prompting-best-practices#long-context-prompting) recommends large documents first, above query/instructions/examples; says **queries at the end can improve response quality by up to 30%** in tests, especially complex multidocument input. It also recommends system roles for behavioral focus.

This is **not a controlled measurement of “plan reminder at tail versus developer/system reminder”**, nor proof that every-request repetition improves tool safety. No primary-source benchmark for that precise comparison was established here. Treat improved tail salience for this use case as **[INFERENCE]** and evaluate it separately from prefix-cache correctness.

## Recommendation for axo

1. Persist authoritative mode state outside model prose, so compaction cannot accidentally exit the mode.
2. Append a durable mode-entry instruction; append an exit/update instruction on transition. Do not rewrite the initial system/tool definitions.
3. After compaction, inject the active instruction once into the reconstructed context (or preserve it as an explicit retained item), and resume append-only history.
4. If freshness is necessary, append sparse durable reminders; for an ephemeral suffix, **cache the persistent history before it** and preserve eligible lookup access to that checkpoint.
5. Model swaps have their own cache namespace/cost: instruction-placement optimization cannot make a switch to a different model retain that model’s absent KV cache.

This gives the cheapest reminder token footprint and ordinary growing-prefix reuse, while keeping the active reminder present after compaction. No live provider measurements or repository verification commands were run; conclusions distinguish documented mechanics, inspected implementation, and deductions above.
