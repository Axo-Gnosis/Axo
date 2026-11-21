# Bun extension placement: historical probes and settled direction

A one-off probe recorded on 2026-11-15 (reported run window 16:00–16:27 −05:00) compares three ways of placing TS extensions inside one Bun process. Its observations remain a historical baseline. A [2026-11-16 follow-up](#process-placement-follow-up-2026-11-16) adds separate processes; the [settled direction](#settled-direction) supersedes the original recommendation, not the measurements. Survey context: [`extensions.md`](extensions.md); implementation roadmap: Track 5 in [`plan.md`](../plan.md).

**Status of each part:** sections through [Limits](#limits) are the original observations. [The probe author's recommendation](#the-probe-authors-recommendation) is historical interpretation, not the approved direction. Follow-up observations and approved decisions are labeled separately; none establishes an implemented axo runtime.

## Question

Given one Bun process hosting extensions for many sessions, what do these placements cost and how do they fail?

- **Shared:** every extension is a module in the main event loop.
- **Workers:** one `new Worker()` per extension.
- **Workers with `smol: true`:** the same, with Bun's smaller JSC heap class. `bun --smol` was also tried on the shared process.

`smol` only selects a heap-size configuration. It is neither a memory cap nor a sandbox.

Separate processes per extension were **not** measured in this original probe (see [Limits](#limits)); they were measured in the later follow-up below.

## Environment

- Linux 7.3.0-cachyos-lto x86_64 (NixOS), 12 logical CPUs, 27.9 GiB RAM.
- Bun 1.4.4, `bun --revision` → `1.4.4+38afee6c8`, from nixpkgs (`nix build nixpkgs#bun`).
- One real npm package: `yaml@2.6.1`, pinned, installed with `bun install --ignore-scripts` into a throwaway directory. That install was the only network use.
- **The host was heavily loaded by unrelated work** (1-minute load average 6.7–18.8 during the bench, median 9.1). Timing is noisy; memory figures have narrow spreads.

## Method

**One host contract for both placements.** A runner exposes `call(fn, payload, onChunk, timeoutMs)` → `ok | err | timeout`.

- Shared mode wraps each call in an async error boundary and forwards stream chunks by callback. "Unload" only drops the runner's references.
- Worker mode runs the same boundary inside the Worker, forwards chunks as messages, and correlates replies by id. Runner policy: a timed-out call is forgotten (later replies count as late), every pending call is rejected on `close`, and stop is `terminate()` then await `close`, capped at 5 s.

**Workloads**, each extension a separate copy of one module so each is its own module instance:

- `hook`: regex redaction and word count over a 500-character message.
- `file`: read a 64 KB JSONL file, parse 1000 rows, aggregate.
- `http`: fetch about 8 KiB of JSON from a local server process (not counted in memory), parse, summarize.
- `stream`: re-chunk a chunked HTTP body into 32 transformed chunks via an async generator.
- `pkg`: `yaml` parse and stringify of a 1.1 KB document.

**Bench.** n = 0, 1, 5, 10 extensions. Per run: load, GC, idle sample; 15 active rounds in which every extension receives 31 concurrent calls (20 hook, 4 file, 4 http, 1 stream, 2 pkg), so the logical work per configuration is identical; GC, retained sample; then 500 sequential hook and 100 sequential http calls to one extension after a 50-call warm-up. 14 configurations × 5 repetitions, order rotated each repetition. Attribution runs restricted the mix to one call type (n = 5, 2 runs each).

**Memory.**

- PSS from `/proc/<pid>/smaps_rollup` for the **whole process**. Linux cannot split PSS between threads of one process, so per-extension figures are marginal: (median PSS at n − baseline) / n.
- Quiescent points: median of 5 samples 40 ms apart.
- GC: `Bun.gc(true)` in the main VM and every Worker VM, twice 300 ms apart, then 500 ms (idle) or 1 s (retained) before sampling. Retained figures still include allocator pages not returned to the OS.
- Active peak: maximum of external samples every 10 ms (64–410 samples per run); shorter spikes can be missed. `VmHWM` was recorded as an upper-bound cross-check. Per-VM JS heap came from `bun:jsc` `heapStats()`.
- Aggregation: median with [min–max] over runs; five runs support nothing finer.

**Failure matrix.** Each run a fresh process in its own process group (`setsid`) with an external 12 s SIGKILL deadline. Extension 0 faulty, extension 1 healthy. The coordinator prints a heartbeat every 25 ms, read from outside; the healthy extension is probed every 100 ms with a 500 ms timeout. Worker thread exit was checked through `/proc/self/task`; residual CPU through `/proc/self/stat` ticks (about 50 ticks per 500 ms means one core still spinning). Survivors in the process group were listed after exit. 5 runs per scenario and placement, 15 for the critical Worker cases: 270 runs.

**Churn.** Load/unload cycles with GC between snapshots: 100 cycles for Workers, smol Workers and shared (a fresh module copy each cycle); 40 cycles killing a Worker mid-spin; 40 killing a Worker with 8 streaming fetches in flight; 10 cycles of resource-leaking extensions per placement. 3 repetitions each.

## Memory

Whole-process PSS in MiB, median [min–max] over 5 runs.

| Placement | n | Idle | Active peak (sampled) | Retained after GC | Δ idle / ext | Δ retained / ext |
|---|---:|---:|---:|---:|---:|---:|
| Bare Bun | 0 | 10.2 [10.1–11.0] | – | 10.5 [10.4–11.3] | – | – |
| Bare `bun --smol` | 0 | 10.2 [10.1–10.9] | – | 10.5 [10.4–11.2] | – | – |
| Shared | 1 | 14.4 [14.4–16.2] | 71.8 [71.5–79.1] | 34.3 [33.6–36.0] | 4.2 | 23.8 |
| Shared, `--smol` | 1 | 14.4 [14.3–14.5] | 70.7 [66.7–72.1] | 34.1 [33.2–35.4] | 4.2 | 23.6 |
| Workers | 1 | 17.9 [17.9–18.0] | 77.9 [75.6–80.4] | 38.9 [38.5–40.0] | 7.8 | 28.4 |
| Workers, `smol` | 1 | 17.9 [17.9–18.1] | 75.6 [72.9–77.0] | 38.8 [37.8–40.0] | 7.7 | 28.3 |
| Shared | 5 | 15.3 [15.3–15.7] | 75.9 [73.4–80.6] | 38.2 [37.1–38.4] | 1.0 | 5.6 |
| Shared, `--smol` | 5 | 15.3 [15.2–15.5] | 75.8 [73.3–76.1] | 37.7 [37.0–38.1] | 1.0 | 5.4 |
| Workers | 5 | 30.9 [30.7–31.3] | 209 [205–213] | 105 [96.0–108] | 4.1 | 18.8 |
| Workers, `smol` | 5 | 31.1 [30.9–31.6] | 219 [218–236] | 101 [94.3–104] | 4.2 | 18.1 |
| Shared | 10 | 15.5 [15.5–15.9] | 81.9 [78.6–82.2] | 40.4 [39.8–41.2] | 0.5 | 3.0 |
| Shared, `--smol` | 10 | 15.6 [15.5–15.7] | 79.9 [77.8–81.9] | 39.6 [39.3–40.7] | 0.5 | 2.9 |
| Workers | 10 | 47.2 [47.0–48.0] | 388 [381–391] | 149 [146–163] | 3.7 | 13.8 |
| Workers, `smol` | 10 | 47.3 [46.7–47.4] | 413 [410–418] | 152 [150–156] | 3.7 | 14.1 |

- **Per-Worker cost is mostly per-VM native state.** After work and GC, each Worker's JS heap was about 3.1–3.6 MiB (capacity 4.2–7.5 MiB), rising to about 22–26 MiB capacity right after the active phase. The shared main heap was 3.5–4.5 MiB for all extensions together.
- **The shared n = 1 jump is mostly one-time process cost** (HTTP client thread, JIT, the `yaml` code, allocator caches), which is why the shared per-extension delta falls as n grows.
- **`pkg` dominates growth.** Restricted to one call type at n = 5, I/O-only calls retained about 1.5–1.9 MiB per shared extension and 4.7–4.9 MiB per Worker; `pkg` retained 5.3 vs 16.9 MiB per extension, because every Worker VM compiles and JITs the package separately.
- **`smol` showed no meaningful memory benefit** for idle, retained or peak at any n; the n = 10 Worker peak was slightly higher. The relatively small measured JS heaps are a possible explanation [INFERENCE], not a demonstrated engine diagnosis; workloads with much larger live heaps were not tested.

## Timing

Representative figures, median [min–max]. All under heavy external load.

| Placement | n | Process CPU, active (ms) | Calls/s | Sequential hook p50 (µs) | Sequential http p50 (ms) | Load (ms) |
|---|---:|---:|---:|---:|---:|---:|
| Shared | 1 | 1560 [1430–1630] | 712 [484–780] | 0.4 [0.3–1.1] | 0.16 [0.15–0.19] | 23.5 [21.0–26.7] |
| Workers | 1 | 1740 [1620–1840] | 667 [451–715] | 28.8 [23.0–31.8] | 0.21 [0.21–0.23] | 30.2 [24.9–33.9] |
| Shared | 5 | 3970 [3920–4690] | 1030 [648–1046] | 0.5 [0.3–0.9] | 0.17 [0.16–0.21] | 25.7 [18.3–58.9] |
| Workers | 5 | 8540 [8090–8930] | 1283 [732–1314] | 26.8 [20.5–37.8] | 0.21 [0.16–0.26] | 32.5 [27.6–44.5] |
| Shared | 10 | 6310 [6260–7990] | 1122 [720–1149] | 0.6 [0.5–0.7] | 0.17 [0.17–0.19] | 21.2 [18.5–63.7] |
| Workers | 10 | 18320 [17520–18490] | 1282 [1147–1352] | 27.9 [25.0–53.6] | 0.22 [0.19–0.36] | 48.9 [41.9–107] |

- At n = 10 Workers used about 3× the process CPU for the same logical work, for about 14% more throughput. Part of that is per-VM JIT warm-up of `yaml` over a short run; longer runs would amortize the JIT but not the per-VM memory.
- A sequential call round trip costs about 0.5 µs in process and about 27 µs through `postMessage`. Against a local HTTP-backed call the difference was 0.17 vs 0.21 ms at p50.
- Under concurrent bursts, shared-mode hooks queued behind CPU work on the single thread (hook p50 burst completion 136 ms at n = 10), while Worker hooks ran on idle threads (5.4 ms). These are burst completion times, not request latency.
- `smol` variants were within run-to-run spread, with a small slowdown at some sizes.

## Failures and cleanup

Here “host” means the **Bun probe coordinator**, not axo's Rust host. A shared Bun stall stops all extensions in that Bun process; it does not freeze the separate Rust process. Rust must still handle unavailable extension-dependent operations explicitly.

| Scenario | Shared | Worker (and smol Worker) |
|---|---|---|
| Sync throw, async reject | `err`, extension usable (5/5) | Same (5/5) |
| Hang (never settles) | Runner timeout; can only abandon the promise | Timeout, then terminate and restart (15/15) |
| Synchronous busy loop | **Host frozen**: heartbeat gap ~11.9 s until the external SIGKILL (5/5) | Host unaffected (heartbeat gap ≤ 31 ms at a 25 ms interval); `terminate()` → `close` in 2–11 ms; thread gone |
| `process.exit` with a call pending | **Host exits** (5/5) | Worker closes with code 3; runner rejected the pending call (20/20) |
| Escaped timer throw / unhandled rejection, no handler | Bun's default exits the process with code 1 (see note) | Worker closes (timer throw code 1, rejection code 0); `error` event carries the message |
| Same, with handlers | Process-level handlers keep the host alive but cannot attribute the error to an extension | In-Worker handlers keep the Worker alive and report (5/5) |
| Late write after timeout | Both writes happen (5/5) | `terminate()` stopped the second write; the first persisted (5/5) |
| WebAssembly infinite loop | Not run | **Not terminable** (20/20): no `close` within 5 s, thread stays, one core stays busy until process exit |
| Child process, listener, timer | Unload closes nothing: child, listener and timer keep running | `terminate()` closed the listener and timer; **the child survived** |

- In every Worker failure run, including the unterminable WebAssembly loop, the healthy extension answered 100% of probes. In shared mode the healthy extension answered every probe except where the whole process froze or exited.
- **The shared-mode "survived" rows for escaped errors without handlers are a probe artifact.** Bun defers the fatal exit while a module's top-level `await` is pending, and the harness then exited with 0. Standalone checks on the same binary show the default is exit 1, immediately when no top-level await is pending. A real shared runner must install process-level `uncaughtException` and `unhandledRejection` handlers.
- **Spawned children outlive both placements and the host process.** In 25/25 resource runs and 60/60 churn-cycle children, a `Bun.spawn` child kept running after unload and after the host exited. Neither placement tracks children; a runner would have to.
- **WebAssembly is outside Worker termination.** This matches open issue [oven-sh/bun#44886](https://github.com/oven-sh/bun/issues/44886). Open issue [#44216](https://github.com/oven-sh/bun/issues/44216) (busy Workers running on after terminate) did **not** reproduce: in 20 instrumented runs the last loop progress coincided with `close`, about 2 ms after `terminate()`, with no progress afterwards.
- **Termination reliability.** About 1,060 `terminate()` calls on JS-executing Workers (190 failure runs, 870 churn cycles, including 160 busy loops and 140 races with in-flight streaming fetches) produced no crash, panic or unexpected signal; `close` fired and the thread exited every time. The [Bun Workers docs](https://bun.com/docs/runtime/workers) still call termination experimental. Closed crash reports [#38519](https://github.com/oven-sh/bun/issues/38519), [#33911](https://github.com/oven-sh/bun/issues/33911) and [#31880](https://github.com/oven-sh/bun/issues/31880) did not reproduce. Open issues [#45252](https://github.com/oven-sh/bun/issues/45252) (terminate during N-API addon load aborts the process) and [#31411](https://github.com/oven-sh/bun/issues/31411) (Worker `resourceLimits` documented but not enforced, so no per-Worker memory cap) were not tested; only their titles were read.

### Churn

| Variant | Cycles | Growth after warm-up | Threads, fds |
|---|---:|---|---|
| Worker / smol Worker | 100 | +0.02–0.03 MiB per cycle | Flat |
| Shared, fresh module copy per cycle | 100 | +0.07–0.10 MiB per cycle | Flat |
| Worker killed mid-spin or mid-fetch | 40 each | +0.05–0.06 MiB per cycle | Flat, no crash |
| Resource-leaking extension | 10 | Children 10/10 alive and outliving the host in both placements | Shared listener fds +2 per cycle; Worker fds flat |

Worker growth of 25–33 KiB per cycle may be fragmentation or caches rather than a leak; 100 cycles cannot tell them apart. Shared-mode growth is consistent with fresh module-cache retention [INFERENCE]; the experiment did not isolate that contribution from allocator retention or other causes.

## What this shows and does not show

- **Shared-mode unload was only dropping references.** That proves the runtime does no automatic cleanup of an extension's module, listeners, timers or children. It does **not** show that a shared loop cannot unload cooperatively: an explicit dispose contract, where the extension closes what it opened, was not tested.
- **Worker containment was shown for JS faults only.** A native crash or OOM inside a Worker is expected to take down the whole process, since Workers share it [INFERENCE: deliberately not tested]. Native addons, `bun:sqlite` and large payloads were not exercised either.
- **The bare Bun baseline is not a process-per-extension measurement.** About 10.2 MiB PSS idle (23.8 MiB RSS high-water) for an empty Bun process excludes loading an extension, an IPC bridge to axo, and the per-process share of real workloads. This original probe did not measure the total cost of a process topology; the follow-up uses a different workload.
- **None of these placements is a security boundary.** Workers share the address space, filesystem, environment, network and child-process table. Separate processes would still run with the same user's full authority; process isolation contains faults, not access.
- **Runner policy is not Bun behavior.** The per-call error boundary, timeouts, forgetting late replies, rejecting pending calls on `close`, restart and error handlers were all implemented by the probe. Bun itself provides `terminate()`, the `close` and `error` events, Worker-local `process.exit`, and closing a Worker's own listeners and timers on termination.

## Limits

- Timing under heavy, uncontrolled external load; memory sampled every 10 ms; retained PSS includes allocator retention; per-extension memory is marginal, not attributed.
- One Bun build on one machine; five repetitions per bench configuration.
- **Reproducibility is bounded.** The probe programs, raw JSON and temporary directory were throwaway and are not kept in this repository; the method above is the record. Rerunning means rewriting the probe from that description against a then-current Bun, and results should be expected to differ in detail.

## The probe author's recommendation

The author of the original probe favored one Worker per extension with `smol` off, under a runner adding per-call timeouts, terminate and restart, in-Worker error handlers and child-process tracking. Their reasoning: the shared loop is much cheaper in memory and CPU, but a blocking or exiting extension stops every extension sharing that Bun coordinator. They considered a process-per-extension measurement worthwhile if hard kills of WebAssembly or native code, or containment of native crashes and OOMs, became requirements. This did not establish that a separate Rust host would freeze.

This historical recommendation is **superseded** by the approved process placement below.

## Process placement follow-up (2026-11-16)

Recorded observations: Bun 1.4.4, Linux x64, three runs of a light workload, each extension loading its own copy of the `marked` package. Whole-tree PSS in MiB, idle / retained:

| Placement | 3 extensions | 10 extensions |
|---|---:|---:|
| One shared loop | 24 / 42 | 25 / 52 |
| Worker per extension | 31 / 52 | 46 / 85 |
| Process per extension | 34 / 58 | 72 / 132 |
| Two processes, split | 29 / 50 | 31 / 63 |

Process start was about 20 ms. With five separate TypeScript compiler loads, retained PSS was 157 MiB shared / 222 MiB Workers / 235 MiB processes. Dependency loading materially changes placement cost; these figures are not a universal per-extension budget.

**Evidence limits.** These recorded results were not rerun for this documentation update. Probe programs and raw samples were not retained, so reproducibility is lower than a retained benchmark. This is not apples-to-apples with the original `yaml` workload, concurrency, repetitions or sampling record. Package deduplication in a shared module graph can be real; separate package copies do not show that the original sharing was “fake.” Whole-tree PSS includes shared-page accounting and allocator retention, not exact allocation by extension. The numbers measure probes, not axo's future bridge. Re-measure on representative workloads if a phase depends on them.

Additional recorded probes found that `node:vm` timeout interrupted a synchronous loop but not a loop in a continuation after real I/O. A watchdog could detect a stalled loop but could only kill the process, not safely interrupt the offending extension alone. This agrees with the limited scope of [VM timeouts](https://nodejs.org/api/vm.html#timeout-interactions-with-asynchronous-tasks-and-promises) and Bun's [timeout support](https://github.com/oven-sh/bun/issues/10035); it is not a sandbox. Worker WebAssembly termination remains a limitation ([#44886](https://github.com/oven-sh/bun/issues/44886)); Worker `resourceLimits` are ignored ([#31411](https://github.com/oven-sh/bun/issues/31411)). These are version-sensitive observations and source evidence, not promises about future Bun releases.

### Host precedents

Editors often isolate a **shared extension host from the core**, not extensions from each other. VS Code adds user-selected [affinity groups](https://github.com/microsoft/vscode/blob/main/src/vs/workbench/services/extensions/common/extensionRunningLocationTracker.ts) and a [three-crashes-in-five-minutes tracker](https://github.com/microsoft/vscode/blob/main/src/vs/workbench/services/extensions/common/abstractExtensionService.ts). Its [CPU profiler](https://github.com/microsoft/vscode/blob/main/src/vs/workbench/contrib/extensions/electron-browser/extensionsAutoProfiler.ts) can suggest blame, but a pending RPC alone does not uniquely identify the culprit in a shared process. These mutable source references were read on 2026-11-16; they are precedents, not requirements or release qualifications for axo.

## Settled direction

Approved on 2026-11-16, not yet runtime implementation:

- TS/JS on Bun, **one supervised process per extension by default**. Users may explicitly group extensions to save memory. No Workers, author placement hints or automatic placement. Every subprocess belongs under axo's `ProcessSupervisor`; restart must also clean up spawned descendants.
- axo bundles no Bun: the host supplies the binary path. Missing Bun produces an extensions-unavailable diagnostic while the rest of axo works. Extensions are trusted user-authorized code, not sandboxed.
- Kill and restart the stuck/crashed process; **3 failures in 5 minutes** disables it with a host event naming the affected extension, or group members. Load-time failure disables only the failing extension. Grouped extensions share a failure/restart domain; a group crash does not uniquely identify a guilty member.
- Fail in-flight work visibly, with no automatic replay. If a required provider is loading, crashed or disabled, fail naming its extension: no fallback provider and no sending a request without its required rewrite.
- **No default memory cap.** Report process/tree or group memory accounting. A dedicated process gives a useful extension accounting boundary, not exact allocation attribution; RSS/PSS are not exact per-extension heap measurements. Grouped extensions have no exact individual accounting. An optional user cap (kill/restart, counting toward the failure budget) is deferred until a demonstrated need, not a new implementation commitment.

Protocol, packaging and acceptance details belong in the four phase plans summarized in [`extensions.md`](extensions.md#delivery-phases). Documentation is being aligned now, before Phase 1; Phase 1 is solely the Rust provider/login seam.
