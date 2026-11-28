# Codemode engine spike

**Decision (2026-11-27): ship `rquickjs` 0.14.0 with `std` only.** It passed the go/no-go checks on x86_64 Linux. Version 0.15.0 also passed those checks, but was published the same day; the plan requires at least two weeks in the field. The difference in caught-OOM behavior is noted below. Neither `aarch64-darwin` nor `aarch64-linux` was available, and there was no cross-target build or runtime verification.

Background: [decision record](../../.axome/research/codemode/decision.md), [engine comparison and spike checklist](../../.axome/research/codemode/engines.md#spike-checklist-any-failure-falsifies-the-choice-or-the-config), and [industry prior art](../../.axome/research/codemode/prior-art-industry.md). Those working notes are local to checkouts that carry `.axome/`.

## Checklist results

Standalone release probes ran under the axo Nix dev shell on `x86_64-unknown-linux-gnu`, with an 8 MiB worker thread and a 1 MiB engine stack limit. No `parallel`, `futures`, loader, custom allocator, or generated bindings in the main comparison. Timings are single-run observations unless stated otherwise.

| Check | 0.14.0 (QuickJS-NG 0.16.2) | 0.15.0 (QuickJS-NG 0.18.0) |
|---|---|---|
| 1. 64 MiB heap cap | `out of memory`, process alive, fresh run succeeded; allocator reported 66,448,104 B at failure | Same; 66,450,232 B |
| 2. Single huge allocation | Giant string and buffer rejected their lengths; huge array fill and JSON source hit OOM; no aborts | Same outcomes |
| 3. Caught OOM loop | Still interrupted at 200.37 ms | The OOM escaped `catch` immediately (~74 µs); no deadline needed for this case |
| 4. Tight loop and catch/finally | 200.01–200.02 ms; catch/finally did not execute | ~200.04 ms; same behavior |
| 5. Cross-thread cancellation | Spinning and waiting both stopped at ~25.1 ms with a 25 ms cancellation trigger | ~25.2–25.5 ms |
| 6. Regex backtracking | Interrupted at 200.06 ms | Interrupted at 200.14 ms |
| 7. Non-polling builtins | See below | See below |
| 8. Stack and promise depth | Recursive functions, one-million-deep JSON stringify and parse, parser nesting, and a stringify getter recursing toward a host call hit the stack guard. 20,000 `.then` jobs settled without native recursion. | Same |
| 9. Async fan-out | 200 results matched their requests despite deliberately out-of-order completions, 16 host futures in flight; injected rejection retained its message and stack | Same |
| 10. Rejection/job diagnostics | Top-level and orphaned promise rejections reported message and source line; a throwing `queueMicrotask` reported its exception using `Runtime::execute_pending_job`. The boolean `Ctx::execute_pending_job` instead discards the exception. | Same diagnostics with `Ctx::drain_jobs` and `JobError::into_value` |
| 11. Fresh VM | Median 57.85 µs; 164.3 KiB PSS/VM | Median 66.59 µs; 167.7 KiB PSS/VM |
| 12. Compiled in, unused | +418 KiB idle PSS in the matched probe; not zero | +402 KiB median idle PSS; not zero |
| 13. No ambient capabilities | No `require`, `process`, `fetch`, timers, `std`, `os`, or bridge global; dynamic import rejected; `Function("return this")()` saw only the original intrinsics and the injected, non-replaceable `tools` | Same |
| 14. Language | Optional chaining, nullish assignment, destructuring, generators and async iterators, BigInt, typed arrays, grouping, nonmutating sort, lookbehind and named groups, Unicode `u`/`v` regex, ISO date, and source stack lines worked; `structuredClone` and `Intl` absent | Same |
| 15. Build | Linux release build with `LIBCLANG_PATH` unset and no bindgen; size below | Same; cross-target builds unverified |
| 16. Optional `rust-alloc` | 64 MiB cap still produced recoverable OOM | Same. This feature remains off. |

### Deadline overshoot

These probes intentionally raised the heap cap to 1,400 MiB to make the target inputs fit. The 100 ms deadline started immediately before the operation; once the builtin returned, a polling JS loop made the interrupt observable. Sort used ten million deterministically shuffled integers and the default comparator. These are not worst-case bounds or measurements at the production 256 MiB default heap.

| Operation | 0.14.0 overshoot | 0.15.0 overshoot |
|---|---:|---:|
| `new Array(5e7).fill(0).indexOf(1)` (array created before measuring) | 413 ms | 727 ms |
| `fill(0)` alone on a 50-million-element array | 322 ms | 648 ms |
| `indexOf(1)` alone on the already filled array | 112 ms | 126 ms |
| Ten-million-element `sort()` | 6,983 ms | 9,341 ms |
| `JSON.parse` of a ~50 MB dense numeric array | 702 ms | 733 ms |

A script slot stays occupied until the worker exits. The engine interrupt is not a hard wall-clock deadline; use a supervised subprocess or wasm if cancellation must preempt these builtins or isolate native engine failures.

### Footprint

Measured PSS from `/proc/self/smaps_rollup`. In check 1 it was 1,926 → 67,918 → 3,158 KiB on 0.14 and 1,219 → 67,219 → 2,457 KiB on 0.15 (before VM creation → at the cap → after drop). For check 11, the timing was the median of 1,000 sequential runtime/context create/destroy pairs; PSS per instance was the difference between baseline and 1,000 *simultaneously* live VMs. The allocator retained most pages after dropping that artificial 1,000-VM batch. The actual sandbox has only four slots.

A matched minimal stripped Rust binary measured engine link cost: both builds read their own PSS, and the feature-on build had an alternate command-line path that initializes the engine and evaluates JS. Neither build used that path for the five idle samples. These numbers isolate the engine in the probe; they are not a measurement of a complete axo consumer, and the idle delta includes resident pages from dynamic loading/linking, not an initialized VM.

| Build | Stripped bytes | Idle PSS (median of five) |
|---|---:|---:|
| Feature off | 364,768 | 536–537 KiB across the two comparisons |
| 0.14.0 linked | 1,682,664 (+1,317,896) | 954 KiB |
| 0.15.0 linked | 1,671,600 (+1,306,832) | 939 KiB |

## Live checks and measurement

Run 2026-11-27 on branch `codemode`, x86_64 Linux, with `crates/axo/examples/drive` built with `--features codemode`.

### OpenAI custom-tool wire

Asked explicitly to use `codemode`, gpt-5-mini called it three times in one session. Each call arrived as a `custom_tool_call` (item ids `ctc_…`), and later turns replayed the earlier calls as `custom_tool_call` plus `custom_tool_call_output` without API errors. The first script used `a ?? b && c`, which JavaScript rejects without parentheses. The tool reported `Unexpected token '&&' (line 19)`, which is the correct line, and the model fixed it on the next call. The second script made 224 nested calls (one `find`, 223 `read`) but neither printed nor returned anything, so the result was `(no output)`. The third script made the same 224 calls and printed a table: 448 `nested_tool_call` entries in total, grouped correctly under their two root calls. The table's per-crate `.rs` and `pub fn` counts matched ripgrep exactly.

### A/B: codemode enabled vs disabled

Three neutral prompts in the axo worktree, one session each: per-crate `.rs` file and `pub fn` counts (T0), the five largest `.rs` files (T1), and which files in `crates/axo/src/tools/` contain a string (T2). "Input" is uncached input tokens summed over requests.

| Model | Mode | Task | Requests | Input | Cache read | Output | Cost (USD) | Top-level calls | Scripts |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|
| gpt-5-mini | on | T0 | 2 | 6,362 | 6,784 | 911 | 0.0036 | 1 | 0 |
| gpt-5-mini | on | T1 | 2 | 6,486 | 6,272 | 443 | 0.0027 | 1 | 0 |
| gpt-5-mini | on | T2 | 3 | 6,810 | 12,800 | 657 | 0.0033 | 2 | 0 |
| gpt-5-mini | off | T0 | 6 | 10,488 | 40,960 | 1,477 | 0.0066 | 5 | 0 |
| gpt-5-mini | off | T1 | 2 | 5,914 | 5,888 | 484 | 0.0026 | 1 | 0 |
| gpt-5-mini | off | T2 | 3 | 6,336 | 11,904 | 910 | 0.0037 | 2 | 0 |
| gpt-5.6 | on | T0 | 2 | 6 | 6,253 | 341 | 0.0421 | 1 | 0 |
| gpt-5.6 | on | T1 | 2 | 6 | 6,238 | 181 | 0.0381 | 1 | 0 |
| gpt-5.6 | on | T2 | 2 | 6 | 6,247 | 295 | 0.0409 | 1 | 0 |
| gpt-5.6 | off | T0 | 2 | 6 | 5,764 | 338 | 0.0391 | 1 | 0 |
| gpt-5.6 | off | T1 | 2 | 6 | 5,749 | 187 | 0.0356 | 1 | 0 |
| gpt-5.6 | off | T2 | 3 | 9 | 11,633 | 228 | 0.0392 | 2 | 0 |

All answers were correct, except that shell-based T0 answers counted 242 `pub fn` lines in `axo` because `grep -R` also matched the crate's Markdown docs. Neither model chose `codemode` for any of these prompts: one shell pipeline or one `grep` answered each. Having the tool available costs about 450–490 more prompt tokens per request, cached after the first. The gpt-5-mini T0 "off" run took six requests against two, but it never had codemode as an option, so the gap is run-to-run variance, not a codemode effect. Single runs per cell; not a statistically meaningful comparison.

### Footprint compiled but unused

Release `crates/axo/examples/memory` built with and without `--features codemode` (codemode disabled in settings either way), scripted provider, three runs each:

| Build | Binary | 1 session idle / mid-turn | 10 sessions idle / mid-turn |
|---|---:|---|---|
| Feature off | 33,582,856 B | 17.4–18.4 / 18.3–19.3 MiB | 20.4–20.9 / 23.7–24.9 MiB |
| Feature on | 35,969,288 B (+2.3 MiB) | 17.4–17.9 / 18.3–18.7 MiB | 20.2–20.6 / 23.6–24.2 MiB |

No idle or mid-turn PSS difference is measurable.

### Not yet measured

- An Anthropic model.
- Prompts where a script should win: many non-shell calls such as spawn/wait fan-out or read-then-edit across many files, or modes without `bash`.
- The corpus re-run against the batching baseline in `efficiency.md`.
- PSS after one 200 MB script.
- aarch64-darwin.

Default enablement and model gating stay undecided until these exist.
