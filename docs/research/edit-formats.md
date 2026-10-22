# How coding agents let models edit files — survey for axo

Scope: wire formats a model uses to change files, their cost, their failure modes, and the published evidence on which works best. Abbreviations: `S` = omp source at `/nix/store/lb6rqm446k7xw01z0lmp2mjz73hkpl4p-source`, `P` = pi at `/tmp/pi-src`. "Read cost" means tokens added to every file read so the format can work. "Edit cost" means output tokens per edit. Unmarked claims are sourced; `[INFERENCE]` marks my own reasoning.

## 0. TL;DR

- No format wins for every model. Every controlled comparison agrees on that: Aider 2023–24, Diff-XYZ (JetBrains 2025), Can Bölük's hashline benchmark (2026), SWE-Edit (Microsoft 2026). Which format is best depends on the model family, and above all on which format the model was **post-trained on**.
- Three formats are "native" to a vendor's training. Claude was trained on `str_replace` (Anthropic's text-editor tool). GPT-4.1/5/Codex were trained on `apply_patch` (V4A). Gemini has nothing special and does best with forgiving search/replace. Moving a model off its native format can cost double-digit points. Moving an untrained model *onto* a forgiving or anchored format can gain a lot.
- **Hashline** (omp) anchors edits to line references that the read tool hands out, plus a staleness check. The author reports the biggest gains on weak or non-frontier models, and gains of 0–8 pp on strong ones. The evidence is a single self-run benchmark. An independent re-run was mixed. omp itself sends 7 model families to `replace` by default instead of hashline.
- Recommendation: make hashline the default (as `design.md` already plans). Choose exactly **one** edit wire format per model family: hashline / `replace` / `apply_patch`, through the engines `pi-edit` already ships. Never expose two edit tools at the same time.

---

## 1. Exact-text replace (old/new string)

### 1a. pi `edit` (`oldText`/`newText`, batched)
- Schema: `{path, edits: [{oldText, newText}]}`. Each `oldText` must be unique and must not overlap the others. Every edit is matched against the *original* file, not applied one after another (`P/packages/coding-agent/src/core/tools/edit.ts:21-41`, `:152`).
- Matching: exact first. On a miss it falls back to fuzzy matching after NFKC normalization, stripping trailing whitespace, and folding smart quotes, Unicode dashes and Unicode spaces to ASCII (`P/.../edit-diff.ts:28-37`, `:207-245`). More than one occurrence is an error (`:328-331`). When fuzzy matching was used, unchanged lines keep their original bytes (`:296-298`, `:353-355`).
- Argument repair: some models (the comment names Opus 4.6 and GLM-5.1) send `edits` as a JSON *string* or as a single object; the tool re-parses both (`edit.ts:103-134`). This is first-hand evidence for Aider's "JSON-wrapped code hurts" finding (§9).
- It preserves BOM and line endings and holds a per-file mutation lock (`edit.ts:163-198`).

### 1b. Anthropic `text_editor` (`str_replace_based_edit_tool`)
- An Anthropic-defined, schema-less tool type. The current version is `text_editor_20250728`, for Claude 4 and later. Commands: `view` (optional `view_range`), `str_replace` (`old_str` must match exactly, including whitespace), `create`, and `insert` (`insert_line`, where 0 = top of file). `undo_edit` was dropped in `20250429` ([docs](https://platform.claude.com/docs/en/agents-and-tools/tool-use/text-editor-tool), version table). The tool definition costs 745–974 input tokens (same page).
- The docs say `view` returns line-numbered text and that those numbers are "essential" for `view_range` and `insert` (same page). So Claude is trained to *read* line numbers and to *write* exact old text.
- SWE-agent 1.x and OpenHands both use this exact tool contract; OpenHands' editor says "the tool parameters are defined by Anthropic and are not editable" ([openhands-aci editor.py](https://github.com/All-Hands-AI/openhands-aci/blob/main/openhands_aci/editor/editor.py)). SWE-Edit's baseline is this tool: GPT-5 reached 93.4% edit success. Kimi-K2, MiniMax-M2.1 and GLM-4.7 reached only 75.6–82.0% (§10).

### 1c. Claude Code `Edit` (and the former `MultiEdit`)
- Takes `old_string`, `new_string` and `replace_all`. Exact matching only, "doesn't use regex or fuzzy matching". The file must have been read first. `old_string` must be unique unless `replace_all` is set. A file changed since the last read can still be edited if `old_string` matches the current content exactly ([tools reference](https://code.claude.com/docs/en/tools-reference.md), "Edit tool behavior").
- `MultiEdit` (a batch of old/new pairs) no longer appears in the current tools table (same page). I found no changelog entry for its removal [INFERENCE: folded into `Edit`].
- Failure evidence: the hashline feature request (#25775) lists CRLF mismatches, tab-indented files that "cannot be edited at all", reformat-after-read failures, and 15-retry escape loops, with ~15–20% of first attempts failing in the requester's pipeline ([issue #25775](https://github.com/anthropics/claude-code/issues/25775)). The request was closed as stale, not addressed.

### 1d. Gemini CLI `replace`
- Schema: `file_path, instruction, old_string, new_string, allow_multiple`. The model must also write a natural-language `instruction` (definitions snapshot, `packages/core/src/tools/definitions/model-family-sets/default-legacy.ts:351-392`). The legacy prompt asks for ≥3 lines of context before and after. The Gemini-3 tool set drops that advice (`gemini-3.ts:358-390`). So Gemini CLI already ships **per-model-family tool declarations**.
- A four-stage matcher: exact → flexible (whitespace) → regex → Levenshtein fuzzy (≤10% weighted difference, where whitespace costs 0.1×; skipped when `old_string` is under 10 chars) (`gemini-cli/packages/core/src/tools/edit.ts:68-70`, `:321-348`, `:1404-1467`). If all four miss, a **second LLM call** (`FixLLMEditWithInstruction`) rewrites the edit using the `instruction` (`:554-591`). Every strategy used is logged to telemetry (`:323-338`).
- [INFERENCE] The fuzzy ladder plus an LLM fixer shows Google measured enough exact-match failures to build a two-level repair path.

**Token cost (1a–1d):** edit ≈ 2×(changed lines + uniqueness context); read cost is 0 (or line numbers only). **Failure modes:** whitespace, tabs and CRLF; escapes inside JSON strings; ambiguity (non-unique match); stale content after a formatter or another agent changed the file (exact match fails, which is safe but costs a retry). It is immune to line drift: no numbers are involved. **Suits:** Claude (trained on it), and every model in general. It is the most common format in training data, so it is the safest choice for unknown models.

## 2. OpenAI `apply_patch` (V4A)
- The input is a single freeform string: `*** Begin Patch`, then `*** Add File:` / `*** Delete File:` / `*** Update File:` (optional `*** Move to:`), then `@@ [class/def anchor]` hunks of ` `/`-`/`+` lines with 3 lines of context, then `*** End Patch`. It has no line numbers and can touch many files per call (`S/crates/pi-edit/prompts/apply_patch.md:5-62`; [GPT-4.1 prompting guide appendix](https://developers.openai.com/cookbook/examples/gpt4-1_prompting_guide)).
- Training: OpenAI publishes "a reference implementation of the apply_patch tool that we used as part of model training" (GPT-4.1 guide). The Codex prompting guide "strongly recommend[s] using our exact apply_patch implementation as the model has been trained to excel at this diff format" (quoted in [Vaughan 2026](https://codex.danielvaughan.com/2026/04/28/codex-models-third-party-harnesses-apply-patch-v4a-portable-agent/); not checked against the primary source).
- Codex today sends it as a **custom/freeform tool constrained by a Lark grammar**, described as "do not wrap the patch in JSON" (`codex/codex-rs/core/src/tools/handlers/apply_patch_spec.rs:5-27`). Codex also intercepts `apply_patch` heredocs sent through the shell (`.../unified_exec_tests.rs:441`).
- Matching is a tolerance ladder: exact → ignore trailing whitespace → trim both ends → fold Unicode punctuation (`codex/codex-rs/apply-patch/src/seek_sequence.rs:1-73`).
- Evidence: in Bölük's benchmark, non-OpenAI models fail badly on it (patch failure rates of 50.7% for Grok 4 and 46.2% for GLM-4.7; "patch is the worst format for nearly every model") ([blog](https://blog.can.ac/2026/02/12/the-harness-problem/)). In the other direction, OpenCode added `apply_patch` for GPT/Codex models only, keeping string replace for everyone else. Warp reports +3–5% after adopting it (both via Vaughan, secondary).
- **Cost:** edit ≈ changed lines × 2 + 6 context lines per hunk + headers; read cost 0. **Failures:** context lines copied wrong, missing `+`/`-` prefixes, duplicated context between nearby hunks, multiple `@@` contexts (Warp found a parser bug there, per Vaughan). **Suits:** GPT-4.1+/GPT-5/Codex only.

## 3. Aider's formats and benchmarks
Formats ([docs](https://aider.chat/docs/more/edit-formats.html)):
- `whole`: return the entire file in a fence.
- `diff`: `<<<<<<< SEARCH / ======= / >>>>>>> REPLACE` blocks.
- `diff-fenced`: the same, with the path *inside* the fence. Made for Gemini, which kept breaking the fencing.
- `udiff`: a unified diff with line numbers stripped, applied as search/replace.
- `editor-diff` / `editor-whole`: the same formats with a narrower prompt, for the Editor model in architect mode.

Current defaults (`aider/resources/model-settings.yml`, tallied): `diff` for almost every model; `diff-fenced` for every Gemini 1.5/2.5 entry; `whole` for weak models (grok-3-mini); `udiff` only for `gpt-4-1106-preview`.

Published results:
- **2023, Exercism, 133 tasks:** plain-text `whole` was best for every GPT-3.5/4 model. Function-call (JSON) variants were worse for all of them. GPT-3.5 `diff` fell to ~19% on the 0613 models, and GPT-3.5 sometimes put the entire file in SEARCH ([benchmarks](https://aider.chat/docs/benchmarks.html)).
- **Laziness benchmark, 89 refactors:** GPT-4 Turbo went from 20% with SEARCH/REPLACE to **61% with udiff**, and June GPT-4 from 26% to 59%. Prompting for "high-level" diffs (whole function blocks) cut editing errors by 30–50%. Aider's stated principles: familiar, simple, high-level, flexible to apply; avoid line numbers ("GPT is terrible at working with source code line numbers") ([unified diffs](https://aider.chat/docs/unified-diffs.html)).
- **Code in JSON (2024):** all 4 strong models scored worse when returning code through a JSON tool call. Strict mode didn't help. Sonnet lost score even with zero syntax errors, which suggests a reasoning cost and not just an escaping cost ([post](https://aider.chat/2024/08/14/code-in-json.html)).
- **Architect/Editor (2024):** o1-preview plus a separate editor model reached 85% (whole), against o1-preview alone at 79.7%. Sonnet paired with itself: 77.4 → 80.5%. GPT-4o: 71.4 → 75.2% ([architect](https://aider.chat/2024/09/26/architect.html)).

## 4. Line-range edits (SWE-agent, Anthropic `insert`)
- SWE-agent's original ACI: `edit start:end` plus replacement text, applied to a 100-line windowed viewer that prints line numbers. A linter rejects edits that introduce syntax errors. Ablation on SWE-bench Lite with GPT-4 Turbo: no edit command 10.3%, edit without linting 15.0%, edit **with linting 18.0%** ([paper, Table 3](https://arxiv.org/html/2405.15793v3)). The paper also finds viewer window size matters: full file 12.7%, 30 lines 14.3%, 100 lines 18.0%.
- **Cost:** edit is the cheapest of all (new lines + two numbers); read cost is the line-number prefixes. **Failure:** *line drift*. After any insertion every later number shifts, and a model that reuses stale numbers silently corrupts the file. Cursor and Aider both name line numbers as a core weakness (tokenizers merge digits; models miscount) ([Cursor](https://cursor.com/blog/instant-apply)). SWE-agent itself later adopted Anthropic's `str_replace` contract [INFERENCE from SWE-Edit/OpenHands lineage; not verified in SWE-agent source].

## 5. Whole-file rewrite
- Aider `whole` and every agent's `write` tool. Cursor: "fully rewriting the full file outperforms aider-like diffs for files under 400 lines". Reasons Cursor gives: more tokens give more forward passes to think, diffs are out of distribution, line numbers are hard ([Cursor](https://cursor.com/blog/instant-apply)).
- **Cost:** edit ≈ whole file every time. **Failures:** laziness (`// ... rest unchanged`); silent "cleanup" of unrelated code (GPT-4 deletes comments; per Cursor); output-length limits on big files. Never stale-unsafe in format terms, but it overwrites concurrent changes. **Suits:** small files, weak models (Aider uses it for grok-3-mini), creating files.

## 6. "Fast apply" models (Cursor, Morph, Relace, OpenHands draft editor)
- The frontier model writes a *lazy* snippet with `// ... existing code ...` markers. A small fine-tuned model merges it into the file using speculative decoding.
  - Cursor: Llama-3-70B fine-tune, ~1000 tok/s, nearly matches Claude-3-Opus-diff ([Cursor](https://cursor.com/blog/instant-apply)).
  - Morph: 7B, 10,500 tok/s at 96% accuracy (v3-fast) or 2,500 tok/s at 98% (v3-large) ([Morph docs](https://docs.morphllm.com/models/apply)).
  - Relace: ~96% accuracy, 4,300 tok/s. Claims search/replace failure rates of 10–15% drop to 4% with lazy edits plus apply, and that search/replace and udiff need 1.5–2× more output tokens than lazy snippets ([Relace](https://relace.ai/blog/instant-apply); vendor numbers, method unpublished).
  - OpenHands has an LLM-based "draft" edit tool ([search result for `llm_based_edit.py`](https://github.com/OpenHands/OpenHands/blob/main/openhands/agenthub/codeact_agent/tools/llm_based_edit.py); fetch 404'd, so unverified).
- SWE-Edit (Microsoft 2026) generalizes this: the main agent sends natural-language edit instructions to a GPT-5-mini "Editor" subagent. Results: +2.1 pp resolve rate, −17.9% cost, and edit success 93.4 → 96.9% for GPT-5 and **+12.8 to +18.3 pp** for Kimi-K2, MiniMax-M2.1 and GLM-4.7. An adaptive find-replace/whole-rewrite policy beat fixed find-replace ([arXiv 2604.26102](https://arxiv.org/html/2604.26102v2), Tables 1, 2, 5).
- **Cost:** small main-model output plus a second model call that reads and writes ≈ the whole file. **Failures:** two models must both get it right (Gauthier, quoted in #25775); ambiguous merges such as renaming a function and changing its body (Relace's own example); a network dependency or self-hosted model; non-deterministic. **Suits:** frontier planners on expensive tokens. Not a fit for a self-contained Rust library [INFERENCE].

## 7. Structural / AST edits
- ast-grep-style pattern→rewrite with metavariables (`$X`, `$$$`). omp ships `ast_edit`: `ops: [{pat, out}]` over paths, staged as a preview that must be resolved before files change (`S/docs/tools/ast-edit.md:20-43`). It is *discoverable*, not an essential tool (`:35`).
- omp hashline borrows the structure without the pattern language: `PUT N*` / `CUT N*` resolve "the block starting on line N" with tree-sitter (`S/docs/tools/edit.md:67-71`).
- **Cost:** tiny for codemods (one rule covers many sites). **Failures:** grammar coverage, patterns that need exact node shapes, and it can't express arbitrary local edits. **Evidence:** none comparative that I found. Best as an *additional* tool for renames and codemods, never the default editor.

## 8. omp hashline — origin, format, evidence

### Origin
Can Bölük (omp author), "I Improved 15 LLMs at Coding in One Afternoon. Only the Harness Changed." (12 Feb 2026, [blog.can.ac](https://blog.can.ac/2026/02/12/the-harness-problem/)). The thesis: every existing format makes the model *reproduce* content it saw. Hashline instead gives each line a stable, verifiable identifier at read time. Original form: `read`/`grep` emit `N:hh|content` (a 2–3 char content hash per line), and edits say "replace `2:f1`", "range `1:a3`–`3:0e`", "insert after `3:0e`". A hash mismatch means the file is stale, and the edit is rejected before it can corrupt anything.

### Published benchmark (self-run)
- Setup: random React files mutated with Babel (operator swaps, boolean flips, off-by-one, removed guards, …), with a plain-English bug description. Tools: read, edit, write. 16 models × 180 tasks × 3 runs, fresh session each time. The result is compared to the original before and after formatting. Cost ~$300 (blog).
- Results stated in the text: "patch is the worst format for nearly every model, hashline matches or beats replace for most, and the weakest models gain the most". Hashline beats patch in 14/16 models; the v2 revision improves further in 12/16. Examples:
  - Grok Code Fast 1: **6.7% → 68.3%** (vs patch).
  - GPT-5.1 Codex Mini: 60.0 → 77.5% (v2).
  - Gemini 3 Flash: +8 pp, to 78.3%.
  - MiniMax: more than doubled.
  - Grok 4 Fast: **−61% output tokens**.
  - Overall framing: 5–14 pp gains and ~20% fewer output tokens.
- The per-model chart is JS-rendered; I could not extract the full table. Code and per-run reports: `omp/packages/react-edit-benchmark` (now `S/packages/typescript-edit-benchmark`, which mutates TypeScript sources instead).

### Independent replication
[nwyin/edit-bench](https://github.com/nwyin/edit-bench) ([analysis](https://raw.githubusercontent.com/nwyin/edit-bench/HEAD/benchmark-comparison.md)):

| benchmark | model | replace | hashline |
|---|---|---|---|
| react-edit | Haiku 4.5 | 75.0 | 76.3 |
| react-edit | Sonnet 4.6 | 82.5 | 85.0 |
| edit-bench (Python) | Haiku 4.5 | 73.3 | 72.2 |
| edit-bench (Python) | GPT-5.4-mini | **95.0** | **62.2** |

The author attributes the gap to:
- omp's Prettier/whitespace normalization forgiving hashline indentation slips;
- Python's significant indentation;
- small samples (20–90 points);
- Haiku's 24.4% hashline *tool* failure rate in omp's own data.

Conclusion: neither benchmark shows a large hashline advantage on strong Anthropic models. The large wins are on models that fail at patch or replace.

**The same author shipped and then removed hashline in his Rust harness, tau** ([nwyin/tau](https://github.com/nwyin/tau)):
- **Added** in commit `71f80d2` (2026-03-19): `hash_file_read`/`hash_file_edit` with a `NUM#HASH` anchor on every line (the blog-era format, not omp's current one), selectable as `edit_mode = "hashline"` next to `"replace"`.
- **Removed** in commit `1dfcab7` (2026-04-02): *"edit-bench showed hashline doesn't improve edit success rates — real failures are hallucinated content, not whitespace mismatches. The dual-mode architecture added ~1,750 lines of complexity."*
- tau today ships only exact replace: `old_string`/`new_string`, which must match exactly once, with a fuzzy fallback (`coding-agent/src/tools/file_edit.rs:26-35`, `:131`, `:323`).

### What omp ships today (differs from the blog)
Hashline has evolved. In this snapshot:
- `read` emits `[path#TAG]` plus `N:text` rows, where TAG is **one 4-hex xxh32 tag per file snapshot**, not a hash per line (`S/crates/pi-edit/src/modes/hashline/format.rs:43-48`, `:97-110`; `S/crates/pi-edit/src/store.rs:79`, `:495`).
- Edits are a freeform patch language (`S/docs/tools/edit.md:45-65`; prompt `S/crates/pi-edit/prompts/hashline.md`, ~2 KB):
  - `PUT N.=M:` replaces lines N–M with `+` body rows (final content only, no `-` rows);
  - `PUT <N:` / `PUT >N:` / `PUT >$:` insert;
  - `PUT N*:` targets the tree-sitter block that opens on line N;
  - `CUT`, with named/anonymous registers for moves, and `REM` / `MV` for whole files.
- All line numbers refer to the *tagged original*, so hunks within one call never drift.
- Safety layers:
  - Stale tags trigger snapshot-chain recovery. It applies only when the recorded chain proves a unique safe result; otherwise the edit is rejected (`S/crates/pi-edit/src/modes/hashline/recovery.rs:22-45`; `edit.md:154`).
  - `edit.enforceSeenLines` rejects edits to lines the model never saw in a read (`edit.md:150`; `store.rs:39`, `:106-122`).
  - Every section is parsed and validated before any write (`edit.md:145`).
  - Overlapping ranges, no-op loops (the third identical no-op is an error) and diff contamination are rejected (`edit.md:152-155`, `:168`).
  - A syntax regression produces a warning and optional auto-repair, not a rollback (`edit.md:141`).
- When the model is constrained to a grammar (OpenAI custom tools), omp emits a Lark grammar (`S/crates/pi-edit/grammars/hashline.lark`; `S/packages/coding-agent/src/edit/index.ts:404-407`). A compact prompt variant exists for selected models (`index.ts:164-171`; `hashline-compact.md`).
- I found **no published benchmark of this current snapshot-tag/PUT-CUT format**. The blog numbers are for the per-line-hash v1/v2.

### All five modes omp ships (`S/crates/pi-edit/src/modes/mod.rs:18-28`; `S/docs/tools/edit.md:122-131`)

| mode | wire | anchoring |
|---|---|---|
| `hashline` (default) | `input` string, `[PATH#TAG]` sections | snapshot tag + original line numbers + tree-sitter blocks |
| `replace` | `path, old_string, new_string, replace_all` | exact text, then a fuzzy ladder (0.95 / 0.92 / 0.8 thresholds, `S/crates/pi-edit/src/fuzzy.rs:15-38`) |
| `patch` | `{path, edits:[{op, rename?, diff}]}` | unified-diff hunks with optional `@@ anchor` substring, per file |
| `apply_patch` | `input` string, Codex V4A; wire name `apply_patch` | context lines + `@@` scope, as in Codex |
| `sloppy` | `*** Edit File:` / `*** Find` / `*** Replace` / `*** Insert Before` / `*** Insert After`, with `…` wildcard captures | find-text anchors, no tags |

Selection (`S/packages/coding-agent/src/utils/edit-mode.ts:20-43`): a per-model `edit.modelVariants` substring map, then `PI_EDIT_VARIANT`, then the `edit.mode` setting, then `hashline`. A settings-derived hashline is **silently downgraded to `replace`** for Kimi, MiMo, MiniMax, DeepSeek, StepFun, Codex Spark and GLM 5.3 Flash, unless `PI_STRICT_EDIT_MODE` is set. [INFERENCE] This is the author's own operational evidence that hashline does not suit every family. Note that MiniMax "more than doubled" with hashline v1 in the blog, but is now routed to `replace`.

### Cost, failure modes and fit
- **Cost:**
  - Read: v1 added ~3–4 tokens per line (hash + separator) to *every* read. The current format adds the `N:` prefix (~1–2 tokens/line) plus one tag per file [INFERENCE from the formats].
  - Edit: new lines + a short header. No old text, no context, which is the lowest of the anchored formats.
  - Prompt: ~2 KB versus ~1.2 KB for `replace` (`wc -c` of `S/crates/pi-edit/prompts/*`).
- **Failure modes:**
  - The model reuses line numbers from an older read after an edit. That is a stale tag: recovered, or rejected with a re-read hint.
  - Off-by-one ranges that swallow a closing brace. The rules forbid starting or ending mid-block (`hashline.md:13-14`).
  - Indentation in the body is still verbatim. This is the edit-bench Python failure.
  - Models leak unified-diff habits (`-` rows, `@@`).
  - Block anchors on unsupported grammars.
  - It needs the read tool to cooperate (tags plus seen-lines), so the edit format is coupled to `read`/`grep`.
- **Suits:** non-Anthropic, non-OpenAI frontier and mid-tier models that fail at exact reproduction (Grok, Gemini Flash, GPT-*-mini outside Codex). It is neutral to slightly positive on Claude, per the self-run data.

## 9. Cross-cutting evidence
- **Format ≠ capability, and the vendor's training dominates.** Aider 26 → 59% for GPT-4 on format alone. Grok Code Fast 6.7 → 68.3%. Codex models degrade outside `apply_patch` (Vaughan/OpenCode).
- **Diff-XYZ (JetBrains, Oct 2025):** search-replace is the best diff representation for *larger* models on most tasks, with structured udiff slightly behind. Small open models gain little from any format ([arXiv 2510.12487](https://arxiv.org/abs/2510.12487)).
- **JSON nesting hurts.** Aider's code-in-JSON result. Bölük's follow-up: "reliability degrades with nesting × heterogeneity × cleverness"; arrays and objects force escaped JSON, while flat primitive params or freeform strings don't ([Minutiae of Tool-calling](https://blog.can.ac/2026/08/03/the-minutiae-of-tool-calling/)). pi's `edits[]` array is exactly the shape he ranks "OK, not best". omp's hashline, apply_patch and sloppy modes all take one flat `input` string.
- **Guardrails pay off.** SWE-agent linting +3 pp. Staleness checks: hashline tags, CC read-before-edit.
- **Separating "what" from "how" helps weaker formatters** (Aider architect, SWE-Edit), at the price of a second model.

## 10. Comparison table

| Format | Read cost | Edit output cost | Stale-file safety | Whitespace sensitivity | Ambiguity | Line drift | Best-fit models | Evidence |
|---|---|---|---|---|---|---|---|---|
| Exact replace (pi, CC `Edit`, `str_replace`) | 0–line nums | 2×(change+ctx) | match fails safely; CC allows if still exact | high (fuzzy fallback in pi/Gemini/omp) | needs unique ctx | none | **Claude** (trained), unknown models | CC #25775 failures; SWE-Edit baseline 93.4% GPT-5, 75–82% Kimi/MiniMax/GLM |
| Gemini `replace` + fixer | 0 | 2×(change+3 ctx)+instruction | same | low (4-stage + LLM fix) | same | none | Gemini | design only (`edit.ts:321-348`) |
| V4A `apply_patch` | 0 | change×2 + 6 ctx + headers | ctx mismatch fails | low–med (4-level seek) | `@@` scopes | none | **GPT-4.1+/5/Codex** | OpenAI trained on it; 46–51% failure for Grok 4/GLM-4.7 (Bölük) |
| Aider SEARCH/REPLACE (`diff`/`diff-fenced`) | 0 | 2×(change+ctx) | fails | med (flexible apply) | first match | none | most; fenced for Gemini | Aider leaderboard defaults; Diff-XYZ: best for large models |
| udiff (no numbers) | 0 | ~2×change + ctx | fails | med | hunk ctx | none | GPT-4 Turbo (laziness) | 20 → 61% laziness bench |
| Line-range (SWE-agent) | line nums | change + 2 ints | **none** (silent corruption) | n/a | none | **high** | historical | 15.0% / 18.0% w/ lint (SWE-agent) |
| Whole file | 0 | whole file | overwrites | n/a | none | none | weak models, files <~400 lines | Cursor; Aider 2023 |
| Fast apply (lazy snippet + apply model) | 0 | snippet + 2nd model ≈ file | depends | low | model guesses | none | any planner | vendor claims 96–98%; SWE-Edit +12–18 pp edit success on open models |
| AST rewrite (ast-grep) | 0 | pattern + out | pattern re-match | none | pattern | none | codemods only | none comparative |
| **Hashline (omp, current)** | `N:` + tag/file | **body only** | **tag + snapshot recovery + seen-lines** | body verbatim only | none | none within call; stale tag across calls | non-trained families, small/fast models | self-run: +5–14 pp, 6.7 → 68.3% Grok Code Fast; independent: ±1 pp Haiku, −33 pp GPT-5.4-mini (Python) |

## 11. Recommendation for axo

1. **Default edit tool: hashline via `pi-edit`, as `design.md:50`/`core.md:223-224` already plan.** It is the only format that removes *both* classes of failure: reproducing text (whitespace, escapes, ambiguity) and staleness (tags + seen-lines + recovery). It also has the smallest edit output. It is also the format axo gets for free with the dependency, and it is battle-tested in omp's daily use across many providers.
   - Caveat for Toph: the public evidence is one self-run benchmark of an *older* hashline. One small independent re-run contradicts it on GPT-5.4-mini and Python. There is no published number for the current PUT/CUT format.
2. **Support more than one format, but exactly one per model family at a time.**
   - Never expose two edit tools together. #25775 describes the "two tools problem", and models pick inconsistently.
   - Route like omp/OpenCode/Gemini CLI do:
     - `hashline`: default for every family not listed below;
     - `replace`: the families omp already downgrades (Kimi, MiMo, MiniMax, DeepSeek, StepFun, Codex Spark, GLM Flash). Consider Claude too, if a smoke run shows hashline isn't a win there: Claude was trained on `str_replace`, and omp's own data shows only +1–2.5 pp for it;
     - `apply_patch`: OpenAI GPT-4.1+/GPT-5/Codex. It is their trained format; send it as a freeform tool with omp's Lark grammar when the provider supports custom tools.
   - This costs almost no axo code. `pi-edit` already has all five engines behind `engine_for(mode, …)` (`modes/mod.rs:12-28`). axo only adds a model→mode table (port `resolveEditMode`, ~25 lines) and swaps the schema and prompt per mode. The `read` output must follow the mode: tags and `N:` rows only in hashline.
3. **Skip `patch` and `sloppy` initially.** They are omp-specific with no evidence of advantage, and `replace` plus `apply_patch` already cover their use cases. Easy to add later since the engines exist.
4. **Keep `write` for creation and whole-file rewrites.** Defer `ast_edit` to an optional extension (it is useful for codemods, but no evidence makes it a default).
5. Skip fast-apply / editor subagents for now. They need a second model and a network service, which is wrong for an embedded library default. SWE-Edit shows the pattern is a good *extension* (a subagent role) if axo later wants cheap or open models to edit reliably.
6. Make the routing table configurable per model (`edit.modelVariants`-style) so it can follow evidence. If routing decisions start to matter, omp's `typescript-edit-benchmark` package is a ready-made harness to re-run against axo. This is an eval run, not a unit test.
