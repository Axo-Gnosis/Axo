# pi-durable vs. what axo needs for an always-on agent in axome-server

Sources: pi v1.3.0 at `/tmp/pi-src` (paths below are relative to `packages/`), axo `plan/design.md`, Axome `.axome/plans/OVERVIEW_AGENT_PLATFORM_PLAN.md` (abbreviated **SP**). The `phase4-runtime.md` notes cover ACP adapter provisioning only (adapter versions, auth, ports). They have nothing on durability, so they are cited once at most.

---

## 1. What pi-durable is

**In one sentence:** a crash-proof agent runtime. Every model turn, tool call, queued prompt and piece of extension state is committed to storage *before* anyone sees it. When the process dies, reopening the storage carries on from the last commit (`durable/README.md:5`, spec invariants 1–3 `durable/docs/spec.md:65-67`).

It is **not** a workflow engine with deterministic replay (Temporal-style), and it is not an actor system. It has three layers:

| Layer | What it is | Code |
|---|---|---|
| **Session** | One storage plus one *mutation line*: a serial queue of atomic commits across all records. Nothing becomes visible before its commit succeeds. | `durable/src/session/*` (2.6k LOC) |
| **Records** | **Conversations** (transcript scopes, forkable), **entries** (immutable transcript records), **tasks** (durable state machines), **submissions** (awaitable inputs with an inbox), **documents** (typed JSON stored as Chord op-logs plus periodic bases) | `durable/docs/spec.md:46-61`, `:80-240` |
| **Harness** | A scheduler that runs tasks, plus built-in tasks `pi.generation`, `pi.tool` and `pi.compaction`, plus an extension registry | `durable/src/harness/*` (9.3k LOC; `scheduler.ts` alone is 2.5k) |

### 1.1 The model: tasks with checkpoints, not journal replay
- A **task** is `{kind, version, input, owner, state}`. Its state is one of `pending | running | waiting(on, policy) | sleeping(until) | completing(outcome) | terminal(outcome)` (`spec.md:1759-1773`).
- Each **phase** handler runs code, then commits the next checkpoint. *A phase reruns from its top after a crash*, so work inside a phase is at-least-once (`README.md:162`, `spec.md:2417-2436`).
- **The effect sandwich** (`spec.md:2040-2051`): commit the intent → perform the effect → commit the outcome. Reopening in an intent phase means "the effect may have happened": retry it safely, poll a handle, or record an interruption.
- **Memos** are small first-writer-wins values, committed atomically, for idempotency keys (`spec.md:2053-2056`).
- A run is not one code path. It is a chain of tasks: `submit → pi.user entry → pi.generation → pi.assistant (tool calls) → pi.tool × n → pi.tool-result × n → next pi.generation …` (`README.md:108-113`).

### 1.2 What survives a crash, and how a run resumes
- **On open**, every task still `running` is reset to `pending` with `restarts+1`. `waiting` and `sleeping` tasks stay as they are (`durable/src/harness/scheduler.ts:370-405`; `spec.md:2019-2026`).
- `harness.resume()` (or the first submit or wait) starts the scheduler again (`harness.ts:292-295`, `README.md:124-127`).
- **Generation** (`spec.md:4135-4307`):
  - The `request` checkpoint pins the model, the thinking level, the stream options and a `cutoff` entry.
  - Partial assistant output is committed to the `pi.live` document at most every 100 ms (`spec.md:4226-4231`, `README.md:502`).
  - On recovery, the leftover partial becomes an *aborted* `pi.assistant` entry, so its spend and text are kept (`generation.ts:434`). The *same* committed messages are then sent again (`spec.md:4233-4235`).
  - Retry backoff and deferred-response polling are durable sleeps (`spec.md:4263-4277`).
- **Tools** (`spec.md:4309-4367`; `durable/src/harness/tool.ts:175`, `:185-197`):
  - `call` validates the call, runs the `beforeTool` hooks, then commits intent `{phase:"execute", arguments, replay}`, then executes.
  - When recovery finds the `execute` phase, the tool runs again only if both the stored policy and the current one are `safe`.
  - Otherwise the model gets an `interrupted` error result built from the durably committed partial output: *"Tool X was interrupted and may have partially run"*.
- **Crash loops:** after `settings.restartLimit` consecutive restarts without progress (default 5), a task is *parked* (`restart_limit`) until `retryTask()` (`scheduler.ts:1365-1367`, `spec.md:2189-2209`).
- **Graceful close is not a crash.** Running tasks go back to `pending` without a restart count and resume on reopen (`spec.md:2027-2038`).
- **Storage error:** the Harness fails closed and must be reopened. Commits are atomic, so the store stays consistent (`README.md:823`).

### 1.3 Scheduling and wakeups
- Durable `sleeping` state, with `runtime.sleepUntil()` (`spec.md:2404-2436`).
- In Node, one timer covers the earliest sleep. For evictable hosts (Cloudflare Durable Objects), the `onWake` callback gives `{active, at}` so the host can arm its own alarm and call `resume()` (`spec.md:2453-2479`, `README.md:166-210`).
- There is **no cron** or general trigger system. Schedules are the host's job.

### 1.4 Idempotency and replay policy
- **Submissions:**
  - `requestId` dedupes: a retried submit returns the existing submission (`durable/src/harness/submissions.ts:210-217`, `README.md:129-138`).
  - An inbox with `steer` / `follow-up` / `reject` / `write` placement is persisted as the `pi.inbox` document (`README.md:504-521`).
- **Tools:** `ToolRegistration.replay?: "safe" | "unsafe"`, default `unsafe` (`durable/src/harness/types.ts:304-305`). `safe` means rerunning after a crash is harmless. Two examples:
  - The subagent tool is `safe` because a rerun finds its child conversation through an ownership index and reuses the same `requestId` (`README.md:650-665`).
  - `codemode` is `safe` through deterministic script replay from a journal of nested results (`README.md:368`).
- **Nested tool calls** reattach by key after a rerun (`README.md:328`). Unsafe callers' nested work is marked `abandonOnRestart` (`README.md:351-353`).
- **`AgentTool.replay: "never" | "safe"`** in pi-agent-core (`agent/src/types.ts:489-490`) is **vestigial**:
  - Nothing in `agent/src` or `coding-agent/src` reads it (grep for `.replay` / `replay:` finds nothing).
  - It is left over from the "durable runtime" that agent-core removed in 1.0.0 (`agent/CHANGELOG.md:35`).
  - Note that its vocabulary (`never`) differs from pi-durable's (`unsafe`).

### 1.5 Multiple processes and ownership
- **Process ownership: none.** *"One process owns a storage at a time; there is no cross-process locking"* (`README.md:790`). The IndexedDB docs ask the host to hold a Web Lock (`README.md:769-771`). There are no leases or fencing.
- **"Ownership" in pi-durable means structured concurrency** (`spec.md:2306-2402`):
  - Tasks and conversations form an ownership tree.
  - Aborts cascade down and run bottom-up.
  - A task finishing while it still owns live work is held as `completing`.
  - `background: true` makes a task a boundary that an Esc or an idle wait does not cross.
  - Background subagents are built from that (`durable/test/examples/23-subagent-background.ts:1-72`): an anchor background task owns each child conversation, and reporter tasks post answers back with `requestId`s, so "a restart [never sends] a message or a report twice" (`README.md:681`).

### 1.6 Observation (clients attaching)
- `viewState()` / `watch()` expose committed state only: the transcript plus the `pi.live`, `pi.inbox`, `pi.usage`, … documents, as Chord ops (`spec.md:4770-4819`).
- **Late joiners and reconnecting clients get the current view. "Nothing is replayed"** (`README.md:500`).
- A slow watcher keeps at most 100 pending frames, then gets one whole-value replacement (`durable/src/session/observation.ts:16`, `:229-232`).
- `watchEvents()` (experimental) produces coding-agent-style events derived from commits, and falls back to a `snapshot` event after 100 batches of lag (`README.md:590-604`).

### 1.7 Storage backends (`README.md:759-790`, `spec.md:5379-5458`)
- **Memory:** the reference semantics.
- **IndexedDB.**
- **SQLite:** WAL with `synchronous=NORMAL`, one SQL transaction per commit.
- **JSONL:**
  - A `main.jsonl` with one marker per commit, plus `doc-<id>` and `task-<id>` sidecars.
  - A commit counts only once its marker is written.
  - Optional `fsync`.
  - It keeps the whole history in memory, and the README says *"use SQLite for long-running agents"*.
- **Cloudflare Durable Object SQLite.**
- The `Storage` trait is a batch `commit(writes) -> Seq` plus cursor scans (`durable/src/types.ts:1179-1183`, `spec.md:5083-5133`).

### 1.8 Dependencies on chord and env
- **chord** supplies:
  - `Context` (cancellation plus a telemetry span on every async call);
  - Delta (immutable JSON op transactions and trackers for documents);
  - replicated `State` and watches;
  - `OutputBuffer` / `Progress` (bounded tool output and paced commits);
  - SQLite facades.

  Import counts in `durable/src`: chord ×44, `/delta` ×13, `/context` ×12, `/sqlite*` ×12, `/output` ×3.
- **pi-env** depends *on* durable (it implements `ExecutionEnv` remotely), not the reverse (`env/package.json` deps).

---

## 2. Status and maturity; does pi's coding agent use it?

- **Experimental:** *"The API changes without notice between releases"* (`README.md:3`).
- **Very new and churning:**
  - 1.0.0 shipped on 2026-10-01 (`durable/CHANGELOG.md:181`).
  - The changelog lists **Breaking Changes** in 1.0.3, 1.0.4, 1.1.0 and 1.2.0, and again in Unreleased (`CHANGELOG.md:3-145`).
  - The clone has 180 commits touching `packages/durable` between 2026-09-18 and 2026-10-21.
- **Large:**
  - About 23.5k LOC in `src`: harness 9.3k, session 2.6k, storage 4.2k, env 2.3k, tools 1.6k, codemode 1.7k, testing 3.0k.
  - Plus 41.6k LOC of tests, a normative spec of 5.6k lines, and chord at 14.8k LOC.
  - It is thoroughly tested: crash and restart tests exist for tools, nested tools, tasks, generation and storage failure.
- **pi's coding agent does NOT use it.**
  - `coding-agent/package.json:44` depends on `pi-agent-core`, and there is no `pi-durable` or chord import anywhere in `coding-agent/src`.
  - The coding agent still runs on `AgentSession` with JSONL `SessionManager`. That manager has no fsync, no lock and no recovery of runs. Messages are persisted only on `message_end`, never as partials, and an interrupted run is simply gone; orphaned tool calls get "No result provided" when the next request is built (`axo/plan/pi/session.md:170-181`, `axo/plan/pi/loop.md:162`).
  - pi-durable is a *parallel product*. It ships its own ports of `read`/`write`/`edit`/`bash`, `tool_search` and codemode, with examples for print/JSON/chat modes (`README.md:845-864`). It targets embedders such as Cloudflare Workers.
- **For axo:** the *product* axo ports (`coding-agent`) is not durable. pi-durable is a reference design to borrow ideas from, not a dependency.

---

## 3. What an always-on agent in axome-server needs

Axome context:
- Phase 6 wants "server-owned chat sessions [that] survive client disconnects (agent processes stay up). Desktop and browser clients attach and detach" (SP:411).
- Its acceptance test: close the desktop → a webhook starts a run → a permission prompt fires a notifier → reopen and approve → the run settles (SP:419-423).
- Triggers and schedules live in an `axome-core` Host service (SP:395). The plan also requires **one writer per session** (SP:73).

**What changes with axo.** The plan assumes harnesses are external ACP processes and that "Axome never owns harness state" (SP:67). Embedding axo makes the *axome-server process itself* the agent. A systemd restart, an upgrade or a crash therefore kills every live run unless axo recovers on open. If axo instead runs as the `axo` ACP binary under the agent pool, the same holds for that process.

| Need | pi-durable gives | axo `core.md` already gives | Gap for axo |
|---|---|---|---|
| **Survive a server restart mid-run** | Yes, fully: task checkpoints, generation re-request, tool replay policy, restart limit | A finished message is appended on `MessageEnd` before the next request (`core.md:138`). The projection is the source of truth (`:165`). Torn-line repair (`:180`). | No record that a run *was in progress*, so there is nothing to resume. Dangling tool calls get synthetic results at best. No restart-loop guard. |
| **Detached runs (no client)** | Runs are tasks owned by the Harness, independent of any watcher | `Session` is headless and `prompt()` is a library call (`core.md:250-257`) | Run lifetime must not depend on subscribers (see the next row). The host needs a registry of live sessions (Axome's side). |
| **Many clients attach, detach and catch up** | Current view (committed state, including the 100 ms partial and tool output) plus ops. No replay; a 100-frame lag cap, then a snapshot. | One typed event stream. **Subscribers are awaited in order for back-pressure** (`core.md:138`). | A slow or remote client would stall the loop. There is no cursor on entries and no in-memory "live state" snapshot to hand a late joiner. |
| **Scheduled and triggered prompts** | Durable sleeps and `onWake`. Inbox plus `requestId` dedupe. | Steer and follow-up queues in memory (`core.md:146-148`) | Queued prompts are lost on restart, and nothing prevents a duplicate trigger (one fired before *and* after a restart). Cron belongs to Axome (SP:385-395), not axo. |
| **Background subagents** | Ownership tree, background boundary, abort cascade, reporter tasks with idempotent delivery | Child sessions are a stated goal (`design.md:20`, `:49`) | Child↔parent links must be persisted, spawning must be idempotent per tool call, and the result must be delivered back once. |
| **Crash-safe tool execution** | Intent committed before execute; `safe` reruns; `interrupted` result with the output so far | Decision 3 records "aborted" results for interrupted calls (`core.md:288-289`) | No per-tool replay policy. Shutdown and user-abort are not distinguished. Bash children may outlive a crashed server [INFERENCE]. |
| **Approvals while unattended** (SP:412-415) | (none) | `before_tool_call` hook may block (`design.md:146`) | Waiting for approval *before* execute is naturally replay-safe, because nothing ran. On restart the call is simply asked again. |
| **One writer per session** | None (`README.md:790`) | (none) | Two processes (axome-server plus an `axo` ACP binary, or a second server instance) appending to one JSONL corrupt the tree. |
| **Leaf survives restart / multiple clients** | (n/a: linear transcript with forks) | Decision 4 keeps pi's rule: the leaf is the last line, so a navigation that appends nothing is lost on restart (`core.md:290-291`) | A server with several clients needs a persisted leaf. |

**Main conclusion.** axo already has the hard part pi-durable builds generically: an append-only transcript that is the source of truth, with the finished assistant message (which carries its tool calls) persisted *before* those tools run. That message **is** the durable intent. What axo lacks is small:
- a **run marker** that says "unfinished";
- a **tool replay policy**;
- a **recovery step on open**;
- a **persisted inbox** with request ids;
- an **entry cursor plus a live snapshot** for attaching;
- a **file lock**.

None of that needs pi-durable's task, document, Chord or scheduler machinery.

---

## 4. Proposal for axo

### 4.1 Do not port pi-durable's runtime; port four ideas
| Port (cheap, high value) | Skip, and why |
|---|---|
| The effect-sandwich *rule* for tools: intent persisted, then effect, then result, plus `Replay::{Never, Safe}` | General task state machines, phases, memos, migrations, handover on registry reload. These exist so *extensions* can define durable workflows; axo has one loop. |
| `requestId` idempotent submissions | Chord documents and op-logs, rewindable documents, bases and checkpoints. axo's entries plus projection already are its state. |
| Restart-limit parking of crash loops | Committing streaming partials every 100 ms. The write amplification only pays off on evictable hosts (Durable Objects). An axome-server is a long-lived process where crashes are rare; losing one in-flight response and sending it again is acceptable. |
| "Graceful close ≠ crash": shutdown leaves runs resumable | Structured-concurrency tree, `completing` holds, wait-for cycle detection, `onWake`, multi-backend storage (IndexedDB, Durable Objects) |

### 4.2 Build in M1–M4 (so nothing is retrofitted)

**M2 (`axo-agent`): seams in the loop**
1. **Tool replay policy** on the trait. The default is the safe choice for side effects:
   ```rust
   enum Replay { Never, Safe }               // pi-durable "unsafe" ≙ Never
   trait Tool { fn replay(&self) -> Replay { Replay::Never } … }
   ```
   - `Safe`: `read`, `grep`, `find`, and `write` (same content, same result).
   - `Never`: `bash` and `edit`. A hashline edit applied twice fails on hash mismatch, but the message would mislead the model, so prefer the explicit `interrupted` result.
2. **Cancellation with a reason.** `tokio_util::CancellationToken` carries no reason, so wrap it as `Cancel { token, reason: Arc<OnceLock<CancelReason>> }` with `enum CancelReason { User, Shutdown }`.
   - On `Shutdown`, the loop **must not** persist aborted assistant messages or the "aborted" tool results from decision 3. The run stays open and resumes after restart, mirroring pi's close semantics (`spec.md:2027-2038`).
   - On `User`, decision 3 applies as written.
3. **A resume entry point.** `Agent::continue_run(transcript)` starts a run from an existing tail: user message, tool results, or a steer. pi has `continue()` (`axo/plan/pi/loop.md:174`); keep it as a first-class API, not a test hook.
4. **Split event delivery.**
   - Keep the *ordered, awaited* subscriber for **persistence only**. It must finish before the next request (`core.md:138`).
   - Host and client fan-out goes through a bounded `tokio::sync::broadcast`. A lagging receiver gets `Lagged`, then resyncs from a snapshot (pi's 100-frame rule, `observation.ts:16`).
   - This removes the "a slow client stalls the agent" hazard before Axome ever exists.

**M3: tools**
5. **bash child lifetime.**
   - Set `PR_SET_PDEATHSIG` (Linux) on the process-group leader. Make the spool directory durable (under the session dir, not `/tmp`), so an `interrupted` result can point at the full output collected before the crash.
   - [INFERENCE] Without this, an orphaned `bash` can keep mutating the repository while a resumed run proceeds.

**M4 (`axo` sessions): state as entries, recovery on open**
6. **New entry types.** All are excluded from the model context, like `custom`:
   ```rust
   RunStart   { run_id, submissions: Vec<SubmissionId> }        // appended before the first request
   RunEnd     { run_id, outcome: Done | Aborted | Failed{error} | Interrupted{reason} }
   RunResumed { run_id }                                       // one per recovery; counts restarts
   Submission { id, request_id: Option<String>, mode: Prompt|Steer|FollowUp, content }  // durable inbox
   SubmissionPlaced { id, entry_id }   // when it became the user message
   SubmissionSettled { id, outcome }   // withdrawn / unanswered
   Leaf { entry_id }                   // persist navigation; this changes decision 4
   ```
   Cost: a few short lines per run, against pi-durable's task-record rewrite per phase.
7. **The `request_id` index** is built on load (`HashMap<String, SubmissionId>`). `session.submit(Submission { request_id, … })` returns the existing id when the request id is a duplicate. Axome triggers pass `request_id = "<task>:<event-id>"`; subagent reporters pass `"<child-run>:report"`.
8. **`recover()` runs on open** when the last `RunStart` has no `RunEnd`:
   1. Count consecutive `RunResumed` entries with no new message entry in between. At or above `restart_limit` (default 5), append `RunEnd{Interrupted{reason:"restart_limit"}}`, report it and stop. The host can offer "retry" (pi: `retryTask`, `spec.md:2204`).
   2. For each tool call in the last assistant message without a result:
      - `Safe` → execute it again.
      - `Never` → append the error result *"interrupted by a restart before its result was recorded; it may have partially run"*, plus the spool path if any.

      This takes no `ToolStarted` entry. Calls that never started get the same wording, which is harmless; the model re-issues them. ponytail: add `ToolStarted{call_id, spool}` only if the wording proves confusing.
   3. Re-queue unsettled `Submission`s.
   4. Append `RunResumed` and call `continue_run`. A torn assistant message never reached the file (it is only persisted on `MessageEnd`), so the request is simply sent again.

   The *host* picks the policy, `RecoverPolicy::{Resume, MarkInterrupted}`:
   - axome-server uses `Resume`;
   - the interactive ACP binary probably uses `MarkInterrupted`, appending `RunEnd{Interrupted}` and letting the user decide.
9. **Single writer.**
   - `Session::open` takes `File::try_lock()` (std, stable since Rust 1.89: https://releases.rs/docs/1.89.0/) on the session file, or on a sidecar `.lock` file if `SessionStore` is not file-backed.
   - Read-only opens (listing, ACP `session/list`) take no lock or a shared one.
   - An flock is released when the process dies, which gives exactly the crash semantics needed with no lease expiry.
   - A real lease with heartbeat and fencing token is only needed if storage is ever shared across hosts: defer it.
   - Expose the error as `SessionBusy { holder_pid }`. This enforces Axome's "one writer per session" (SP:73) at the storage level.
10. **Event cursor.**
    - Each entry gets `seq: u64`, its ordinal position in the append-only file. Derive it on load instead of serializing it.
    - `SessionStore::append` returns it. Forks copy into new files, so their `seq`s restart from 0, which is fine because the cursor is per session.
11. **LiveState snapshot** (in memory, never persisted):
    ```rust
    struct LiveState { run: Option<RunId>, partial: Option<AssistantMessage> /* from Accumulator */,
                       tools: Vec<ToolSlot{call_id, name, status, output_tail, details}>,
                       queue: Vec<SubmissionId>, compaction: Option<…>, retry: Option<{at, error}> }
    fn attach(&self, from: Option<Seq>) -> Attach { entries_since: Vec<Entry>, live: LiveState, rx: broadcast::Receiver<SessionEvent> }
    ```
    - This is pi-durable's `pi.live` (`spec.md:3997-4062`), kept in memory instead of committed.
    - Because `seq` comes from durable entries and deltas are ephemeral, a reconnect replays *only the durable entries it missed*, then the current partial. That is more than pi-durable offers (it never replays), at almost no cost.
    - It maps directly onto ACP v2 `session/resume` with `replayFrom` (SP:219). ACP v1 `session/load` is `from: None`.

**M5:** compaction and retry need no extra durability work.
- A compaction entry is one atomic line; a crash mid-summary just reruns it.
- Retry backoff on recovery is "retry now", with `RunResumed` counting toward the limit.

### 4.3 Seams to leave (no code now)
- **A `SessionHost` in `axo` or in Axome.** It owns open sessions by id, calls `recover()` for each one with an open run at startup, and exposes `attach`/`detach`. It is about 300–500 LOC and best written when Axome Phase 6 starts. Until then, the Session-level API above is sufficient.
- **Child-session links.** Add `ChildSession{session_id, parent_session, parent_call_id, background: bool}` when subagents land.
  - **Foreground subagent:** the tool is `Safe` because spawning is idempotent. Look up the child by `(parent_session, call_id)` and reuse `request_id = call_id` (pi's pattern, `README.md:650-665`).
  - **Background subagent:** the result is delivered as a parent `Submission{mode: FollowUp, request_id: "<child-run>:report"}`.
  - Each child recovers through its own `RunStart` / `RunEnd`; no ownership tree is needed. The one rule to keep from pi: aborting the parent's call aborts a foreground child, while background children survive a parent's Esc (`README.md:674-676`).
- **Durable sleep and wake:** defer. Schedules belong to Axome's trigger service (SP:385-395). axo's only in-run waits (retry backoff) restart safely.
- **`ToolStarted` entry and the persisted partial-output tail:** add them only if interrupted results prove too vague.
- **`SessionStore` beyond JSONL** (for example SQLite, if many sessions per server makes per-file JSONL listing slow): the trait already exists (`core.md:180`).

### 4.4 Defer or never
- Defer: generic durable tasks for extensions; partial commits every 100 ms; multi-host leases.
- Never: Chord-style op-log documents; CRDT or multi-writer support (pi-durable lists it as a non-goal too, `spec.md:5614`); deterministic codemode replay.

### 4.5 Added complexity (estimate [INFERENCE])
| Item | Milestone | LOC (Rust) |
|---|---|---|
| `Replay`, `CancelReason`, `continue_run`, broadcast fan-out split | M2 | 150–250 |
| bash PDEATHSIG plus durable spool dir | M3 | 30–60 |
| Run, submission and leaf entries; request-id index; `recover()`; restart count; flock; `seq`; `LiveState` plus `attach` | M4 | 500–800 |
| Regression tests: (a) kill mid-`bash` → `interrupted` result, then the run completes; (b) duplicate `request_id` → one run; (c) restart loop → parked after N; (d) attach from a cursor gets exactly the missed entries | M4 | 4 tests |
| `SessionHost` (later, with Axome Phase 6) | — | 300–500 |
| **Total** | | **~1.0–1.6k**, versus about 16k LOC of pi-durable core (harness, session, storage) plus 14.8k of chord |

---

## Recommendation for axo

1. **Do not depend on or port pi-durable.**
   - It is explicitly experimental, breaks in almost every release, is about 16k LOC of core plus chord, and pi's own coding agent does not use it.
   - Its generality (extension-defined durable tasks, evictable-host wakeups, Chord documents, committed partials) solves problems a long-lived axome-server does not have.
   - That fits Toph's "own the core" preference (`design.md:15`).
2. **Adopt its rules on top of axo's transcript-as-truth design.** The persisted assistant message is the tool intent. Then:
   - a `RunStart`/`RunEnd` marker makes a run resumable;
   - `Replay::{Never, Safe}` (default `Never`) decides between rerunning and an `interrupted` result;
   - `request_id` dedupes submissions;
   - restart-limit parking stops crash loops;
   - shutdown is distinguished from user abort, so a graceful restart resumes instead of aborting.
3. **Make these M2 seams now; they are what gets expensive to retrofit:**
   - `Tool::replay()`;
   - `CancelReason`;
   - `continue_run`;
   - persistence-only ordered subscriber plus lossy broadcast fan-out.
4. **Put in M4:**
   - run, submission and leaf entries;
   - `recover(RecoverPolicy)` on open;
   - an `flock` single-writer guard;
   - an entry `seq` cursor;
   - an in-memory `LiveState` with `attach(from_seq)`.

   Together these cover all six of Axome's always-on needs at the library level. They also make ACP `session/load` (v1) and `session/resume{replayFrom}` (v2) trivial.
5. **Revise `core.md`:**
   - decision 3 applies only to user aborts;
   - decision 4 (leaf persistence) should flip to a persisted `Leaf` entry, because a multi-client server needs it;
   - add "durability" to the M4 "done when" line: *kill -9 mid-tool, reopen, the run completes, with an `interrupted` result for bash.*
6. **Tell Axome.** An embedded axo makes axome-server itself the harness process. That changes SP:67's "Axome never owns harness state" in spirit: the state is axo's, but it lives in the server's data dir. Phase 6 then needs a `SessionHost` that resumes open runs at startup, rather than "agent processes stay up".
