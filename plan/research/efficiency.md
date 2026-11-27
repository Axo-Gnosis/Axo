# Token and context efficiency

Study of 2026-11-26. Inputs to [Track 8](../plan.md#track-8-token-and-context-efficiency). Labels: **Decided** is settled; **Rejected** was checked and dropped. Before the evaluation, **Direction** marked a proposal still under discussion and **Open** an item not yet discussed; after the [evaluation of 2026-11-26](#evaluation-results-2026-11-26) and the [decisions by Toph](#decisions-by-toph) every inventory item is Decided or Rejected. Claims marked [INFERENCE] were not verified directly.

## How this was gathered

- **Claim vetting.** An earlier agent's audit produced about 60 claims about axo, Axome, the axophyte extension and the user prompts. Seven read-only verifiers checked each claim against the code. Three adversarial skeptics then tried to refute the survivors and re-examined the dismissals. Provider behaviour was checked against provider documentation.
- **Session measurements.** 209 deduplicated Axome journals (68 root and 141 child sessions, 6,278 requests), 2026-11-13 to 11-26:
  - Sources: `~/.local/share/axome/axo/sessions/` plus `sessions.bak-20261124-205237/`, top-level `*.jsonl` only. When a name appears in both trees, the larger file wins. Forked sessions copy their parent's entries; 848 duplicate entries were dropped, keyed by `id|timestamp`.
  - Requests by family: Claude 3,139 (Opus 5.5 / Fable 5.2), GPT 2,801 (`openai-codex`), DeepSeek 122, Qwen (`zebes`) 140.
  - Text tokens are estimated as chars/4; usage figures are provider-reported. Images are counted, not sized; ≈1.4k tokens each is an assumption. For Claude, chars/4 runs ≈1.6–1.7× low in Claude's own tokens (see [Measurement notes](#measurement-notes-and-corrections)).
  - "Resident-weighted" means tokens × the number of later requests still carrying them. The resident total, 251.6M token-requests, is about half of the 506.5M cache-read tokens, a plausible scale.
  - Bash commands were bucketed by command shape with a pattern classifier, spot-checked on about 80 commands. A pipeline headed by a view/search/list command is a **unit**; a **primary** call holds only units (1,633 calls), a **mixed** call holds units plus other commands (613).
- **Harness survey.** Pinned to versions and sources:
  - omp 19.2.0 (local source)
  - pi 1.6.0 (local source; v1.6.1 released but not inspected)
  - Octet `main` at [`4eb91c22`](https://github.com/skaft-software/octet/commit/4eb91c22f02bfbf2eb1bebddfbeefe7df83bc996)
  - Claude Code (code.claude.com docs and [Piebald prompt extractions](https://github.com/Piebald-AI/claude-code-system-prompts), v2.1.331)
  - Codex `main` (not pinned)
  - OpenCode v1.0.0 / v1.1.30
  - Gemini CLI v0.10.0
  - Aider v0.86.0
  - SWE-agent v1.0.0 and its paper (arXiv 2405.15793v3)
  - Cline v3.20.0
  - Roo Code v3.25.0
  - Cursor (public docs only)
  - Amp: only a third-party reverse-engineering exists (decodeclaude.com); everything about Amp is unverified and excluded from the comparisons.

## Principle (Decided)

Token and context efficiency is axo's job, not host policy. axo ships efficient defaults that work with zero host code; hosts get settings and hooks to tune, replace or disable them. Recorded in [`AGENTS.md`](../../AGENTS.md) under Embedder-First Design.

Ownership follows from it:
- **axo:** everything about tools, guidance, caching, clearing and compaction.
- **Axome:** only its own surface — its host tools, bundled agents and modes, and its repository docs.
- **axophyte and `~/.config/axome`:** only the user's personal guidance and workflows.

### Defaults that changed between drafts

The earlier ownership draft (`efficiency-ownership`) set three defaults differently from this document. Recorded here so the change is not silent.

| Setting | Ownership draft | This document | Why |
|---|---|---|---|
| Built-in bash-view blocking `ToolPolicy` (A1 #5) | Blocks pure 1:1 views and searches by default | Setting, **off by default**, by Toph's direction in discussion; the evaluation confirmed it and narrowed it to commands that are a single tool-equivalent unit | No surveyed harness enforces by default; blocking costs a round-trip and catches legitimate commands; fix the causes first. If blocking pays off after A1, the axo default flips; it is not a personal override (P5) |
| OpenAI `reasoning.context: current_turn` (A18) | Default where supported | **Rejected**: keep `auto` | Live tests and a journal replay show `current_turn` costs ≈10× what it saves (A18) |
| Clearing thinking from completed turns on keep-all models (A4) | Default on | **Rejected** | On Opus 5.5 and Fable 5.2 every clearing pass already drops later thinking through `drop_block`, so stripping saves 0.0%; on OpenAI it equals `current_turn` (A4) |

## Where the tokens go

| Measure | Value |
|---|---|
| Usage totals | input 7.74M, cache read 506.5M, cache write 18.85M, output 3.62M |
| Cache hit ratio, all providers | 95.0% (codex 94.4%, anthropic-subscription 96.1%, anthropic-auth 93.7%, deepseek 98.5%, zebes 94.6%) |
| Root vs child sessions | root 3,442 requests, 376M cache reads, 15.4M writes; child 2,760 requests, 130M reads, 3.45M writes |
| First-request prompt (≈ system + tools prefix) | root p50 10.8k, child p50 8.4k |
| Anthropic cost in input-equivalents (read 0.1×, write 1.25×, output 5×) | cache reads 50%, cache writes 32%, output 18% |
| Anthropic cache writes caused by the 5-minute TTL expiring (gaps of 5–60 min) | 38% (7.24M of 18.85M); 29 user idle, 20 parent in `wait`, 7 in `ask`, 2 other long tool |
| Tool-result tokens by tool | `read` 49%, `bash` 35%, `grep` 6%, `web_fetch` 3% |
| Resident-weighted tool volume | `read` 52%, `bash` 33%; write/edit call arguments add 13% on top |
| Prompt size per request | p50 61k, p90 210k, mean 86k, max 363k |
| Peak prompt per session | p50 53k, p90 127k, max 363k; 36 sessions over 100k, 15 over 150k |
| Tool-using assistant messages with one call | 79%; 20% multi-call for both Claude (1.29 calls per message) and GPT (1.69) |
| Edit failures | 15.4% (134 of 871; Claude 74 of 471, GPT 59 of 385): 89 "not read / changed since read", 18 "could not find", 11 ambiguous, 3 overlap, 13 other |
| Bash head+tail truncations | 13 of 3,025 results; 111 more long-line cuts (269k tokens) |
| `read` calls hitting the 2000-line / 50 KB cap | 40 of 2,399 |
| `grep` / `find` limit hits | grep 70 of 582 (12%), holding 172k tokens; find 16 of 123, holding 27k |
| Images | 98 in tool results (browser 74, read 24) plus 2 in user messages; ≈137k raw, ≈3M resident |
| Clearing passes / compactions | 5 / 11 (8 manual, 3 threshold); all clearing at ≈354–363k tokens |
| Child reports | ≈2.5% of carried volume |

The tool output caps almost never bind. The money goes to three places:
- cache rewrites after idle gaps;
- serial round-trips over a large prompt (each one re-reads ≈86k cached tokens, ≈8.6k input-eq);
- the agent leaving the `read`/`edit` contract by viewing files through bash.

### Anthropic cache misses by cause

A miss is a request with cache read < 50% of the previous prompt and cache write > 5k.

| Cause | n | Cache-write tokens |
|---|---|---|
| First request of a session (cold) | 70 | 633k |
| Gap 5–60 min, no other event (5-minute TTL) | 58 | 7.24M (38%) |
| Gap > 60 min | 3 | 441k |
| Thinking / mode / agent change | 5 | 976k |
| Model switch | 6 | 664k |
| Clearing (`context_edit`) | 5 | 783k |
| Compaction | 4 | 152k |
| Other (< 5 min, no event) | 2 | 132k |

- Each miss rewrote p50 113k, p90 232k tokens.
- TTL-miss gaps: p50 10 min, p90 23 min, max 48 min. Of 3,068 inter-request gaps, 74 were over 5 min and 3 over 60 min.

### Reads, edits and arguments

- **Reads:** 901 whole-file (2.18M tokens, p50 1,255) and 1,498 ranged (1.84M, p50 769); 229 URI reads; 55 errors.
  - 361 same-hash re-reads of a path, only 123 overlapping lines already returned (≈26.5k tokens, ≈15.9k of them still visible). 16 re-reads had ≥90% of lines already seen (17k). 41 identical-argument re-reads in one run (11k).
  - Confirm-reads within 1–2 requests of an edit: 29 (43k). 131 reads followed an edit/write as the previous touch of the path, at any distance.
- **Edit failures:** of the 89 stale/not-read failures, 51 came after a bash-only view, 25 after a `read`, 13 with no prior view. Next call after a failure: 75 ranged reads, 8 whole reads, 23 edit retries, 13 reads of another path. 114 of the 134 failures were followed by a successful edit of the same path.
- **Arguments:** 1.31M tokens in total. Edit 412k (31.4%, 19.15M resident), write 356k (13.53M resident), bash 261k, yield 64k (p50 1,240), spawn 62k (3.41M resident). 85 write/edit calls had arguments over 2k tokens, together 342k.

### Thinking vs visible text

| Family | Messages | Thinking blocks | Visible thinking | Replay payload (signed/encrypted) | Text | Tool args | Billed output |
|---|---|---|---|---|---|---|---|
| Claude | 3,139 | 2,025 | 355k | 1.37M | 107k | 762k | 2.58M |
| GPT | 2,801 | 1,353 (164 empty) | 13k | 1.78M | 128k | 497k | 865k |
| DeepSeek | 122 | 111 | 69k | 1.5k | 3k | 42k | 133k |
| Qwen | 140 | 135 | 24k | 1.8k | 5k | 11k | 48k |

- Thinking/text ratio per session: p50 0.37, p90 3.64.
- Visible thinking from completed turns, resident-weighted: Claude 7.9M, GPT 0.1M, others 1.6M.

### Prompts and agents

- **Root prompt composition** (always-on, ≈5.0–5.5k tokens): project AGENTS.md ≈55% (axo ≈2.9k at the time, 2,094 tokens in the current file per W6; Axome ≈2.3k), WORK ≈28%, schemes ≈300. Mode bodies arrive as `mode_notice` messages: plan ≈570, review:adversarial ≈1.3k, decomplect ≈2.6k, decomplect:staged ≈1.5k. axo AGENTS.md (1.6–2.0k tokens) was carried in 7 roots, 886 requests.
- **Children:** every child carries WORK = 1,328 tokens, ≈3.7M cache reads (0.7% of all) plus ≈1.66k written per child session. Tool schemas: task p50 2,344, scout 1,853, workflow scouts ≈1.2k; root 4,318.
- **Spawn listing:** the root spawn schema is ≈1,456 tokens, of which the 15 workflow-agent lines are ≈529. Listed in 36 of 61 roots, 2,308 requests, ≈1.2M cache reads. One mis-spawn in 31 workflow-agent spawns (review-adversarial-quality-scout under the orchestrate keyword, no mode or skill).
- **Child reports:** scout p50 1,267 (max 4,167); decomplect-scout p50 4,863 (max 5,408); decomplect-skeptic p50 1,395; task p50 436; mid-run notes: task p50 100, scout p50 218. `send` calls: root→agent 147 (108 in one session), child→parent 217, child→agent 16.

### Not measured

- memory-writer sessions are not persisted. W7 measured X2's volume from `axome.db` instead (18 captures and at most 28 writer sessions in ≈31 h); writer output quality is still unmeasured.
- The Axome AGENTS.md appeared only in the newest session, so X4's first estimate (≤1.9M cache reads over 1,491 Axome root requests) was an upper bound. W7 replaced it with a steady-state projection: 921 tokens, ≈0.86M cache reads per two weeks.
- Edited `tool_use` input in signed earlier turns (A4) was settled from documentation, not a smoke test: Anthropic treats it like any earlier-message change (accepted; later thinking dropped under `drop_block`), and Gemini validates signatures only within the current turn [INFERENCE: untested]. OpenAI accepts it (live Test C).
- The `ToolHooks` question is answered from code (see [Hook and ledger facts](#hook-and-ledger-facts)): a host can veto a bash call before it runs, but cannot rewrite it.

## Bash as file viewer and search

### Measurements

- **Bash share for Claude.** Claude used bash for 50–90% of file views and 93–100% of searches, depending on model and role. GPT root sessions viewed files through bash 0% of the time. Over the whole period, Claude called the `grep` tool 50 times and `find` once.
- **Volume.** Bash as a file viewer: 710 calls, ≈1.04M tokens (≈26% of `read`'s volume). Bash as search: ≈895k tokens, 1.8× the `grep` tool. Bash results over 2k tokens: 405 (58.7% of bash tokens); over 5k: 98 (26.9%). Top commands by tokens: `sed` 471k, `rg` 412k, `cat` 350k, `grep` 347k, `git diff` 239k (109 calls), `nl` 225k.
- **Most common shapes.** Nearly all map one-to-one onto an existing tool:
  - `sed -n 'N,Mp' F` — 928 pieces
  - `grep -n P F | head` — 356
  - `grep -n P F` — 244
  - `cat F` — 201
- **Compound commands.** 74% of primary view/search calls were compound (1,204 of 1,633 calls; 1.51M of 2.09M tokens), holding 3,895 units (p50 3, max 13 per call). In 231 of them (283k tokens) every unit is tool-equivalent today. Across primary and mixed calls:
  - 127 calls viewed ≥2 ranges of the same file (300 ranges);
  - 412 calls viewed more than one file (87 viewed ≥4);
  - 141 calls searched a file and viewed it in one go;
  - 33 calls ran `wc -l F` next to a ranged view.
  - Separately, 540 search **units** had several paths (196 of GPT's 471 search units).
- **Other shapes.** `wc -l` 132 units; `ls` one level 273 units, `ls -la` 61, `find -maxdepth` 59.
- **Stated reasons.** Agents gave two: line numbers to cite, and batching ("I'll read those files in parallel", followed by one bash command). GPT's `nl -ba` comes from scout prompts that ask for `path:line` citations: 195 of 294 line-numbered units are GPT scouts.
- **The guideline barely moves Claude.** With the `read` guideline present, Claude's view share is 65–66%; without it, 74–87%.
- **By project** (view / search share): axophyte 76% / 98%, Axome 52% / 91%, axo 30% / 58%, dot.nix 15% / 41%; this tracks which models were used.
- **Paying twice.** 113 bash-viewed files were later edited, and 105 of them still needed a `read` (≈103k bash-view tokens plus ≈55k read tokens). 45 of those reads came only after a failed edit. Nested `AGENTS.md` never attaches on bash views.
- **Cases where bash is genuinely better** (about 21% of pieces): counts, `-l`/`-o`, exclusion filters, `sort | uniq`, file metadata, `git show REV:path`, `jq`, one-level listings.

### Three approaches across harnesses

| Approach | Harnesses | Trade-off |
|---|---|---|
| **Shell-first:** few or no file tools | pi (grep/find/ls off by default; prompt routes search to bash), Codex (no read tool; "prefer rg"; "Do not use python scripts to attempt to output larger chunks of a file"), Octet (no search tool) | Matches training, but the harness cannot know what the agent saw: no read ledger, no structured results, no nested context |
| **Better tools plus steering** | OpenCode ("MUST avoid cat/grep/find/ls"), Gemini, Roo, SWE-agent, omp (blocking interceptor, **off by default**), axo today | Structured, but Claude ignores the steering when the tool is worse than bash. No surveyed harness enforces by default |
| **Accept harmless shell views** | Claude Code: single-file `cat`/`nl`/`bat`/`head`/`tail`/`sed -n`/`grep`/`egrep`/`fgrep`/`rg` with no pipes or redirects satisfy read-before-edit and load nested CLAUDE.md; if the command printed none of the file's text, the check still applies. On Linux, Glob/Grep are off by default and the shell has built-in bfs/ugrep | No fight with the model; the harness must classify commands and prove freshness |

### Tool features compared

| Harness | Line-numbered read | Many ranges/files per call | Search output modes | Listing |
|---|---|---|---|---|
| axo | no | no | lines only | no |
| Claude Code | yes (`N⇥`, or `:`) | no | files (default) / content / count; type, multiline, paging | no (shell `ls`) |
| omp | yes (`N:text` + file tag) | `f:5-16,960-973`; grep/glob take `a;b;!excl` | content, grouped by file, per-file caps | `read` on a directory |
| Octet | yes (`n: `), header with total lines and hash | no (parallel-call prompt) | none | no |
| OpenCode | yes (`00042\|`) | `batch` tool (experimental) | content | `list` |
| Roo Code | yes (`42 \|`) | 5 files × several ranges | content | `list_files` |
| Gemini CLI | no | `read_many_files` | content | `list_directory` |
| SWE-agent | yes (100-line window) | — | files with match counts | `find_file` |
| pi | no | no | content | `ls` (off by default) |
| Cline | unverified | no | content (`--context 1`), 300 results / 0.25 MB cap | `list_files`; `list_code_definition_names` outline |
| Cursor | unknown | List: full tree in one call | Instant Grep (local regex index) | List with file counts and types |

SWE-agent's ablations (SWE-bench Lite, GPT-4 Turbo, Table 3; default interface 18.0% resolved):
- Summarized search beat iterative search, 18.0% vs 12.0%; no search tool at all gave 15.7%.
- The numbered 100-line window beat whole-file viewing (12.7%) and a 30-line window (14.3%).
- Keeping the last 5 observations beat full history, 18.0% vs 15.0%.
- No linter on edits gave 15.0%; the default lint gate rejects syntactically broken edits.
- `search_dir` refuses with "More than N files matched… Please narrow your search" when more than 100 files match.

OpenAI's harness guide advises keeping a wrapper tool's name, arguments and output close to the underlying command, so models do not drift back to the shell.

### Cross-harness consensus

Seven harnesses (OpenCode, Gemini, Aider, SWE-agent, Cline, Roo, Cursor; Amp excluded):

| Design choice | Count | Who |
|---|---|---|
| Line-numbered read output | 3 of 4 with a read tool | OpenCode, SWE-agent, Roo (Gemini: no; Cline unknown; Aider has none) |
| Line numbers in search hits | 3 | OpenCode, Gemini, SWE-agent `search_file` |
| Search hits grouped under a file header | 2 | OpenCode, Gemini |
| Files-with-matches as default search output | 1 | SWE-agent (+6 points over iterative search) |
| Search result cap with a "narrow it" message | 3 | OpenCode 100, SWE-agent 100 files, Cline 300 |
| Results sorted by modification time | 2 | OpenCode grep, Gemini glob |
| Dedicated directory listing tool | 5 | OpenCode, Gemini, Cline, Roo, Cursor |
| Several files or ranges per call | 4 | Gemini, Roo, OpenCode `batch`, Cursor List |
| Batch tool or parallel-call guidance | 3 | OpenCode, Gemini, Cursor (Cline opposes it) |
| Explicit "don't cat/grep/find in shell" prompt | 1 | OpenCode |
| Enforcement or rewriting of shell commands | 0 | none |
| Large output spilled to a file with paging hints | 2 | OpenCode (all tools), Gemini (shell) |
| Older tool output pruned or collapsed | 3 | OpenCode prune, SWE-agent last 5, Cline duplicate-read notice |
| Code outline or map tools | 3 | Aider, Cline/Roo definitions, Roo outline on truncation |
| Lint or LSP feedback after edits | 2 | OpenCode `<file_diagnostics>`, SWE-agent lint gate (Claude Code also reports LSP errors after edits) |

### Hook and ledger facts

From the code, relevant to A1 #4 and #5:
- **Hooks can veto, not rewrite.** `ToolHooks` returns only `Allow` or `Block { reason, terminate, origin }` (`crates/axo-agent/src/hooks.rs:62-75`); there is no argument rewrite (also stated in `/repo/rust/Axome/docs/plan/plan.md:104`). `after_tool_call` can replace `content`, `details`, `is_error` and `terminate` (`hooks.rs:103-110`).
- Within a parallel group, every call's hooks finish before any call starts (`docs/extending.md:654,659`).
- An extension `toolPolicy` fails closed (a thrown error, unreachable extension or invalid reply blocks) and has a 60 s deadline (`remote.rs:13`).
- Axome registers no policy today.
- **No classifier exists.** `read_only_bash` is a sandbox flag that never inspects the command. The closest reusable code is the crate-private shell-word scanner in `bash_urls.rs:150-300` (`scan`, `Word{range,plain,redirect}`); it would need to report the first word of each pipeline segment.
- **Only `read` and write/edit record files.** `FileLedger::record` is not exposed to extensions ("unsafe outside the file lock", `ledger.rs:127-133`). Adding `details.files` to a bash result in `after_tool_call` counts only after a reload (via `restore`, `ledger.rs:188-201`), not live, so it must not be relied on. A sound version records under `lock_file`, or has the host re-read through `tools.readFile`; the race between the view and that re-read is why A1 #4 hashes before and after.

### Direction

Combine the second and third approaches. Fix the reasons first, accept the remainder, and do not block by default.
1. `read` prints line numbers and the total line count, and lists one level when given a directory. **Decided (2026-11-26):** each line is `N⇥text`, an unpadded number and a tab (Claude Code's format, close to `nl -ba`); `edit` strips that prefix if the model pastes it into `oldText`. The strip is the necessary companion to numbered reads and ships with them.
2. `read` accepts several ranges and several files per call. **Decided (2026-11-26):** ranges go in the path as selectors (`src/a.rs:5-16,960-973`, the form the model already writes for `sed -n` and `file:line`); several files go in a list of paths. This supersedes the ownership draft's sequencing (guidance first, measure, then multi-target read only if Claude still batches through bash).
3. `grep` gains files-with-matches and count modes, several paths with `!` exclusions, before/after context (including after-only: `-A` alone, 554 units, means "view from anchor") and paging, with output shaped like `rg`. Also measured: `-o` (107 units), `| cut -c` width control (115; `read` has no per-line cut), `-w`, multiline, type filter, smart case. Multiple `--include` (≈150 units) already works as one brace glob; say so in the guideline. **Decided (2026-11-26, W1):** several paths with `!` exclusions; `before`/`after` context, with `context` kept as the symmetric shorthand; output modes `content` (default), `files` and `count`; `-o`; matches grouped under a file header (rg heading style); overlapping context windows merged; `limit` kept with its "refine pattern" notice and **no paging parameter** (omp's page notice was followed by `skip` in 3–5% of cases). The grep guideline names the brace glob for several `--include`s. Not adopted: files-with-matches as the default (only 112 of 2,704 search units use `-l`), `-w`, `--type`, multiline and smart case (≤2 uses each), `| cut` width control (no surveyed harness offers it), an exclude-pattern parameter for `| grep -v` (255 units, a case where bash is genuinely better).
   - Per-file match cap: **Decided (W1).** 10 matches per file when searching several files, 50 for one file; the notice names the capped files and points to a `path:N-M` selector or a narrower pattern. One chatty file can eat the global 100-match budget (in the 70 limit hits the top file held p50 46% of matches), and context lines are re-read from disk. A 10-per-file cap drops up to ≈108k tokens (22% of grep result tokens); in omp it costs ≈2.7 points more follow-up greps. Grouping saves the repeated `path:N:` prefixes (15.5% of grep output) and merging saves ≈36.6k duplicated context lines. These pay on every `grep` call whether or not Claude migrates from bash.
4. **Decided (2026-11-26): edit freshness is proven by the match.** An `edit` whose every `oldText` matches the *current* file exactly once is applied even if the file was never read, or changed since it was read: if the old text is still there, the agent saw the part that matters. `write` over an existing file keeps the strict read-before-write ledger, because it replaces content the agent may never have seen. This removes the "not read / changed since read" failure class for `edit` (89 of 134 failures) without a bash classifier, and matches Claude Code's rule (A33). The "File mutation guards" invariant in [`AGENTS.md`](../../AGENTS.md) and the tool docs change with the implementation.
   - Nested `AGENTS.md` on bash views: **Decided (W1): attach only.** For every view unit's file path, including inside compounds, run the same nested-context attach `read` runs, found with the public classifier from #5. No ledger record and no hash: the hash existed only to make a bash view count as a read, which #4 made unnecessary for `edit`, and `write` keeps its strict ledger. Without it X4 would miss ≈40% of its target sessions (15 of 38 Axome sessions bash-viewed `src/` and never read, edited or wrote there). Ship it before or with X4.
   - Evaluation note: #4 turns a not-read failure into a success when the text matches; when the text is wrong at the edit site (for example after a stale bash view), some "changed since read" failures become "could not find" failures instead [INFERENCE]. See A19.
5. A blocking policy exists as a setting, off by default (see [Defaults](#defaults-that-changed-between-drafts)). It never blocks compounds, pipes into filters, or the cases where bash is better. The shell classifier is exposed as a public helper so hosts can write their own policy. **Decided (W1):** axo exports one public classifier that splits a command into pipelines and reports per unit its kind (view with path and ranges, search, list or other), its file paths, its trailing filters and whether it writes or redirects. It drives both the off-by-default block setting and the nested attach in #4. The policy blocks only a whole command that is **one** tool-equivalent unit with no redirect and no filter other than `head`/`tail`, and its reason names the replacement call (for example ``read `src/a.rs:5-16` ``). After A1 it would reach 286 calls (9.5% of bash calls, 464k tokens). omp's regex rules replayed on axo match 813 calls, 71% of them compounds or mixed. In omp, blocking moved Claude's search share (21.5% in September to 1.3% in November) but not its view share, which was ≈8% before and after the interceptor; the low view share comes from omp's read surface and prompts.
6. The bash guideline states the real contract: use the file tools for files; use bash for pipelines, counts, git and jq. Command-scoping advice: prefer `--quiet`, `--stat`, `-q` or filters; **never** advise piping into `tail`/`head`, because the artifact is created only when the shaper itself truncates (`jobs/command.rs:767-775`), so piping destroys the full log. Related: omp says "Don't trunc output by habit"; Gemini's optional "shell output efficiency" block (quiet flags; redirect to a temp file, then grep). `git diff` alone is 109 calls / 239k tokens. **Decided (W1), narrower:** two bash lines, "Use bash for pipelines, counts, git, jq, builds and tests; use the file tools for viewing and searching files." and "Scope output with the command's own flags (`--quiet`, `--stat`, `-q`, `rg -l`/`-c`). Don't pipe build or test output into `head`/`tail`: the result keeps its head and tail and saves the full log." `| head` on searches stays unmentioned: it works as a `limit`, and 49% of axo bash calls end in `head`/`tail`. The read guideline replaces "edit and write refuse files this session hasn't read" (`read.rs:19-21`, false for `edit` after #4) with the #4 contract and names the new read features. No `git diff --stat` advice: only 41 of 285 diff segments are bare (15% of diff tokens). The artifact mechanism justifies the build-pipe line; the behavioural evidence for it is weak (6–10 re-run events).
7. Targeted-read guidance in the read guideline: locate with grep/find before reading **large** files; don't re-read content still visible. Do not push ranges onto small files, which fragments reads into extra turns (omp: "Read small files whole; ranges only for large files"). Owner: axo guideline per the ownership draft; the skeptic suggested host. **Decided (W1), low confidence:** one ≈20-token line in axo's read guideline, "Read small files whole; for large files, find the spot with grep and read only the ranges you need.", without the re-read clause (axo's overlapping re-reads are 0.65% of read tokens; omp has the clause and still 6%). Whole reads of files over 500 lines are 121 reads, 852k tokens (21% of read tokens). The line costs ≈12k input-eq per two weeks and breaks even at ≈2 converted reads; for Claude the benefit is probably small. The reliable mechanism for large reads is A6. Owner: axo, by the ownership principle. Recheck with `W1Tools/followups.ts --corpus axo` two weeks after shipping.

### Line numbers: cost and A/B (resolved)

- **Cost of line numbers.** They add 2–4 tokens per line, a 10–20% increase on code for every read. Design history: hashline is a listed non-goal (`plan/design.md:44`); the earlier hashline plan is at `plan/pi/context.md:424-433` and `plan/research/pi-edit.md`; `plan/research/edit-formats.md:24` says Claude is trained on line-numbered `view`, and `:59` quotes Aider: "GPT is terrible at line numbers". **Measured (W1)** on all 2,116 axo file reads: `N⇥text` adds 11.6% in o200k tokens (1.35 per line; `N:` 17.2%, padded `  N⇥` 14.3%; code files 13.4%, Markdown 7.1%). A fit on real Claude usage (2,277 single-read steps) puts omp's `N:` at +15.2% for Claude, so `N⇥` is ≈10–14% [INFERENCE: the tab/colon ratio comes from the proxies]. Most of the bash volume numbering replaces moves into `read` rather than disappearing, so the net is a likely **cost** of ≈0.2–0.5M result tokens per two weeks. **Decided:** ship `N⇥` anyway; the justification is the contract (`path:line` citations without `nl`, `edit`'s prefix strip, selectors that address what the model saw, the total-line header), not tokens. `N⇥` is the cheapest of the three formats.
- **A/B test before committing.** **Rejected (W1)** as underpowered: two weeks gave 35 Claude root sessions, view share is dominated by model and task, and an A/B cannot separate numbers from selectors, which are also decided. Instead, two weeks after shipping, rerun `OmpMiner/stats.ts --corpus axo` and compare Claude and GPT view share, V2 (`nl`/`cat -n`) units, edit failure classes, confirm-reads and read tokens per call against the Phase 0 baseline (`OmpMiner/axo.md`).
- Alternatives to #4 were listed as A31–A33: A31 and A32 are Rejected, A33 is adopted as #4.

## Inventory by owner

Status uses the labels defined at the top. Evidence anchors are into axo unless noted. Notes that change how an item would be built follow each table.

### axo

| # | Item | Evidence | Status |
|---|---|---|---|
| A1 | Bash/tools direction above | see section | Decided: every sub-point is resolved in [Direction](#direction): #3 grep set with per-file cap, #4 attach-only nested context on bash views, #5 single-unit block setting (off) on a public classifier, #6 and #7 guideline lines, and numbered reads with no A/B |
| A2 | Automatic cache TTL: 1h when a request ends the turn, in `wait`, or in `ask`; 5m otherwise. Setting `off/auto/always`. | `axo-ai/src/api/anthropic.rs:615` hard-codes `{"type":"ephemeral"}`; usage/cost already parse 1h writes (`:358,395`, `cost.rs:31`). 1h writes cost 2×; 1h breakpoints must precede 5m ones ([docs](https://docs.claude.com/en/docs/build-with-claude/prompt-caching)). [INFERENCE] net ≈5–6M input-eq saved, ≈7–8% of measured Anthropic spend. A system-prefix-only TTL recovers only ≈0.5M. Superseded by W2's replay simulator under the exact billing rules: per-request 1h loses even with a perfect predictor (+1.4% Anthropic IE; the outcome predictor +4.2%), because a 1h block after a 5m run rewrites the prefix at 2×. | Decided for priced routes: gated warming, `cacheWarming: off \| streaming \| idle`, default `idle`; the gate uses the catalog row's multipliers and needs ≥12.5k input-equivalent tokens of expected saving; never on streaming, budget thinking, forced `tool_choice`, structured output, a finished child or a changed context. −11.9% Anthropic IE at API prices. Setting `cacheTtl: 5m \| 1h` per bucket (root, child). OpenAI: do nothing. Rejected: per-request predicted 1h, pins, 1h on tools+system only, ungated long caps. Decided (Toph, 2026-11-27): on subscriptions, use 1h for root conversations and gated in-run warming for children ([Q1](#decisions-by-toph)). |
| A3 | Parallel batching guidance in SYSTEM.md; multi-target `read` (A1, Decided); possibly codemode (Track 1) as the batching tool | 79% single-call messages; each extra round-trip re-reads ≈86k cached tokens. Codex: "plan all reads → one parallel batch". pi codemode and Octet composition pass only script output to the model. OpenCode `batch`. W4: axo already chains fewer lookups than omp on same-model, same-role rows (opus `task` 20.8 vs 25.5 chains per 100 requests; gpt-6.1-sol `task` 10.6 vs 17.1); axo Claude batches inside compound bash (2.50 lookups per lookup message vs omp 1.66). | Decided: do nothing now; re-measure after A1 against the saved baseline (`W4Batching/trigger.ts --compare baseline-preA1.json`); run an A/B only if a cell triggers. omp's batching line came with eight other prompt changes, and its Claude arms looked net negative. Rejected: SYSTEM.md line, GPT-only line, tool-description line, per-turn nudge, `batch` tool, codemode as the batching vehicle |
| A4 | Clearing also covers aged write/edit arguments, images inside tool results, and thinking from completed turns | Clearing touches only `ToolResult` text (`compaction/elide.rs:130`). After the 5 passes, these remained: 5–36k of args, 10–31k of thinking, up to 45 images (≈63k). `ContextReplacement` can rewrite assistant blocks (`store/project.rs:391-415`). On Opus 4.5+/Sonnet 4.6+, earlier thinking is kept and billed ([docs](https://platform.claude.com/docs/en/build-with-claude/thinking#thinking-block-preservation-by-model)). Only a pure prefix before the last real user message may be stripped. Images: skip, do not error, on corrupt base64 (`elide.rs:230-232`). W3: on Opus 5.5 and Fable 5.2 axo sends `drop_block`, so each pass already drops every later thinking block (286k over 5 passes, confirmed by the observed prompt drops). | Decided: inside the existing Clearing pass, also stub aged `edit`/`write` arguments (keep `path`; `[shaken ~N tokens; artifact://n]` for large values) and replace aged images with `[shaken image]`, skipping undecodable data; on Gemini only before the last real user message. −0.89% to −0.93% Anthropic IE at no extra cache cost; OpenAI likewise only inside the pass. Rejected: thinking clearing (saves 0.0%), per-request stubs. Decided (Toph, 2026-11-27): keep client-side clearing on every model, including those that preserve thinking ([Q2](#decisions-by-toph)). |
| A5 | Cache-aware pruning between triggers: supersede stale reads, drop used empty results | omp `compaction.supersedeReads` / `dropUseless` run only when the cache is cold or little has been sent since. OpenCode prunes every turn (protect 40k, prune if ≥20k). Cline dedupes re-reads. SWE-agent last-5 observations +3 points. This upgrades the earlier dismissal of superseding. Clearing today keeps no read ranges in `details` (`tools/ledger.rs:60-70`). W3: supersede/useless removes only ≈2k tokens in the whole corpus. | Decided: cold-clear. Before a request axo knows will miss (last cache touch older than the TTL it requested, or warming gave up), run the Clearing pass when the prompt is ≥150k and the pass removes ≥20k. −0.35% to −0.44% with warming on, −7.6% without; never worse than doing nothing. Rejected: supersede/useless (+0.16%), every-request pruning, idle clearing while warm, scheduled clearing |
| A6 | Outline for unranged reads of large code files | omp returns declarations only from 100 lines, with folds re-readable by name. Roo adds a tree-sitter outline on truncation. Aider's PageRank repo map. axo keeps C grammars out of the default build (`plan.md` Track 5). W6: after an omp outline a ranged follow-up comes 74–78% of the time; axo's complete whole reads are followed by a ranged read 0.8% of the time. | Decided: a declaration-aware indentation outline with no parser, for unranged reads of Rust, TS/TSX, JS or Svelte files ≥24 KB (≈6k tokens): `N⇥` lines, `… a-b` folds, a footer naming the multi-range selector; one `read` setting holds the threshold (0 turns it off). Declaration recall vs tree-sitter 99.7–100% on three out-of-sample corpora at the same ratio; every band ≥6k tokens saves in all four scenarios (−2.2M to −5.2M cache reads per two weeks). Rejected: omp's ≥100-line default (1k–4k bands lose), tree-sitter (+4.54 MB, C toolchain), wasm grammars, LSP, outline on cap hit only |
| A7 | Cap tool-call arguments in summary rendering | `compaction/summary.rs:238-251` writes arguments uncapped in `ToolDetail::Capped`. Separate requests serve every split-turn prefix and every branch summary, plus overflow (`maintenance.rs:349-353`) and retries (`:395,436`); `prepare_branch` keeps calls and drops results. W3: all 11 compactions cut mid-turn and went through `separate()`; arguments were 5–90% (median 22%) of the split-turn renders. | Decided: apply `tool_text` (2000 UTF-16 units) to each call's rendered `name(args)` in `ToolDetail::Capped`. Rejected: dropping arguments (loses paths and commands), per-key caps |
| A8 | `compaction.model` / `compaction.thinking` for Separate requests only | Live selection is used (`session/maintenance.rs:236-244`). Live must stay matched for the cache. Octet has `compact_model`. W3: Anthropic compactions are 1.2% of Anthropic IE; the 64k Qwen case that motivated it is free and fixed by A9. Separate requests get `cache_control` and a fresh session id, so their whole input is written at 1.25× and never read (the Fable 181k compaction wrote 40,399, read 0). | Rejected: unset by default it saves nothing, and a cheaper default puts <1% of Anthropic IE at risk on the most loss-prone path. Decided instead: send no `cache_control` on one-shot Separate summary requests |
| A9 | Derive clearing protect window and headroom from the trigger; add a size floor | Literal 40k (`elide.rs:92`) and 20k (`compaction/mod.rs:18`). A 32k window can never clear. No second pass was observed after the first. Clearing has no minimum (`elide.rs:196-205`). | Decided: window min(40k, T/4) and headroom min(20k, T/10), unchanged at the default trigger; one 400-token floor for every elision mode (drop the Clearing exemption; +0.20% IE, 43% fewer artifact files); keep clearing all eligible results. On the 64k Qwen session 2 of 3 compactions become clearings. Rejected: T/8, floors of 50–200, "just enough" clearing |
| A10 | Re-evaluate default `maxContext` (400k) and the clearing point | Clearing fired at ≈360k. Prompts p90 210k. A clearing costs one suffix rewrite and saves 0.1× on every later request. W3: the three ≈50k threshold compactions are a 64k Qwen window (trigger 49,152) where clearing could never qualify (A9). Only 7 of 205 sessions ever exceed 250k. | Decided (Toph, 2026-11-27): keep `maxContext` at 400k for everyone and set a separate clearing threshold at 75% of the window, without changing the summary trigger ([Q3](#decisions-by-toph)). The original 350k / 300k / 250k cap options save 1.1–2.4% / 2.4–5.1% / 1.2–7.4% of Anthropic IE; 300k was recommended before the separate-threshold comparison. Rejected: no cap (+2.5% to +4.6%), 200k, 150k |
| A11 | Image size cap and token estimate per model | On Claude 4.7+ (high-res tier, 2576 px / 4784 tokens) a 2000 px image is not downscaled: 2000×1500 costs 3,888 tokens vs ≈1,564 at 1568 px ([docs](https://docs.claude.com/en/docs/build-with-claude/vision)). The estimator is flat, ≈1.2–1.4k per image. 74 browser screenshots observed. W2 decoded all 100 images: the longest edge was 1,721 px. | Rejected: the 2000 px cap never bound; a 1568 px cap saves 1.2k tokens in total; the flat estimate is 19% high and the next usage report corrects it |
| A12 | Edit guideline: a successful edit keeps the file current; no re-read | 29 confirm-reads + 28 bash views right after edits. omp: "NEVER confirm an edit by re-reading". W1 recount: 27 confirm-reads (41k), of which 9 (5.9k) were not followed by another edit. | Rejected: the line costs ≈9k input-eq per two weeks, about the most it could save; omp's higher confirm rate cannot be credited to its line (hashline anchors); an `edit` diff return pays on every edit |
| A13 | Agent `hidden` frontmatter flag: omitted from the spawn listing, still spawnable by name | Spawn-by-name resolves the full catalog (`session/spawn.rs:340-350`); the listing skips only `runAs: root` (`:902-920`). 15 workflow agents ≈529 tokens per root request; ≈1.2M cache reads over 2,308 requests. | Decided: `hidden: bool` in `AgentSpec` and frontmatter; omitted from the spawn listing, still spawnable by name and usable as a team member. 0 of 633 omp workflow spawns came from the listing. Rejected: reusing `disable-model-invocation` (it blocks model invocation), names-only listing, a deferred catalog, mode-scoped agents, a host filter hook |
| A14 | Report-size clause in DELEGATION.md | No size guidance; child reports ≈2.5% of volume; decomplect-scout p50 4,863 | Rejected: re-reads after reports cost 3.8× the reports and are highest after the smallest reports (64%); omp's "concise" directive coexists with reports twice axo's size; the cap never binds (max ≈21 KB). Plan-mode re-reads are X3's |
| A15 | Promoted `tool://` read omits schema and example (keeps guidelines); tool-less sessions omit docs/schemes sections | `docs/prompts-and-context.md:61,65`; memory-writer has `tools: []` | Decided: `tool://<name>` returns the schema as compact JSON (−38% to −48% of schema tokens on every provider; `schemes/tool.rs:60`); omit `docs` and `schemes` when the prompt shape has no tools. Rejected: the promotion-aware read (the read cannot know whether promotion will happen; ≈0.8M resident, Anthropic only) |
| A16 | Tighter bash inline output | axo inlines up to 50 KB. Codex defaults to 10k tokens with `original_token_count`; omp 20+20 KB, 500 lines. Only 13 truncations observed. | Rejected: keep 50 KB / 2000 lines and add no setting. Lower caps save ≤1.4% of cache reads gross and cut whole-file views and diffs the model then pages back. Revisit when a supported ≤128k-window model produces capped results, or a host asks |
| A17 | Repeated identical read hint | omp appends "will not change" on the third identical read; 41 identical re-reads observed. W1: 33 identical-output re-reads, 29 of them `proc://` polls and 1 a file. | Rejected: one third-or-later file repeat in two weeks; in omp 10 of 41 hints were followed by the same whole re-read |
| A18 | OpenAI `reasoning.context: current_turn` | `openai_responses.rs` never sends it. Live: with the field omitted the effective default is `all_turns` on gpt-6.2-sol, gpt-6.1-sol and gpt-5.6-luna. `current_turn` bills like stripping earlier reasoning and breaks the cache at every new user turn: ≈1.29M input-eq of rewrites against ≈0.13M of rendered reasoning saved (replay). | Rejected: keep `auto`. `current_turn` is a cost (≈10×), not a saving, and drops reasoning continuity |
| A19 | Low value, if convenient: read dedup stub, edit near-match hint, lossless repeat-line collapse, "grep it" in truncation notices, cwd-relative capped compaction file lists | Measured small: ≈26.5k overlapping re-read tokens; 18 "could not find"; collapse potential 0.9% of bash; file lists 20–536 tokens | Rejected: dedup stub (0.65% of read tokens); near-match excerpt (12 of 17 recoveries edited different text, 9 of 18 failures are single lines; revisit if "could not find" exceeds ≈5% of edits after A1, starting with a whitespace-only hint); collapse (0.01% of bash tokens, 296 tokens). "grep it" is already in the schemes section; file lists are A25 |
| A20 | Edit guideline: keep `edits[].oldText` as small as possible; do not pad with large unchanged regions (Octet wording) | Edit arguments are 412k tokens (31.4% of argument tokens, 19.15M resident); 85 write/edit calls over 2k argument tokens hold 342k. Nothing targets edit-argument size at ingestion. W1: padding is 9.0k tokens; the 342k is new content, not padding. | Rejected: the `edit` description already says it (`edit.rs:82`), and the target waste is 2.8% of edit-entry tokens |
| A21 | Promotion snapshot duplication: on Timer (default 60 s), Host or Steer promotion the model gets `render(None)`, and the final `job_result` re-renders the same shaper, so the head (≤20 KB) appears twice. Fix: a short tail snapshot at promotion; keep the final result complete. | 213 snapshots total 28k tokens (p50 61, max 4,080). W5: 16k of that is snapshots and 12k notices; duplicated lines 15.3k, 0.94M resident; 173 snapshots are empty. | Rejected: a tail snapshot saves ≈56k input-eq per two weeks and hides the head that informed 2 of 6 kills; a delta final breaks the complete result. Revisit if promotions get large heads |
| A22 | `find` default limit 1000 → ≈200 | Worst case 10–15k tokens; 16 of 123 calls hit the limit (27k). The description string and the schema default must change together. W1: only 2 of the 16 hits were at the default. | Rejected: a 200 default saves ≤18.8k tokens, and omp's 200 default hits the limit on 12% of calls |
| A23 | Validation failures echo `Received arguments:` on every failed attempt | `run.rs:~655`. W1: `run.rs:1128-1133` echoes the parsed (repaired) arguments; `axo-ai/src/json.rs:86` echoes the raw text when parsing failed. | Rejected: 13 failures, 4.6k echo tokens, 0 consecutive repeats; each echo is the model's only view of what the validator saw |
| A24 | Child-report cap reuses `Limits::default()`, so changing read or bash defaults silently changes child capping | `child.rs:55`; maintainability only | Rejected: one inline budget is the documented contract; 0 of 366 child reports were capped |
| A25 | `previous_summary` re-feeds the deterministic file lists to Separate summarizers | `summary.rs:304-310`, `mod.rs:186-192`. Fix: keep the raw summary text separate; stripping by parsing is fragile. | Rejected: lists were 11–521 tokens, only 3 update compactions re-fed them, and the model did not duplicate them |
| A26 | Second rolling cache breakpoint (B3bp) | axo uses 3 of 4 breakpoints. Lookback is 20 positions and a run of `tool_use` or `tool_result` blocks counts as one, so a miss needs ≈10+ rounds in one request. Counter-evidence: Octet's `CACHE_AUDIT.md` found its conversation breakpoint unreachable after wide parallel batches past the 20-block window; reuse went from 0.43% to 98.9% at 32 calls per turn (fresh input ≈1.25M → ≈14K per 8 turns, simulated). The two conflicted. Resolved by W2: Octet's simulator counts every block separately, while under the documented run-collapse rule the same batch adds 2 positions; axo's 3,143 Anthropic requests added at most 8. | Rejected: 0 lookback misses. Re-open if axo serves Claude through Bedrock, Vertex or Foundry, or one request appends ≥20 positions |
| A27 | OpenAI caching knobs | Pre-GPT-5.6: `prompt_cache_retention` (`in_memory`/`24h`; 24h is already the default for non-ZDR orgs, so no action). GPT-5.6+: `prompt_cache_options.ttl` (`"30m"`/`"12h"`), explicit breakpoints, `mode:"explicit"`, reads at 0.1× (0.05× on some 6.x). axo sends none of these (`openai_responses.rs:134-150`). GPT-6.2 bills top-level `instructions` uncached on every request (32% of 6.2 input cost in the journals; live 1,313 uncached per call), while a leading `developer` message is cached. | Decided: on every OpenAI Responses row (API and Codex) send the initial system prompt as a leading `developer` item instead of `instructions`, and set `supports_mid_convo_system_messages` so later prompt diffs append in place (live: a mid-conversation developer item kept 4,676 of 4,679 cached and dropped nothing). `supports_additional_tools` only after per-endpoint confirmation. Rejected: `24h` retention, `12h` TTL, explicit breakpoints, a head rewrite, a section split, dropping `prompt_cache_key` |
| A28 | Cache misses from thinking / mode / agent changes | 5 misses, 976k cache writes in two weeks. Not proven to be system-prompt collapses; cause and owner unknown. W2: their gaps were 5.3–22.1 min, and 9 of 9 such events followed within 5 min hit the cache. | Decided: closed as TTL misses; the A2 policy recovers them |
| A29 | Re-hydration after compaction (Claude Code): re-read up to 5 recently modified or read files (over 5k tokens → path only) and re-inject skill bodies (5k each, 25k total) | axo re-reads nothing after compaction; the summary lists files only | Rejected: in omp only 13–15% of the files the rule would load were re-read within 20 requests; axo keeps a 20k raw tail, which Claude Code lacks. Skill-only re-injection is a continuity question, not decided here |
| A30 | Split area-specific sections of axo's own `AGENTS.md` into nested files, as X4 does for Axome | Project AGENTS.md is ≈55% of the always-on root prompt (axo ≈2.9k). Depends on A1 for nested attach on bash views. W6: the current file is 2,094 tokens; the area rules are ≈460 (22%). | Rejected: saves ≈0.22M cache reads per two weeks; 3 of 7 roots touched `crates/axo` only through bash, and the extension-surface invariant is not a directory slice |
| A31 | Session tools installed as bash commands (omp `<tool> args`, `--help`), so compound bash calls route through tools and the ledger | omp keeps `grep` top-level because it is the interceptor's redirect target | Rejected: multi-path `read`/`grep` (A1) absorbs the compounds; bash cannot record into the ledger without the file lock; schema deferral belongs to A15/X1 |
| A32 | Optional `expected_hash` on `edit`/`write` (Octet): `read`'s header carries a whole-file sha256; a stale hash rejects the write. | Octet `read.rs` header `path:offset-end/total hash=…` | Superseded for `edit` by A1 #4 (Decided). Rejected as a `write` refinement: the 9 refusals in two weeks are the ledger working, and a header hash costs ≈40 tokens per read |
| A33 | Claude Code edit freshness rules: an edit to a file changed on disk is still allowed if `old_string` matches the current content exactly once; newer models may edit an unread file when reading it would not need a permission prompt | 51 of 89 stale failures came after a bash-only view | Adopted as A1 #4 (Decided) |
| A34 | "Did you mean" suggestions (up to 3) for a missing file (OpenCode); default ignore list plus `ignore[]` on listings | 55 read errors | Rejected: suggestions could save ≤5 search round-trips per two weeks; the directory `read` covers recovery; the walker already respects `.gitignore` |
| A35 | Project context files for subagents (added by W6) | `SubagentSettings::default` sets `context_files: Off` (`config.rs:193`), so children get neither startup files nor nested attach. 55 successful `.svelte` edit/write calls (173 edits) in 15 `task` child sessions ran without Axome's Svelte rules. Post-X4 cost per two weeks: nested-only ≈140k input-eq (lower bound), full context ≈677k. | Decided (Toph, 2026-11-27): make nested-only mode (`contextFiles: "nested"`) the child default, with no root file and area files attached on first touch ([Q4](#decisions-by-toph)). |

#### axo notes

- **A2.**
  - Ordering rules from the provider docs that decide feasibility: reading a 5-minute entry only restarts 5 minutes, even when the request asks for `ttl:"1h"`; the TTL clock starts at request start, so a slow stream eats the window; billing splits at A (last hit, read), B (last 1h breakpoint, span B−A billed as a 1h write) and C (last breakpoint, span C−B a 5m write). The miner's estimate ignored these.
  - Economics [INFERENCE]: requests ending in `wait`/`ask`: the 27 misses become reads, ≈4.3M saved vs ≈0.9M extra (0.75 × 1.21M written by those 202 requests). Requests ending the turn: ≈3.7M vs ≈1.5M. 2 misses (0.28M) came from "other long tool".
  - Implementation: `SimpleStreamOptions` has no cache field (`api/mod.rs:75-87`). A host could already inject a TTL with the `RequestRewrite` hook. `AnthropicCompat` derives `cache_control_on_tools` from model compat.
  - Warming: pi's `cacheWarming:"idle"` warms "when a refresh is expected to pay for itself". Octet warming defaults to `streaming`; `idle` also covers waiting for the user; savings and miss diagnostics show in `/cache`; `cache_retention` defaults to "short".
  - pi's Anthropic "message threads" (send only new messages) and section-patched system prompt are in the catalogue; the second bears on system-prompt collapse.
- **A3.**
  - Calls per message: 0: 494, 1: 4,540, 2: 575, 3: 202, 4: 160, 5: 97, 6: 59, 7: 29, 8: 30, 9: 6, 10: 9, 11: 1.
  - Composition tools: Octet 256 nested calls, 30 s, 1 MiB store, `Only` mode exposes tools only inside the script. pi codemode: 10k output tokens with head/tail spill; BM25 `searchTools`/`describeTool`. OpenCode `batch`: 1–25 calls, no nesting, no MCP, experimental flag; its "2–5×" claim is unverified.
  - Codex guide: order history as all function_calls first, then outputs; its prompt names `multi_tool_use.parallel`. Cline says "one tool per message" (the opposite). Cursor's Explore subagent runs parallel searches and returns summaries.
- **A4.**
  - Server-side alternatives: Anthropic `context_management` thinking clearing (`clear_thinking_20251015`) and tool-result clearing (`clear_tool_uses_20250919`: `trigger`, `keep`, `clear_at_least`, `exclude_tools`). Exposing these may fit better than mutating the transcript.
  - Thinking: `/shake thinking` has no age protection; it is safe today only because shake is standalone and returns Busy during a run. Stripping on every request would rewrite the previous turn's cache entry. Anthropic recommends passing thinking back for continuity. Last-turn-only models (up to Sonnet/Haiku 4.5) save nothing and still break the cache.
  - Write/edit stubs: a call is aged only when its result is outside `age_protected`, and clearing never touches the current tool loop. Google `thought_signature` is another unknown. The model may imitate stubbed arguments. No internal consumer reads old arguments (`child.rs:150-185` reads the journal).
  - On newer OpenAI models, editing history before a reasoning item can make the API drop that item; any OpenAI clearing must account for it.
  - Data: Claude visible thinking 355k tokens, replay payload 1.37M; completed-turn thinking resident 7.9M (Claude).
- **A5.**
  - Cache economics of early clearing: break-even ≈ (C−D)×1.15 / (R×0.1) turns, ≈46 turns for C=150k, D=30k, R=30k. Opportunistic rule: clear early when the cache is already cold (`now − last_assistant > TTL`). axo has no TTL awareness today. This is also the basis for rejecting scheduled clearing.
  - omp specifics: `supersedeReads` lets a whole-file read remove earlier range reads only if that read was complete (`tools/read-supersede.ts`); `dropUseless` removes "no matches"-type results once used.
  - Counterpoint: Zechner's "Building pi in a World of Slop" talk argues against hidden context mutation such as pruned tool outputs (from the talk page summary, unverified in detail).
- **A7.** Codex compacts at the end of a turn by percentage and has server `/responses/compact`; Octet compacts at completed-turn boundaries (catalogue).
- **A8.** Anthropic invalidates the cache on a thinking change. Forcing a cheaper model on Separate degrades the loss-prone path. `modelRoles` exists but compaction doesn't use it.
- **A9.**
  - Mechanism: clearing elides everything eligible, so the first pass overshoots; slack then decays as s′ ≈ p·s − (1−p)(H+o). A lower headroom slows the decay but does not remove it. On a 32k model the target is 24k − 20k = 4k. The 40k protect window counts tool-result tokens only, so the real protected context is larger.
  - Size floor: the 400-token minimum is skipped in Clearing, so each tiny result gets its own artifact file. Fix: a modest floor, or keep the first line. A missing tool name or status in the stub doesn't matter; adjacency carries them.
- **A10.**
  - Trigger math: trigger = W − reserve, with W = min(window, maxContext = 400k) and reserve = min(reply + 4096, W/4), reply ≤ 32,768. 400k gives ≈363k (91%); 200k gives ≈163k (82%); the floor is 75%.
  - Data: manual compactions at 59k–329k; summaries 0.7–3.3k tokens. After clearing the next prompt was 136–196k; each pass removed 65–140 results (69k–128k tokens); later peaks reached 194k–327k with no second pass. **3 threshold compactions at ≈50k in one axophyte project**: some model or setting triggers that low, unexplained.
- **A11.** The vision formula is now ⌈w/28⌉×⌈h/28⌉; the old w·h/750 and ~1.15 MP figures are gone from the docs. Images over the limit returned to provider computer-use/browser-use tools are **rejected**, not downscaled. OpenAI high-detail fits 2048 then 768. axo already appends a resize hint (`files.rs:~272`).
- **A12.** Alternative: Octet's `edit` returns a unified diff (3 lines of context, capped at 16 KiB), which replaces confirm-reads at a token cost per edit.
- **A13.** One mis-spawn in 31 workflow-agent spawns; hiding does not block spawn-by-name.
- **A15.** Schema budgeting elsewhere: Octet `set_tool_schema_budget_bytes` refuses an over-budget tool set before any call; Codex compacts MCP input schemas above 5,000 bytes; pi `prepareLoadout` hides declarations. Open, low.
- **A16.**
  - Configurable budget shape: TAIL is derived from the total, so shrinking HEAD alone saves zero tokens. The fix needs an embedder-settable total plus head share, defaulting to today's pi-parity values. Config today exposes only `promote_after_ms`.
  - Artifact follow-up: only 7 of 127 cut notices were followed by a reference to that artifact (read whole ×2, ranged ×3, send ×2), none by grep or tail. All artifact references: 15 whole reads, 3 ranged, 5 bash filters, 2 sends; 18 reads returned 41k.
  - Elsewhere: Claude Code ~30k chars inline, else a file plus a 2k-char preview; failed commands get a 10k-char head+tail and no file; `BASH_MAX_OUTPUT_LENGTH`. Codex collects up to 1 MiB, `tool_output_token_limit` caps what is stored in history, and its guide recommends 10k tokens split half head, half tail. Octet marks each stream `complete_stdout=true` or `truncated_stdout=head:N tail:M omitted_bytes:K` and evicts spills. pi returns a structured `full_output_path`, and its notices hand over the exact next call. Gemini keeps 1000 lines (first 1/5, last 4/5) and wraps lines at 120 chars. OpenCode's truncation hint delegates to an explore agent ("Do NOT read the full file yourself"). Open, low.
- **A17.** omp's exact text: "[You have received this identical output N times. Re-reading '…' will not change it — use a narrower selector (path:A-B), or proceed with the edit.]"
- **A18.** With `current_turn`, resent reasoning items from older turns stay in the payload but are not rendered, shrinking effective context. On newer models, editing history before a reasoning item can make the API drop it.
- **A19.**
  - Read dedup fix shape: a read index (call id, path, range, hash) beside the ledger, rebuilt via `restore`/`after_append`, skipping shaken results (detectable by their marker). A ledger record is **not** a visibility proxy, because shaken results keep their records. Re-reads after clearing or compaction are correctly not deduplicated.
  - Repeat-line collapse must be lossless (identical consecutive lines only); "differs only by digits" hides diagnostics.
  - "grep it": the schemes section already says artifacts can be grepped (`context/prompt.rs:126-133`); the data above shows artifacts are rarely followed up at all.

### Axome

| # | Item | Evidence | Status |
|---|---|---|---|
| X1 | Defer `search_docs`, possibly `web_fetch`; keep `web_search` and `memory_search` direct | `services/axo/web/mod.rs:96-108`; direct tools ≈1.2k cached tokens per turn; `memory_search`'s guideline drives recall. W7 measured per request (schema + guideline): `web_search` 234, `web_fetch` 127, `search_docs` 309, `memory_search` 301; `search_docs` was called twice in two weeks. | Decided: defer `search_docs` only (≈1.64M cache reads saved against ≈18k input-eq of round trips). Rejected: deferring `web_fetch` (22 calling sessions ≈191k input-eq vs ≈68k saved), deferring all four (even on tokens, and loses the recall and fact-check guidelines), off by default, axo auto-defer |
| X2 | `memory-writer.md`: `sections {work, prose, docs, schemes: false}`, `contextFiles: false` | Up to 20 fresh sessions per capture (`services/memory/writer.rs:300-319`) each carry WORK (≈1.3k) and PROSE. W7: WORK+PROSE are 36% of each writer's first request; 18 captures and ≤103 requests in ≈31 h. | Decided: `sections: {work: false, prose: false}` and `contextFiles: false`; `docs`/`schemes` are omitted by axo's A15 instead |
| X3 | Bundled plan modes: scout-cited `path:line` evidence counts as read | `modes/plan.md`, `plan/step.md`, `plan/roadmap.md` require everything "read in this session". W7: scouted paths are re-read 58% of the time in plan modes vs 29% with no mode (28% in other modes, a bucket that covers any non-plan mode). | Decided: at `plan.md:33`, `plan/step.md:34` and `plan/roadmap.md:33`, evidence is your own read or a scout's cited `path:line` excerpt; re-read only for verbatim quotes the scout did not quote, or doubtful claims |
| X4 | Move Svelte/SvelteKit/Tailwind rules to `src/AGENTS.md` | AGENTS.md lines 57–95 ≈1.3k tokens in every root session (W7 measured 921). Nested attach fires only on read/write/edit, so this depends on A1. W7: 938 of 1,772 Axome root requests came before the first attach or never attached. | Decided: move lines 57–95 now with a one-line root pointer (≈25 tokens); it no longer waits for A1. Saves ≈0.86M cache reads per two weeks |
| X5 | Surface new axo settings (TTL, steering, compaction model) | W7: the typed mirror rejects unknown keys, and `settings_mirror_defaults_match_axo` passes with a missing field. | Decided: each axo item that adds a setting mirrors it in `AxoSettings` and the README as part of done; add a key-completeness check to that test; UI rows only beside existing siblings. Fix the stale "no `Serialize`" clause in Axome's `docs/plan/design.md` P2 |
| X6 | `web_fetch` answers a question about the page with a small model instead of returning raw text (Claude Code WebFetch) | Today's 14k-char preview ≈3.5k tokens (see Rejected). W7: `extract_parallel` always sends `full_content: true`, so the 81 focused calls (71%) got whole pages; Parallel served every successful call. | Decided instead: with `focus`, return Parallel's objective-focused excerpts inline and write `full_content` to `local://web/`. Rejected: the small-model answerer (a second call per fetch; paraphrases API docs) |
| X7 | `web_fetch` loses the tail if `save_spillover` returns None, leaving only `[Truncated at …]` | VerifyAxomeHost A1.7. W7: the `None` branch is unreachable in Axome (built-in writable `local://`; 42 of 42 truncations saved). The reachable defect is the `Err` branch, which fails the whole call and discards the fetched page. | Decided: `save_spillover` never fails the call; on failure return the preview plus `[Truncated at 14000 characters; the full page could not be saved: <reason>.]`. Rejected: `artifact://` fallback or move, an axo spill helper |
| X8 | Carve WORK into separately removable sections (Axome host sections or more axo prompt names); possibly `work: false` on pure research scouts | Every child carries WORK (1,328 tokens, ≈3.7M cache reads) | Rejected: `work: false` on scouts saves ≈0.14M input-eq per two weeks, but scouts use WORK's tooling rules (nix in 28 of 287 bash calls); carving needs new axo prompt names for ≤0.12M more |

#### Axome notes

- **X1.** Do **not** fix with per-agent `tools:` allowlists on `task`: they also drop lsp, browser, extension and future tools. Per-tool sizes: `web_search` ~290, `web_fetch` ~130, `search_docs` ~430, `memory_search` ~350 (superseded by W7's measurement: 234, 127, 309 and 301). `scout.md:21` uses them in parallel. Per-folder `cfg.web_search.enabled` / `cfg.docs.enabled` is an alternative.
- **X2.** Behavioural reason too: WORK's "warm creative partner" persona conflicts with memory-writer's "exact format, no commentary". The fixed prefix is ≈30–35% of each writer session's first request; up to ~26k tokens per capture.
- **X4.** Attach also misses review scouts that consume a `git diff` of `.svelte` files, and root-level frontend config (vite/svelte.config). Keep Living Documentation and Important Notes at the root. A brand-new file in `src/` gets the rules only after the write. W7: the review-scout point is moot, because children never load context files today (A35); the root pointer covers new files and bash-only viewers.

### Personal (axophyte, `~/.config/axome`)

| # | Item | Status |
|---|---|---|
| P1 | `hidden: true` on the 15 workflow agents; keep `committer` and `pr` visible (needs A13) | Decided: once A13 lands. Every workflow spawn names its agent in mode or skill text; `committer` and `pr` stay listed because 13–15% of their omp spawns were model-initiated |
| P2 | `triage_findings` takes `local://` paths or agent ids. Review/cleanup stop re-typing findings and stop reprinting the report; the second cleanup call becomes cheap. The extension API resolves both (`docs/extension-api.md:118-121`). | Decided for arguments: the tool takes scout, omission, skeptic and fixer agent ids, reads `agent://<id>/result`, flattens the findings, assigns ids and writes the flattened file the skeptic reads; `gaps` stays inline. omp: ≈6.1k argument tokens per run, and 12 of 200 re-typed ids drifted. Smoke-test `readFile("agent://…/result")` from the extension first. Decided (Toph, 2026-11-27): instead of reprinting the report in chat, reply with the verdict line or section counts and the `local://findings.md` link ([Q5](#decisions-by-toph)). |
| P3 | `modes/plan/findings.md:32`: same wording fix as X3 | Rejected: leave line 32 unchanged. Lines 32 and 35 do not contradict (32 grounds what the plan states; 35 re-vets findings), and the one observed run read new files, not re-reads. X3's scout clause stands for Axome's bundled modes; the findings mode follows X3 only where the wording stays consistent with it |
| P4 | WORK.md: merge the testing rules at lines 34 and 67 | Decided (Toph, 2026-11-27): use draft (b), the neutral merge without the money and security clause ([Q6](#decisions-by-toph)). |
| P5 | `settings.jsonc`: personal values for the new axo settings, if they should differ from the defaults | Decided: no personal overrides now. The defaults are tuned on Toph's own journals; if blocking pays off after A1, the fix is the axo default (A1 #5), not a personal opt-in |
| P6 | Small-diff gate for review/cleanup fan-out: review:adversarial runs 8–9 agents (6 scouts + omission + 1–2 skeptics); decomplect requires a skeptic per proposal; no small-diff relief exists. Trades depth for cost, so Toph's call. | Decided for decomplect: keep the skeptic per proposal (38–48% of proposals challenged; `/decomplect:staged` covers small targets). Decided (Toph, 2026-11-27): no change to review or cleanup for now; keep small-diff relief under "Open ideas" in the axophyte README ([Q7](#decisions-by-toph)). |

#### Personal notes

- **P2.**
  - Usage is rare: 1 `triage_findings` call in two weeks (4.2k argument tokens). Output-token estimate 150–250 per finding, i.e. 4–10k for 20–40 findings. Triage's deferred schema (~1.5k tokens pretty-printed, ≈719 as compact JSON per W8) also shrinks with references. Both workflows already write flattened findings to a file "when large".
  - Checked by W8: Axome renders the tool result as raw monospace with a 12-line preview, so the chat reprint is today the only rendered copy; `local://` links in chat are clickable. The reprint is Q5.

## Harness mechanisms catalogue

Mechanisms from the survey, by harness, with the sources' citations. Items already adopted above are cross-referenced.

### omp 19.2.0

Paths relative to `S/` = `/nix/store/g29kgzj8wmj60ls6vvldgy5laz0vxcz4-source/packages/coding-agent/src`.
- **Read:** selectors in the path (`:50`, `:50-200`, `:50+150`, `:-60`, `:5-16,960-973`); a single range also shows 1 line before and 3 after; cut blocks get their open/close line; `:raw` is verbatim (`S/prompts/tools/read.md`). `read.defaultLimit` 300 lines (`settings.ts:161-163`); lines capped at 768 bytes (`settings.ts:85-87`). Unranged code files from 100 lines return declarations only (`settings.ts:192-254`). Directories list one level, children capped at 12. Third identical read gets a hint (`read.ts:1064-1110`).
- **Grep:** `;`-separated paths with `!glob` exclusions and file selectors; content mode only; context set by settings, not the model (`settings.ts:531-552`); 20 files per page, 10 matches per file multi-file, 50 single-file (`grep.ts:87-104`).
- **Interceptor** (`S/exec/settings.ts:29-104`, off by default `exec/settings.ts:197-206`): matches the whole command and each segment split on `; && || |`, each also with leading `VAR=` removed; a segment reading piped stdin is exempt; `cd … &&` is checked with and without the `cd`; a rule fires only if its target tool is enabled; user `patterns` add to the defaults and can be scoped by path (`S/tools/bash-interceptor.ts:36-90`). `sed -n`, `nl` and `ls` are **not** covered.
- **Tool commands in bash:** session tools installed as `<tool> args` / `--help` (`tools/tool-commands.ts:1-22`); `grep` stays top-level as the interceptor's target (`tool-commands.ts:44-58`). See A31.
- **Output:** results over 50 KB spill to `local://N.log` with 20 KB head + 20 KB tail, ≤500 lines inline (`settings.ts:12-126`; `output-meta.ts:565-640`); re-readable pages are not re-spilled (`output-meta.ts:589-592`).
- **Pruning:** `supersedeReads` and `dropUseless`, cache-aware (`session/session-maintenance.ts:870-920`; `context-settings.ts:359-380`). See A5.
- **Prompt lines:** "Read small files whole; ranges only for large files"; "NEVER confirm an edit by re-reading or grepping"; "Don't trunc output by habit" (`prompts/system/system-prompt.md`, `prompts/tools/bash.md`).

### pi 1.6.0

Paths relative to `P` = `/nix/store/561vpjwys0x8an78vgs3j4gkbqxg50d9-source/packages`.
- **Defaults:** `read`, `bash`, `edit`, `write`; grep/find/ls off; prompt says "Use bash for file operations like ls, rg, find" (`system-prompt.ts:85-120`).
- **Actionable notices:** `Use offset=K`, `Use limit=2N for more, or refine pattern` (`grep.ts:302-318`), and the exact `sed -n 'Np' path | head -c 51200` fallback for a huge first line (`read.ts:313-317`).
- **Bash output:** last 2000 lines / 50 KB; structured `full_output_path` or `full_output_error` (`bash.ts:56-60,288`).
- **codemode:** QuickJS script; only script output reaches the model; 10k `max_output_tokens` with head/tail spill (`P/coding-agent/docs/codemode.md:3-14`); BM25 `searchTools`/`describeTool` (`codemode.md:27-30`).
- **Cache:** `cacheWarming:"idle"` up to 30 min "when a refresh is expected to pay for itself" (`docs/settings.md:21`; CHANGELOG `:132,162`); Anthropic message threads send only new messages (CHANGELOG `:146`); system prompt split into tagged sections, only changed sections re-sent as a patch (`system-prompt.ts:51-56,245-258`).
- **Deferred tools:** `prepareLoadout` hides declarations (`docs/extensions.md:2786`).
- **Compaction:** window − `reserveTokens` 16384; keeps ≈20000 recent tokens (`compaction/compaction.ts:122-129,267-269,446-472`).
- **Rationale** ([blog](https://mariozechner.at/posts/2025-11-30-pi-coding-agent)): no MCP because of context overhead (13.7k–18k tokens per server); system prompt plus tools under 1,000 tokens. [Talk](https://ai.engineer/talks/pi-coding-agent) argues against hidden context mutation (unverified in detail).

### Octet `4eb91c22`

`B` = `https://github.com/skaft-software/octet/blob/4eb91c22f02bfbf2eb1bebddfbeefe7df83bc996`.
- **Read:** `n: line`, header `{path}:{offset}-{end}/{total} hash={sha256}`, footer `next_offset=K truncated=bool` (`B/crates/octet-agent/src/tools/read.rs`).
- **Edit:** optional `expected_hash` on `edit` and `write` instead of mandatory read-before-edit (A32); guideline "Keep edits[].oldText as small as possible… Do not pad with large unchanged regions." (A20); returns a unified diff, 3 lines of context, capped at 16 KiB (`builtin.rs` `MAX_UNIFIED_DIFF_BYTES`; A12).
- **Bash:** per-stream head/tail with `complete_stdout=true` / `truncated_stdout=head:N tail:M omitted_bytes:K` (`bash.rs` ~L1195-1450); spills capped at 64 MiB / 32 files per owner, evicted oldest first (`B/crates/octet-agent/src/tools/bash_spill.rs`); `max_output_bytes` default 1 MiB (`B/docs/configuration.md:62`).
- **Composition:** one script with nested calls; 256 calls, 30 s, 1 MiB store; `Only` mode (`B/crates/octet-agent/src/tool_composition.rs:1-40`).
- **Compaction** (`B/docs/context.md`): window − 16K reserve, keeps ≈20K recent, `compact_model`; summary only at a completed-turn boundary with read/modified file footers; compacts and retries on a provider "too large" error; `native-responses` mode uses OpenAI compaction; optional `octet-snap-compact` sends history as PNG frames.
- **Schema budget:** `Agent::set_tool_schema_budget_bytes` refuses an over-budget tool set before any provider call.
- **Cache** (`B/CACHE_AUDIT.md`, `B/docs/cache-warming.md`): breakpoint reachability fix after wide parallel batches (A26); warming default `streaming`, `idle` also covers the user; `/cache` diagnostics; `cache_retention` default "short".

### Claude Code (docs 2026-11-26; Piebald v2.1.331)

- **Read:** line number + tab, or `:` with the tab-aware separator ([CHANGELOG 2.1.285](https://github.com/Piebald-AI/claude-code-system-prompts/blob/main/CHANGELOG.md)); 25k tokens per call (`CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS`), whole files over 256 KB refused, partial views don't count for editing, `allow_large` ([tools-reference §Large files](https://code.claude.com/docs/en/tools-reference)).
- **Grep:** `files_with_matches` (default) / `content` / `count`, `glob`, `type`, `multiline`, `head_limit`, `offset` ([tools-reference §Grep](https://code.claude.com/docs/en/tools-reference); [tool-description-grep.md, cc 2.1.235](https://github.com/Piebald-AI/claude-code-system-prompts/blob/main/system-prompts/tool-description-grep.md)). Glob and Grep absent by default on Linux/macOS/WSL ([tools-reference §Glob](https://code.claude.com/docs/en/tools-reference)).
- **Accepted shell views** for read-before-edit and nested CLAUDE.md ([tools-reference §Edit](https://code.claude.com/docs/en/tools-reference)); edit freshness rules (A33; ResearchClaudeCodex §6, no URL given).
- **Bash output** ([tools-reference §Output limits](https://code.claude.com/docs/en/tools-reference)): see A16 notes.
- **WebFetch:** a small model answers a question about the page (X6).
- **Re-hydration after compaction** ([context-window doc](https://code.claude.com/docs/en/context-window)): A29.
- **API tool-result clearing** `clear_tool_uses_20250919` ([context editing doc](https://platform.claude.com/docs/en/build-with-claude/context-editing)): A4.
- **LSP** errors reported after each edit; ToolSearch for deferred tools.

### Codex (`main`, not pinned)

- No read tool ([handlers dir](https://github.com/openai/codex/tree/main/codex-rs/core/src/tools/handlers)); earlier experimental `read_file`/`list_dir`/`grep_files` ([issue #11652](https://github.com/openai/codex/issues/11652)), current status unknown.
- Shell-first prompt: "prefer using `rg`…" ([gpt_5_2_prompt.md:250](https://github.com/openai/codex/blob/main/codex-rs/core/gpt_5_2_prompt.md)); "Do not use python scripts to attempt to output larger chunks of a file" (`:251`); parallelize reads with `multi_tool_use.parallel` (`:252`).
- `exec_command` `max_output_tokens` default 10,000 ([shell_spec.rs](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/shell_spec.rs); [unified_exec/mod.rs:74](https://github.com/openai/codex/blob/main/codex-rs/core/src/unified_exec/mod.rs)), collected up to 1 MiB (`mod.rs:75`), `original_token_count`; `tool_output_token_limit` ([config reference](https://developers.openai.com/codex/config-reference)).
- Compaction: `model_auto_compact_token_limit`, end-of-turn compaction by percentage, server `/responses/compact`.
- `tool_search`, a `get_context_remaining` tool, MCP input schemas compacted above 5,000 bytes.
- [Codex Prompting Guide](https://developers.openai.com/cookbook/examples/gpt-5/codex_prompting_guide): wrapper tools close to the underlying command; "plan all reads → one parallel batch"; history ordered as all function_calls, then outputs.

### OpenCode v1.0.0 / v1.1.30

`[O0]` = `https://raw.githubusercontent.com/sst/opencode/v1.0.0/packages/opencode/src/`; `[O1]` = `https://raw.githubusercontent.com/anomalyco/opencode/v1.1.30/packages/opencode/src/`.
- Read `00042| text` in `<file>…</file>`, "Did you mean" up to 3 for a missing file (`[O1]tool/read.ts:115`); `list` with default ignore list plus `ignore[]` (`[O0]tool/ls.ts:8-37`).
- Grep sorted by mtime, capped at 100 (`[O0]tool/grep.ts`); bash prompt "MUST avoid … `find` and `grep` … `cat`, `head`, `tail`, and `ls`" (`[O0]tool/bash.txt`).
- `batch` 1–25 calls, experimental (`[O1]tool/batch.ts`, `[O1]tool/registry.ts:114`).
- Truncation at 2000 lines / 50 KB, saved for 7 days; hint delegates to an explore agent (`[O1]tool/truncation.ts`).
- Prune: skip last 2 user turns, keep newest 40k tool tokens, prune only if >20k freed (`[O0]session/compaction.ts:46-90`).
- `<file_diagnostics>` after edits (`[O0]tool/edit.ts:105-113`).

### Gemini CLI v0.10.0

`[G]` = `https://raw.githubusercontent.com/google-gemini/gemini-cli/v0.10.0/packages/core/src/`.
- `read_many_files`, `list_directory` (`[G]tools/ls.ts`); no line numbers (`[G]tools/read-file.ts:~90`).
- Optional "shell output efficiency" block: quiet flags, redirect to a temp file, then grep/tail/head (`prompts.ts:170-188`).
- Shell output over 4M chars keeps 1000 lines (first 1/5, last 4/5), wraps at 120 chars, saves the full output (`[G]config/config.ts:167-168`; `core/coreToolScheduler.ts:256-305,1025-1045`).

### Aider v0.86.0

- Repo map: tree-sitter tag graph ranked with PageRank (`aider/repomap.py:352-507`); `map_tokens` 1024, ×8 (`map_mul_no_files`) with no files in chat, binary-searched to fit within ≈15% (`repomap.py:45-52,116-128,654-676`).

### SWE-agent v1.0.0

- 100-line numbered window with path, total lines and lines above/below (paper §3, App. A.1 Table 4); `search_dir` file list with match counts, refuses over 100 files (`tools/search/bin/search_dir`); lint gate on edits; ablations above.

### Cline v3.20.0

`[C]` = `https://raw.githubusercontent.com/cline/cline/v3.20.0/src/`.
- `search_files` passes `--context 1` (docs say 2), capped at 300 results / 0.25 MB (`[C]services/ripgrep/index.ts:25,62,132,184-192`).
- Duplicate-read notice replaces earlier `<file_content>` copies (`[C]core/context/context-management/ContextManager.ts:543-612`).
- `list_code_definition_names` outline (`system.ts:147,583`); "You can use one tool per message" (`system.ts:32`).

### Roo Code v3.25.0

`[R]` = `https://raw.githubusercontent.com/RooCodeInc/Roo-Code/v3.25.0/src/`.
- `read_file` with up to 5 `<file>` entries, each with several `<line_range>` (`[R]core/prompts/tools/read-file.ts:4,16-56`); `  42 | text` (`[R]integrations/misc/extract-text.ts:112-131`); outline on truncated reads (`[R]core/tools/readFileTool.ts:531-581`).

### Cursor (docs only)

- Instant Grep local index; Explore subagent runs parallel searches and returns summaries ([search docs](https://cursor.com/docs/agent/tools/search)).
- Read "reads full files when appropriate"; List explores full trees with file counts and types ([1.4 changelog](https://cursor.com/changelog/1-4)).

## Rejected

| Claim | Reason |
|---|---|
| Lower the global 50 KB / 2000-line caps | 40 of 2,399 reads hit them; lowering adds paging turns |
| Child report cap | Applies only to oversized final text, which already links to `agent://…/result` |
| Shrink `web_fetch` 14k-char preview | ≈3.5k tokens, with spillover. The question-answering alternative is X6. |
| Clearing fires too late (as a code gap) | Timing is set by `compaction.maxContext` / `reserveTokens`; see A10 |
| `edit` schema repeats a rule | Three copies, ≈40 cached tokens, deliberate per-field |
| Spawn catalog grows without bound | Children never get `spawn` at maxDepth 1; root cost is A13 |
| Earlier or periodic clearing on a schedule | Each pass rewrites the suffix at 1.25×; break-even ≈46 turns (A5 notes); superseded by the cache-aware variant in A5 |
| System-prompt collapse invalidates the cache | Models with `system_updates` do not collapse. The 5 misses (976k writes) in the "thinking / mode / agent change" bucket are not proven collapses; that cost is tracked as A28. Revisit if Axome changes host sections often on models without `system_updates`. |
| maxTurns 50 / lower per-agent caps | Ceiling with a wrap-up turn; lower caps force re-runs. Root sessions have no turn cap at all; only children get maxTurns. |
| Loop breaker | 165 identical repeats, ≈21k tokens, mostly edit retries after stale failures (A1 root cause) |
| Multi-slice `read` because parallel calls exist | Overturned by data: Claude rarely parallelises (1.29 calls per message, 20% multi-call) and batches through compound bash instead. Now part of A1. |
| Reuse one memory-writer session | Per-chunk isolation is deliberate; the prefix cache already hits across sessions; X2 is the fix |
| Context-pressure hint | On merit: the session compacts on its own, so the model gains little from knowing, and a pressure note tends to make models skimp or wrap up early. Codex's `get_context_remaining` tool is the counter-example, noted. If revisited, axo owns it, not hosts. |
| Prefer `send` over a cold spawn | Already stated twice; workflows deliberately require fresh agents |
| WORK.md vs Ponytail contradiction; WORK.md vs AGENTS.md testing; WORK.md "verbatim" duplication | Precedence is explicit; the testing rules answer different questions; paraphrases cost ≈100 cached tokens. The textual conflict between WORK.md lines 34 and 67 is re-opened as P4 (Q6). |
| Plan modes demand exact literals | Deliberate deliverable spec; the real cost is X3 |
| Scout excerpts paid twice | Parents rarely edit scouted files themselves; excerpts replace reads |
| `web_search` "skip if known" clause | Invites stale-API errors |
| Drop WORK.md from all children | Review scouts check its Bun/Nix/Fish rules; committer needs Signatures; only memory-writer (X2). A finer per-agent drop is X8. |
| Strip web tools from scout | Intentional (`scout.md:21`) |
| Cap the number of review findings | Breaks adversarial completeness; P2 fixes the cost |
| Command-aware output condensers | Brittle; the data shows 0.9% collapse potential |
| Browser snapshot caps and schema dedup | 4 snapshot results in two weeks; screenshot cost is A4/A11. Worst case recorded: 24 KB per step × `MAX_BATCH_STEPS` 12 ≈ 288 KB per call, no batch cap, unlimited default depth, guidelines push a snapshot after each action; schema duplicated (≈600–1k tokens after promotion; avoid `$ref`). Measured: 98 results, 21 errors, 37 spilled. |
| Limit notices suggest "limit=2N" before "refine pattern" (`grep.rs:259`, `find.rs:187`) | Moot: 0 of 70 limit hits were followed by a larger limit |
| Huge single line sends the model to `bash sed … \| head -c` | No fix needed |
| Redundant tool and system breakpoints; unsigned thinking replayed as plain `text` (`anthropic.rs:1048`) | Redundant breakpoints have no merit to remove in the common case; the thinking replay is low |
| Automatic effort stepping (B9) | The capability exists (`set_thinking` `#[ext]`, mid-conversation effort markers via `effort_updates`); nothing drives it. On merit: a heuristic (low after read-only turns, high after errors) is unproven, risks quality on turns that only look mechanical, helps only models with effort markers, and otherwise breaks the cache. If revisited, axo owns the heuristic. |
| Repeated "Read-only /cleanup…" description prefixes; no warning against polling `agent://` | Prefixes ≈100 tokens, gone with A13; polling is already covered |

## Evaluation plan (2026-11-26)

**Goal.** Every item above that is not Decided or Rejected ends with one of two outcomes:
- **Decided**, with the evidence for the chosen option and the specific reason each alternative, including "do nothing", loses; the verdict must survive an adversarial skeptic.
- **A question for Toph**, asked only when the answer changes product goals or direction, or the evidence is a genuine toss-up; each question carries the practical context, the options and a recommendation.

**Evidence, strongest first.**
1. **Our data:** the axo/Axome journals (209 sessions, method above), plus omp's own journals under `~/.omp/agent/sessions/` as a natural experiment. omp already has line-numbered reads, range selectors, multi-path grep, cache-aware pruning and, in this config, the bash interceptor, so the same user and models on a different tool surface show what those features change. Confounds (different prompts, tasks and dates) are stated with every comparison.
2. **Targeted tests:** small, scripted live requests where provider behaviour decides the outcome, and offline simulations replaying journal timelines under candidate policies. No feature is implemented to test it. Live tests ran first against the OpenAI API (a short-lived key Toph provided). A follow-up ran against the Anthropic subscription through the `anthropic-auth` login (44 requests, see [Tests and scripts run](#tests-and-scripts-run)). There was no Gemini testing.
3. **Code:** axo, Axome and the surveyed harnesses at pinned versions.
4. **External research:** provider docs, harness sources, papers and author posts, cited.

**Per-item method.**
1. List every option, including "do nothing".
2. Gather evidence for each option.
3. Propose a verdict, with a refutation of each losing option.
4. Run a skeptic pass: a separate agent argues for the strongest losing option.
5. The verdict stands, flips (one more round), or becomes a question.
6. Record the result here.

**Phase 0: shared evidence base.**
- Extract omp journal metrics with the same definitions as the axo study:
  - bash view/search share and edit failure classes;
  - calls per message;
  - re-reads and confirm-reads;
  - cache misses by cause and gap;
  - effect of supersede/prune.
- Keep the analysis scripts reusable.
- Inventory which provider credentials are usable for live tests.

**Phase 1: workstreams, in parallel.**

| Workstream | Items | Main evidence |
|---|---|---|
| W1 Tools and bash | A1 #3 grep feature set, #5 blocking default, #6 bash guideline, #7 targeted-read guidance, line-number cost; A12, A17, A19 (dedup, near-match, collapse), A20, A22, A23, A31, A32 (write refinement), A34 | omp vs axo journals; axo data; harness sources |
| W2 Caching and providers | A2 TTL vs warming vs nothing; A11, A18, A26, A27, A28 | Replay simulation of journal request timelines under exact Anthropic billing rules (5m refresh rule, A/B/C split); Anthropic 1h TTL on subscription OAuth from docs and harness code (Claude Code, omp, pi); live OpenAI tests of `prompt_cache_key` and `prompt_cache_retention`; provider docs |
| W3 Clearing and compaction | A4, A5, A7, A8, A9, A10 (incl. the ≈50k compactions), A25, A29 | Live OpenAI tests of edited earlier `function_call` arguments and dropped earlier reasoning items (cache and acceptance); Anthropic equivalents from docs and harness code; simulation of `maxContext` and pruning policies on journals; omp pruning data; Anthropic `context_management` docs |
| W4 Batching | A3: guidance vs `batch` tool vs codemode (Track 1) | omp vs axo calls per message; published code-mode/programmatic-tool-calling results |
| W5 Output shaping | A15, A16, A21, A24 | axo data; harness sources |
| W6 Agents and prompts | A6 (outline, dependency), A13, A14, A30, X8 | Spawn and report data; omp outline behaviour and dependency cost |
| W7 Axome | X1–X7, plus X2–X4 confirmation | Tool usage frequencies in journals; Axome code |
| W8 Personal | P1–P6 | Workflow data; P6 is a likely question |

**Phase 2: skeptics.** One adversarial pass per workstream, on its verdicts only.

**Phase 3: consolidate.**
- Update the inventory: every item Decided, Rejected or Question.
- Write the question list for Toph.
- Add a short summary of tests run, with commands and results.

**Limits.**
- Live tests: OpenAI API only, minimal prompts, small models where the behaviour is model-independent, at most ≈60 requests in total. The final count was 63 (see [Tests and scripts run](#tests-and-scripts-run)).
- Journals of both harnesses are read-only.
- Reports carry shapes and numbers, never conversation content.
- No feature implementation and no changes outside this document and scratch scripts.

## Evaluation results (2026-11-26)

Eight workstreams (W1–W8) evaluated every item that was not yet Decided or Rejected. One adversarial skeptic per workstream attacked each verdict. Refuted or unverified verdicts were revised and attacked again (SkepticRound2, SkepticRound2b), or settled by the main agent. The inventory tables above carry each item's final status; this section gives the reasoning in one place. Costs are in input-equivalent tokens (IE: uncached input 1×, cache write 1.25×, output 5×; cache reads at the per-model rate, 0.05× on Opus 5.5 and 0.025× on Fable 5.2, unless a figure says 0.1×).

### Decided

**Tools and bash (axo, W1).**
- **A1 #1, numbered reads: ship `N⇥` with no A/B, then monitor.** Numbering adds 11.6% o200k tokens per read (≈10–14% for Claude) and is a likely net cost of ≈0.2–0.5M result tokens per two weeks. It is kept for the contract: citations, `edit`'s prefix strip, selectors, the total-line header. An A/B loses because two weeks give 35 Claude root sessions, too few to separate a 10–20-point effect, and it cannot isolate numbers from selectors.
- **A1 #3, grep:** several paths with `!` exclusions, `before`/`after` context, `content`/`files`/`count` modes, `-o`, file-grouped output, merged context windows, and a per-file cap of 10 (several files) or 50 (one file). No paging parameter. The cap, grouping and merging pay on every call: up to 22%, 15.5% and ≈36.6k tokens of grep output. Doing nothing leaves 56% of bash search units without a tool equivalent (20% after); files-by-default loses because the habit is content (112 of 2,704 units use `-l`).
- **A1 #4 extension, nested `AGENTS.md` on bash views: attach only**, via the public classifier, with no hash and no ledger record. Without it X4 misses ≈40% of its target sessions. The hashed, ledger-recording variant loses because it serves a guarantee `edit` no longer needs.
- **A1 #5, blocking:** one public bash classifier; the off-by-default setting blocks only a command that is a single tool-equivalent unit, naming the replacement call. omp-style regex rules lose: 71% of their axo matches are compounds. On by default loses: omp's view share was ≈8% before its interceptor too.
- **A1 #6, bash guideline:** two lines (tools for files, bash for pipelines, counts, git, jq, builds and tests; scope output with flags and don't pipe build or test output into `head`/`tail`), plus the read guideline restated for A1 #4. A general "don't truncate" line loses: `| head` on searches works as a `limit`.
- **A1 #7, targeted reads:** one ≈20-token line, low confidence. Large whole reads are 21% of read tokens; the line breaks even at ≈2 converted reads. The re-read clause loses: omp has it and still re-reads 6%.
- **A6, outline:** a parser-free, declaration-aware indentation outline for unranged Rust, TS/TSX, JS and Svelte reads ≥24 KB, behind a `read` threshold setting. Recall vs tree-sitter 99.7–100% out of sample; −2.2M to −5.2M cache reads per two weeks, saving in every scenario. omp's ≥100-line gate loses on files under ≈6k tokens; tree-sitter adds 4.54 MB and a C toolchain for 0.3% more TS declarations. The outline was rebuilt twice after skeptic refutations; the version decided here is v3, measured on three out-of-sample corpora.
- **A15:** compact JSON schema in `tool://` reads (−38% to −48% of schema tokens); omit `docs` and `schemes` in sessions with no tools. The promotion-aware read loses: the read cannot know whether promotion will happen, for ≈0.8M resident on Anthropic only.

**Caching and providers (axo, W2 and LiveOpenAI).**
- **A2, Anthropic on priced routes: gated warming (`cacheWarming`, default `idle`) plus a `cacheTtl: 5m | 1h` setting per bucket.** The replay simulator (exact TTL, mixing and A/B/C billing rules) gives −11.9% Anthropic IE at API prices with the decided 12.5k-token gate. The study's per-request 1h proposal loses even with a perfect predictor (+1.4%), because the first 1h block after a 5m run rewrites the prefix at 2×. The subscription default is Q1.
- **A2, OpenAI: do nothing.** 2,692 of 2,704 same-model Codex gaps are under the 30 m TTL; `12h` costs 2.5× writes to cover the 8 longer gaps that fall within 12 h, and warming pays only for returns within ≈5 h.
- **A27: the initial system prompt becomes a leading `developer` item, with `supports_mid_convo_system_messages` on OpenAI Responses rows (option g).** GPT-6.2 never caches top-level `instructions` (32% of 6.2 input cost in the journals; confirmed live). Live, a mid-conversation developer item kept 4,676 of 4,679 tokens cached and dropped nothing. Moving the prompt to the head without the flag (option e) gets the same saving but turns each prompt change into a whole-prefix rewrite, where (g) appends 28 tokens. Codex-endpoint acceptance is [INFERENCE] from codex-rs, which appends developer items mid-conversation.
- **A28: closed as TTL misses.** All 5 misses followed gaps of 5.3–22.1 min, and 9 of 9 change events followed within 5 min hit the cache.
- **No `cache_control` on one-shot Separate summary requests** (from W3, filed under A8). They run under a fresh session id, so they write their whole input at 1.25× and are never read (the Fable 181k compaction wrote 40,399, read 0). Keeping the breakpoint buys nothing.

**Clearing and compaction (axo, W3).**
- **A4: stub aged `edit`/`write` arguments and aged images inside the existing Clearing pass** (Gemini: only before the last real user message). −0.89% to −0.93% Anthropic IE at no extra cache cost, because the pass already breaks the cache. Thinking clearing loses: `drop_block` already drops later thinking on Opus 5.5 and Fable 5.2 (286k over 5 passes), and on OpenAI it equals `current_turn`. Per-request stubs lose: each needs 69–114 later requests to repay its rewrite (live Test C).
- **A5: cold-clear.** Run the Clearing pass before a request axo knows will miss, if the prompt is ≥150k and the pass removes ≥20k. It costs no cache, because the request rewrites anyway: −0.35% to −0.44% with warming on, −7.6% without. Supersede/useless loses: ≈2k tokens of material in the corpus.
- **A7: cap rendered call arguments with `tool_text`.** All 11 compactions cut mid-turn and used `separate()`; arguments were up to 90% of a split-turn render. Dropping arguments loses the paths and commands the summary needs.
- **A9: derived window min(40k, T/4) and headroom min(20k, T/10); one 400-token floor for every elision mode.** Unchanged at default windows; on the 64k Qwen session 2 of 3 compactions become clearings. Lower floors save 0.18 points but keep a mode-specific exception.

**Batching (axo, W4).**
- **A3: do nothing now; re-measure after A1.** axo already chains fewer lookups than omp on same-model, same-role rows, and omp's batching line cannot be separated from eight other prompt changes made the same day; its Claude arms looked net negative per session. The trigger is saved: `bun ~/.cache/axo-eff/scripts/W4Batching/trigger.ts --compare baseline-preA1.json --from <A1 ship date>`. A cell triggers when chains per 100 requests rise ≥25% (90% CI above 0) and requests per session rise ≥10%; then an A/B of the line decides. A default line loses because its effect is unproven and would contaminate A1's before/after.

**Agents and prompts (axo, W6).**
- **A13: `hidden: bool` on agent specs.** Saves ≈1.2M cache reads per two weeks; 0 of 633 omp workflow spawns used the listing. Reusing `disable-model-invocation` loses: in modes and Claude Code it blocks model invocation, and these agents must be spawned by the model by name.

**Axome (W7).**
- **X1: defer `search_docs` only.** 2 calls in two weeks against ≈1.64M cache reads carried. Deferring `web_fetch` loses: 22 calling sessions would pay ≈191k IE in round trips to save ≈68k.
- **X2: memory-writer gets `sections: {work: false, prose: false}` and `contextFiles: false`.** WORK+PROSE are 36% of each writer's first request and conflict with its fixed output format. `docs`/`schemes` belong to A15, which fixes every tool-less agent at once.
- **X3: scout-cited `path:line` excerpts count as evidence in the three bundled plan modes;** re-read only for verbatim quotes the scout did not quote, or doubtful claims. Plan modes re-read 58% of scouted paths vs 29% with no mode. Counting citations unconditionally loses: a `@dash` scout's paraphrase would become a plan literal.
- **X4: move the frontend rules (lines 57–95, 921 tokens) to `src/AGENTS.md` now, with a root pointer.** ≈0.86M cache reads per two weeks. Waiting for A1's bash-view attach loses: the pointer gives the same coverage for ≈25 tokens.
- **X5: mirror each new axo setting, enforced by a key-completeness check** in `settings_mirror_defaults_match_axo`, which today passes with a missing field.
- **X6: with `focus`, return Parallel's focused excerpts and spill `full_content`.** 71% of calls pass `focus` and get the whole page today. A small-model answerer loses: a second call per fetch and paraphrased API docs.
- **X7: a spill failure degrades to the preview with a reason.** The reported `None` case is unreachable; the reachable `Err` case discards a fetched page.

**Personal (W8).**
- **P1: `hidden: true` on the 15 workflow agents** once A13 lands; `committer` and `pr` stay listed (13–15% of their omp spawns were ad hoc).
- **P2, arguments: `triage_findings` takes agent ids, flattens findings and assigns ids itself.** ≈6.1k argument output tokens per omp run, and 12 of 200 re-typed ids drifted. A parent-written file loses: the parent still types every finding. The reprint is Q5.
- **P5: no personal setting overrides.** If bash-view blocking pays off after A1, the axo default changes (A1 #5).
- **P6, decomplect: keep the skeptic per proposal.** 38–48% of proposals were challenged, and `/decomplect:staged` already covers small targets.

### Rejected

| Item | Reason |
|---|---|
| A8 `compaction.model`/`thinking` | Unset by default it saves nothing; a cheaper default saves <1% of Anthropic IE on the path where summaries lose constraints. Its motivating case is free and fixed by A9 |
| A11 image cap and estimate | The 2000 px cap never bound (longest edge 1,721 px); 1568 px saves 1.2k tokens; the estimate error is corrected by the next usage report |
| A12 no-confirm-read line | Costs ≈9k IE per two weeks, about the most it could save (9 pure confirm reads, 5.9k tokens) |
| A14 report-size clause | Smaller reports go with more re-reads, not fewer; the cap never binds; plan-mode re-reads are X3's |
| A16 tighter bash output | Lower caps cut whole-file views and diffs the model pages back; small-window models never hit the cap; no host asks for a setting |
| A17 repeated-read hint | One third-or-later file repeat in two weeks |
| A18 `reasoning.context: current_turn` | A cost of ≈10× its saving, and it drops reasoning continuity |
| A19 dedup stub, near-match excerpt, collapse | 0.65% of read tokens; the excerpt would show the wrong neighbourhood in 12 of 17 recoveries; 0.01% of bash tokens |
| A20 minimal `oldText` line | Already in the `edit` description; padding is 2.8% of edit-entry tokens |
| A21 tail snapshot | ≈56k IE per two weeks; hides the head behind 2 of 6 kills |
| A22 `find` limit 200 | ≤18.8k tokens saved; omp's 200 hits the limit on 12% of calls |
| A23 validation echo | 4.6k tokens, no repeats; the echo is the model's only view of what the validator saw |
| A24 separate report limit | The shared budget is the documented contract; 0 of 366 reports capped |
| A25 raw `previous_summary` | A few hundred tokens on 3 update compactions, no duplication |
| A26 second rolling breakpoint | 0 lookback misses; at most 8 positions per request |
| A29 re-hydration | 85% of re-loaded files would never be re-read; axo keeps a raw tail |
| A30 nested axo `AGENTS.md` | ≈0.22M cache reads per two weeks, with invariants missed in bash-only sessions |
| A31 tools as bash commands | A1's multi-target tools absorb the compounds; no live ledger path from bash |
| A32 `expected_hash` on `write` | The ledger already catches stale writes; every read would pay ≈40 tokens |
| A34 missing-file suggestions, listing ignores | ≤5 round trips per two weeks; `.gitignore` already applies |
| X8 carving WORK | ≤0.14M IE per two weeks; scouts use WORK's tooling rules |
| P3 findings-mode wording | Lines 32 and 35 do not contradict, and the one run read new files; the Confirmed-findings clause would let the planner write steps for code it never read |

### Decisions by Toph

**Answers (2026-11-27):** Q1 option 1, Q2 option 1 (client-side on every model), Q4 option 2 and Q5 (b) follow the recommendation. Q3: `maxContext` stays at 400k for everyone, and a separate clearing threshold at 75% of the window runs clearing earlier (see Q3). Q6 is (b), the neutral merge without the money and security clause. Q7 is (a), noted under "Open ideas" in the axophyte README for later.

**Q1. Anthropic cache policy on the subscription (A2).** All of your Anthropic traffic is subscription OAuth, whose meter is unpublished. The best policy depends on whether the plan charges 1h cache writes at the API's 2× premium. The live follow-up confirmed every mechanism the options rely on, but could not measure that weight:
- 1h works on OAuth with axo's current betas, as long as every earlier breakpoint is also 1h.
- `max_tokens:0` warm pings are accepted, read the whole prefix with 0 output, and refresh the 5m TTL.
- The plan meter moves in whole 1% steps (≈200k–400k uncached Opus tokens on Max 5x), and this session shares it. A clean measurement would cost 2–3% of a 5-hour window in a quiet window; Toph declined.
- Options, as simulated change in Anthropic cost:
  1. 1h for root conversations plus gated in-run warming for children (Claude Code's subscription default). Roots mark tools, system and messages 1h together. Worst case +1.6%; with no 1h premium −0.1% to −16.6%; at most 13 extra requests per 5-hour window.
  2. Gated warming everywhere, as on API routes: −11.9% at API prices, up to +10.8% if reads weigh fully.
  3. In-run warming only: max regret 8.4 points.
  4. Ungated fixed-cap warming: lowest max regret (7.2), worst case +6.7%, 24–92 extra requests per window.
- Recommendation: option 1. It follows the only first-party signal and adds almost no requests. Switching to option 2 is a settings change if the meter turns out to charge the premium.

**Q2. Client-side or server-side clearing on Opus 5.5 and Fable 5.x (A4).** Each client-side clearing pass makes the API drop every later thinking block (286k tokens over 5 passes; confirmed live, see T5). Anthropic's docs recommend server-side context editing, which keeps reasoning.
- Options:
  1. Keep client-side clearing: expected +0.1% to +0.4% from re-derived reasoning; worst case +2.7% if all dropped thinking is re-derived. A pass breaks the cache once.
  2. Server-side `clear_tool_uses` on preserved-thinking models. Live on OAuth it is accepted and keeps all thinking. But once it triggers, the cleared range grows with every tool call, so every request rewrites the cache after the last entry written before clearing began (10.3–10.8k of a ≈19k prompt per request in the test). The +1.65% estimate left this out.
     - With axo's current breakpoint layout the rewrite grows without bound.
     - With a new breakpoint at the oldest kept result it is about (keep − 1) × result size at 1.25× per request.
     - `clear_at_least` only delays the first trigger.
  3. Client-side, also stripping thinking before the cut.
- Evidence: after passes, Fable produced more output in 2 of 3 sessions (one at 5.88×, outside the no-pass band); Opus showed no effect. n = 5 passes.
- Recommendation: option 1 on both models. Server-side clearing turns a rare one-time cache break into a cost on every request. It is worth revisiting only with boundary-breakpoint placement, and only if Fable's reasoning loss proves real.

- Scope (asked by Toph): the dropped thinking is not tied to the login or to two models. It applies to every model that keeps earlier thinking in context and binds it to the exact preceding text. On Anthropic these are the models flagged `effort_updates` in the catalog (Opus 5.5, Fable 5.x; axo then sends `drop_block`, `rules.rs:243`, `anthropic.rs:708-716`). OpenAI GPT-6.2 behaves the same way, removing later reasoning after an edit. Older models strip earlier thinking anyway, so clearing loses nothing there. API keys and OAuth behave identically. Option 1 is therefore the provider-neutral path with no model-specific code. Option 2 would be the Anthropic-only special case.

**Q3. Default `maxContext` for 1M-window models (A10).** Only 7 of 205 sessions (long axophyte implementation roots) ever exceed 250k. The quality literature does not separate 250k–400k on current models.
- Options, as change in Anthropic cost:
  1. 400k, today: 0%; 5 clearing passes.
  2. 350k: −1.1% to −2.4%; 6 passes; no extra summaries.
  3. 300k: −2.4% to −5.1%; 11 passes; no extra summaries.
  4. 250k: −1.2% to −7.4%; 30 passes and 1 extra summary.
- Each pass also drops later thinking; at 250k the worst-case re-derivation could turn the warm-cache saving into a +0.6% loss.
- Recommendation: 300k, the lowest cap that adds no summary; choose 250k for multi-hop-heavy work, or 400k if reasoning continuity matters more than 2–5% of cost.

- In plain terms: `maxContext` is how large a conversation may grow before axo shrinks it. Models accept 1M tokens, and axo caps at 400k today. Every request re-sends the whole conversation, mostly as cheap cache reads, so a long session pays for its size on every turn. A lower cap shrinks sooner:
  - First, a clearing pass replaces old tool results with short stubs. It breaks the cache once and drops the thinking after the cut.
  - At the hard limit, a summary replaces older turns. A summary is lossy.

  Lower caps make requests cheaper but run more clearing passes and eventually summaries. 300k is the lowest cap that adds clearing passes (11 instead of 5 in the corpus) but no extra summaries, and it saves 2.4–5.1%. Only 7 of 205 sessions ever reached the range where the cap matters. Research cannot tell whether models reason worse at 300k than at 400k.

- **Decided by Toph (2026-11-27): keep `maxContext` at 400k, and clear earlier.** Toph asked for 400k with a somewhat more aggressive threshold, and W3 simulated the shapes with A5's cold-cache clearing on in every arm (`W3Clearing/sim.ts --tiers`, `out/tiers-summary.txt`). In the table, ΔIE is relative to today across four bases: per-model reads, 0.1× reads, warming, warming + 0.1×. Passes are shown without and with warming.

  | Arm | ΔIE | With worst-case re-derivation | Passes | Extra summaries | Dropped thinking |
  |---|---|---|---|---|---|
  | Today, 400k | 0 | 0 | 17 / 7 | 0 | 494k / 317k |
  | 300k cap | −0.7 / −2.6 / −2.4 / −5.1% | −0.6 / −2.4 / −2.1 / −4.9% | 22 / 13 | 0 | 512k / 350k |
  | Clear at 250k, summary unchanged | −0.8 / −3.0 / −2.0 / −5.3% | −1.0 / −3.1 / −1.1 / −4.7% | 22 / 17 | 0 | 481k / 398k |
  | **Clear at 300k, summary unchanged** | −0.7 / −1.7 / −1.9 / −3.3% | −0.9 / −1.8 / −1.9 / −3.4% | 19 / 9 | 0 | 477k / 313k |
  | Clear at 330k, summary unchanged | +0.2 / −0.4 / −0.7 / −1.6% | +0.1 / −0.5 / −1.0 / −1.8% | 17 / 8 | 0 | 479k / 291k |
  | Larger headroom (60k or 100k) | 0 | 0 | 17 / 7 | 0 | 494k / 317k |

  - The decision is a clearing threshold at 75% of the window (300k at the default cap), separate from the summary trigger, which stays at `trigger = W − reserve`. Clearing still has to bring the context below the clearing threshold minus 20k. If it can't, nothing else happens until the summary trigger; there are no early summaries. Headroom stays as it is: clearing already removes everything eligible, leaving prompts at 136–196k.
  - Why 75% wins: it saves in every base even after worst-case re-derivation, drops no more thinking than today, adds no summaries, and adds only 2 passes with warming on. It lands within 0.2–1.8 points of the 300k cap while keeping the full 400k for sessions that need it.
  - Why the others lose:
    - 250k is better only without warming, and with warming it drops 81k more thinking.
    - 330k barely moves.
    - Larger headroom changes nothing.
    - The 300k cap would take away the extra 100k that Toph wants available.
  - OpenAI is unaffected (0.00%) in every arm.

**Q4. Should subagents see area-specific project rules (A35)?** Today children get no project context, so 55 `.svelte` edit calls by `task` children ran without Axome's Svelte rules.
- Options, priced after X4 per two weeks:
  1. Keep `Off`: 0.
  2. A nested-only mode as axo's child default (no root file, area files on first touch): ≈140k IE, a lower bound.
  3. Nested-only as an opt-in per agent, set in Axome's `task.md`.
  4. Full context for children: ≈677k IE.
- Recommendation: option 2. It ships the efficient default with zero host code and delivers exactly the area rules; the parent's brief carries the global context.

**Q5. Reprint the triage report in chat (P2)?** Every run reprints it: p50 ≈1.6k output tokens and ≈11 s, plus ≈1.7k tokens left in context. The tool card shows only 12 raw lines, so the reprint is today the only rendered copy; `local://findings.md` is clickable.
- Options: (a) keep the reprint; (b) reply with the verdict line or section counts and the `local://findings.md` link.
- Recommendation: (b), unless you read reports in chat rather than in the file.

**Q6. One testing rule in WORK.md (P4).** Line 34 allows kept tests only for critical invariants; line 67 asks for a "regression check" on non-trivial logic, which reads as a kept test.
- Draft: "No speculative, boilerplate, or mock-heavy unit tests. Write a test only when it guards a critical invariant or contract, something we MUST know if it breaks, drifts, or needs changing. Other non-trivial logic (branches, loops, parsers) gets the smallest runnable check: a throwaway smoke run that exercises the code and shows the output. Trivial one-liners need neither. No frameworks, fixtures, or per-function suites unless asked." It replaces line 34; line 67 is deleted.
- Options: (a) the draft plus "money and security logic count", a new policy that gives money and security logic a kept test; (b) the draft as written, the neutral merge; (c) keep both lines.
- Recommendation: (b). Choose (a) only if you want that new policy. Children make 52 of 63 axo test-adding edits and carry both lines today.

- Decided by Toph: (b).

**Q7. Small-diff relief for `/cleanup` and `/review:adversarial` (P6).** A review costs p50 ≈4.4M eq and a cleanup ≈1.9M. Cleanups under 500 changed lines were 5 of 22 runs and 17% of cleanup spend, at 3.7–5× the cost per confirmed finding. No small review was observed.
- Options: (a) no change; (b) an opt-in `quick` argument that keeps the audit and quality scouts (94 of 142 confirmed cleanup findings) and the skeptic; (c) an automatic gate under 300 lines.
- Recommendation: (a) for now: the evidence is 2–5 runs and every scope yields confirmed findings. Choose (b) if you often run these on small diffs.

- Decided by Toph: (a). Small diffs are rare in his use; the idea is noted under "Open ideas" in the axophyte README.

### Measurement notes and corrections

- **Tokenizer.** On read text, Claude counts ≈1.68 tokens per o200k token (median over plain single-read steps, W1 `claudetok.ts`), and chars/4 ≈ o200k here. Every chars/4 estimate of Claude volume in this study is therefore ≈1.6–1.7× low in Claude's own tokens; W3 independently fitted K = 1.672. Absolute token figures are approximate; relative comparisons stand.
- **Anthropic read price.** The cost split in [Where the tokens go](#where-the-tokens-go) prices cache reads at 0.1×. Opus 5.5 reads cost 0.05× and Fable 5.2 reads 0.025×, so that split overweights reads; W2 and W3 use the per-model rates.
- **Children's `.svelte` edits.** W7's "78 `.svelte` child edits" does not reproduce. The correct figure is 55 successful edit/write calls (173 edits) in 15 child sessions (`W6Agents/child-ctx.ts`); 78 is the number of Axome child sessions.
- **A18 reversal.** `reasoning.context: current_turn` is a cost, not a saving: it renders like stripping earlier reasoning and breaks the cache at every new user turn (≈1.29M IE of rewrites against ≈0.13M saved). The study had proposed it as a default.
- **A2's first estimate** (≈5–6M IE saved by per-request 1h) ignored the TTL mixing rule; under the exact rules per-request 1h is a loss. W2's first simulation also priced the warming gate with the swept read weight, which produced a false "0.0%"; the gate now always prices at API rates.
- **Clearing drops thinking.** On Opus 5.5 and Fable 5.2 axo sends `drop_block`, so each clearing pass drops every later thinking block. Earlier thinking is not "kept and billed" after a pass, as the A4 notes assumed. Clearing's local target check (chars/3.5) undercounts Claude by ≈1.46×, with no observed consequence.
- **The ≈50k compactions** (A10) are a 64k Qwen window with trigger 49,152, where clearing could never qualify.
- **Superseded sizes:** axo `AGENTS.md` is 2,094 tokens, not ≈2.9k; Axome's frontend rules are 921 tokens, not ≈1.3k; X1's tools are 234 / 127 / 309 / 301 tokens, not ≈290 / 130 / 430 / 350; the triage schema is ≈719 tokens as compact JSON (≈1.5k pretty-printed).
- **Recounts:** 27 confirm-reads, not 29 (A12); 33 identical-output re-reads, 29 of them `proc://` polls (A17, a different definition from the earlier 41); the validation echoes live at `run.rs:1128-1133` and `axo-ai/src/json.rs:86` (A23).
- **A26 conflict** resolved: Octet's figure came from a simulator that counts each block separately.
- **X3's "research mode" row** is the script's other-mode bucket, which covers any non-plan mode.
- **X7:** the reported `None` failure is unreachable in Axome; the defect is the `Err` branch.
- **A1 #4:** it can turn some "changed since read" failures into "could not find" failures [INFERENCE]; the post-A1 failure mix decides whether A19's near-match hint returns.
- **A6:** the first two outline versions failed skeptic review (top-level only; then a JSDoc line broke the indent unit). v3 was re-measured in sample and on three out-of-sample corpora.
- **W4:** its first revision's projections from the omp experiment (+1,380 requests) and the "ship with A1" coupling were withdrawn.
- **A4 on OpenAI 6.2:** the live tests left two 6.2-safe variants untested: deleting whole aged call+output pairs, and OpenAI server compaction. They are not adopted. Today's result-text clearing already triggers the same `prefix_mismatch` removal of later reasoning, so stubbing arguments inside the same pass adds no new loss.
- **Anthropic live follow-up.** It confirmed several earlier inferences on the subscription:
  - 1h without the extended-TTL beta;
  - warm pings with `max_tokens:0`;
  - stubbing call arguments drops no thinking beyond what stubbing the result drops: all three stub variants dropped the same 5 later blocks and kept the call's own thinking.
  - New constraint: a 1h breakpoint may not follow a 5m one (400). The A2 1h modes must therefore mark tools and system 1h too.
  - A 400 on `thinking: {type: "disabled"}` for Opus 5.5 is not reachable from axo. The catalog maps Off to unsupported, and `clamp_level` moves it to Low.

### Tests and scripts run

**Live OpenAI requests: 63** (API key, `store:false`, non-streaming, plus one `GET /v1/models`), against a plan limit of ≈60.

**Live Anthropic requests: 44** on the subscription (`anthropic-auth` OAuth, through axo's credential store; the token was never copied or logged): 40 succeeded, and 4 returned 400 (two of them findings). They used about 5 points of a 5-hour window and 1 point of the weekly window. A dedicated meter-weight run (T1b) was cancelled before sending anything.

| Test | What it settled |
|---|---|
| T0: meter precision | Rate-limit headers and `/api/oauth/usage` both report whole percent; one 5h step is ≈200k–400k uncached Opus input on Max 5x |
| T1: meter weights | Not resolvable within budget; the 1h-write weight stays the Q1 unknown |
| T2: warm ping | `max_tokens:0`, `stream:false` accepted on OAuth; reads the full prefix, 0 output; pinged prefixes hit at 8 min while the control missed (A2) |
| T3: 1h TTL | Honoured with axo's betas (read back after 361 s); a 1h breakpoint after a 5m one returns 400 (A2) |
| T4: server-side clearing | Accepted and keeps thinking; each request after the trigger rewrites everything after the last pre-clearing entry (Q2) |
| T5: client-side stubs | Stubbing arguments, the result, or both drops the same 5 later thinking blocks and reads the same cache (A4) |

OpenAI tests:

| Test | Requests | What it settled |
|---|---|---|
| A: prefix caching and `prompt_cache_key` | 8 | Before GPT-5.6 a different key misses on the same prefix, so axo keeps its per-session key; on 5.6+ only boundaries an earlier request ended on are reusable (A27) |
| B: retention after idle gaps | 12 | The 5.6 30 m entry expired by 36 m and `12h` survived; explicit `24h` gave no benefit on gpt-5.4 (A2 OpenAI, A27) |
| C: clearing an earlier call's arguments or output | 11 | Accepted; the cache survives only to the edit; break-even 69–114 later requests (A4) |
| D: reasoning items, `reasoning.context`, `instructions` | 28 | 6.2 never caches `instructions` and does cache a leading developer message; `current_turn` bills like stripping; on 6.2 an edit before reasoning removes later reasoning (A27, A18, A4) |
| Probe: `reasoning.context` default | 1 | gpt-6.1-sol defaults to `all_turns` (A18) |
| Mid-conversation developer item (W2) | 3 | An appended developer item keeps the earlier prefix cached and drops nothing (A27 option g) |

**Simulators and their validation.**
- `W2Caching/sim.ts`, Anthropic TTL, mixing and warming replay: reproduces journal cache reads 365.30M vs 365.38M and writes 19.35M vs 19.28M; 62 gap misses vs the study's 61.
- `W3Clearing/sim.ts`, clearing and compaction replay: Anthropic IE −0.06% vs the journal, reads +0.01%, writes −0.1%; reproduces all 5 clearing passes in the same sessions and the post-pass prompts exactly; OpenAI IE exact.
- `W1Tools/claudetok.ts`, Claude token fit over 2,277 single-read steps (a = 1.60 per o200k token on bodies, b = 2.65 per numbered line).
- `W4Batching/trigger.ts`, the post-A1 batching trigger: the baseline compared with itself gives `ok` on every eligible cell.
- `W6Agents/a6-outline3.ts`, outline vs tree-sitter ground truth: recall 99.7–100% on three out-of-sample corpora; `a6-bands.ts` prices the size gate.
- `OmpMiner/stats.ts`, the Phase 0 metrics pass: on the axo corpus it reproduces the study's usage, calls-per-message histogram, TTL misses and edit failures exactly.

**Script directories,** each script with a usage header and saved outputs beside it, all under `~/.cache/axo-eff/scripts/`: `OmpMiner/` (Phase 0 and the shared loaders), `W1Tools/`, `W2Caching/`, `W3Clearing/`, `W4Batching/`, `W5Output/`, `W6Agents/`, `W7Axome/`, `W8Personal/`, `LiveOpenAI/` (request log in `out/requests.jsonl`), and `LiveAnthropic/` (log in `out/log.jsonl`: usage, rate-limit headers and request ids only). Journals were read only; no feature was implemented.
