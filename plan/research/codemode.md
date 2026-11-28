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
