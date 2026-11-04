# Sessions, Durability & Compaction

`axo` persists every session as an append-only JSONL entry tree (`crates/axo/src/store/` and `crates/axo/src/session.rs`), journals background jobs into the same file (`crates/axo/src/jobs.rs`), and maintains context size through recoverable artifact elision and model summaries (`crates/axo/src/compaction/`).

## Session Storage (`JsonlStore` & `SessionStore`)

By default, sessions are stored under `<AXO_DIR>/sessions/--<encoded-cwd>--/<timestamp>_<uuidv7>.jsonl`, where `<encoded-cwd>` is the absolute cwd without its leading separator, with `/`, `\`, and `:` replaced by `-`, and the timestamp replaces `:` and `.` with `-`.

- **Lazy file creation:** `Axo::create_session` reserves a planned path and `Header` in memory. The first submission or non-system message creates the file and parent directory, flushing earlier buffered metadata. A built-in command that only changes model or thinking settings does not by itself create a new session file.
- **Single-writer lock:** Opening an existing session for writing (`JsonlStore::open_write`) acquires an exclusive non-blocking OS file lock (`std::fs::File::try_lock`) on the session file itself. The lock is explicitly released when the last local writer/write-operation handle drops, so briefly inherited subprocess descriptors cannot prolong it after writer closure. A second writer receives `Error::SessionBusy`. Read-only opens (`Axo::open_session_read`, `Axo::list_sessions`, and `Axo::schemes`, which reads only the header) take no lock.
- **Torn-line repair:** If the final line lacks a trailing newline, `open_write` appends the newline when the line parses as JSON, and otherwise truncates the torn line back to the previous complete line.
- **Sequence numbers (`SeqEntry`):** Every entry loaded or appended receives a 0-based monotonic `seq: u64` corresponding to its line index after the `Header`.
- **Synced records:** `JsonlWriter::append` flushes every line and additionally calls `sync_data` for `run_start`, `run_end`, `job_start`, `job_end`, and any `message` carrying a non-empty `admittedJobs` or `deliveredJobs` receipt, so job state survives a host crash.

### JSONL format (`FORMAT_VERSION = 1`)

Line 1 is the `Header` (`{"type":"session","version":1,"id":"…","timestamp":"…","cwd":"…","parentSession":…,"forkedFrom":…}`). Subsequent lines are `Entry` variants sharing `EntryBase { id, parent_id, timestamp }`:

| `type` | Variant | Role in projection |
| :--- | :--- | :--- |
| `message` | `Entry::Message { base, message, submission_id, admitted_jobs, delivered_jobs }` | Appends `AgentMessage` (`System`, `User`, `Assistant`, `ToolResult`, or `Custom`). `admittedJobs` / `deliveredJobs` (job ids `bg_N`, omitted when empty) are [background job receipts](#background-job-durability) and never enter model context |
| `thinking_level_change` | `Entry::ThinkingLevelChange { base, thinking_level }` | Updates `Projection::thinking` |
| `model_change` | `Entry::ModelChange { base, provider, model_id }` | Updates `Projection::model` (`"{provider}/{model_id}"`) |
| `usage` | `Entry::Usage { base, kind, provider, model, usage, note }` | Recorded for accounting (such as summary requests); never enters model context |
| `compaction` | `Entry::Compaction { base, summary, first_kept_entry_id, preserved_user_ids, tokens_before, details, usage, from_hook, system_message }` | Replaces messages before `first_kept_entry_id` with the checkpoint `system_message`, preserved user messages, and a `compactionSummary` custom message wrapping the summary in `<summary>…</summary>` (lowered to a user message) |
| `branch_summary` | `Entry::BranchSummary { base, from_id, summary, details, usage, from_hook }` | Injects a `branchSummary` custom message wrapping the summary in `<summary>…</summary>` (lowered to a user message) |
| `custom` | `Entry::Custom { base, custom_type, data }` | Host metadata; never enters model context |
| `custom_message` | `Entry::CustomMessage { base, custom_type, content, details, display }` | Lowered to a user message in model requests |
| `context_edit` | `Entry::ContextEdit { base, target_id, replacement }` | Replaces `target_id`'s content (`Some(ContextReplacement)`) or hides `target_id` from the projection (`None`) |
| `label` | `Entry::Label { base, target_id, label }` | Parsed and preserved (pi compatibility); not projected and never enters model context |
| `session_info` | `Entry::SessionInfo { base, name }` | Parsed and preserved (pi compatibility); not projected and never enters model context |
| `leaf` | `Entry::Leaf { base }` | Moves the active branch pointer to `base.parent_id` (`None` = before the first entry) |
| `run_start` | `Entry::RunStart { base, run_id }` | Marks the start of an agent run |
| `run_end` | `Entry::RunEnd { base, run_id, outcome, reason }` | Marks a settled run (`done`, `aborted`, `failed`, `interrupted`; a parked run is `interrupted` with `reason: "restart_limit"`) |
| `run_resumed` | `Entry::RunResumed { base, run_id, attempt }` | Records a crash recovery resume attempt |
| `submission` | `Entry::Submission { base, submission_id, kind, parts, request_id }` | Durable `prompt`, `steer`, or `follow_up` input recorded before queueing. `parts` holds the prepared [prompt parts](prompts-and-context.md#prompt-parts-cratesaxosrcinputrs) as sent (`Saved` instead of `Content`, skill commands unexpanded); the model-facing text lives on the user `message` entry. Older files wrote `content` (text blocks); it loads through `#[serde(alias = "content")]` as `Text` parts. Built-in commands write no submission |
| `job_start` | `Entry::JobStart { base, session_id, job_id, call_id, command, artifact }` | Written when a `bash` call is promoted to background job `job_id` (`bg_N`), before the promoted tool result, and also when a foreground command exits leaving helpers running in its container (such a record is never admitted; it only reserves the id). `session_id` is the session that started it (copied records in a fork keep the parent's id); `artifact` is the job's output spool `artifact://<artifact>`. Never enters model context; foreground commands whose container is empty at exit write no job records |
| `job_end` | `Entry::JobEnd { base, job_id, text, is_error }` | The promoted job's rendered result, or the interrupted result written on reopen. Never enters model context; the model sees the result only through the message that delivers it |
| *(unknown)* | `Entry::Unknown(Value)` | Preserved verbatim on load; ignored by projection |

## Projection (`crates/axo/src/store/project.rs`)

`project(entries, leaf)` walks the ancestor chain from the current leaf (`None` uses the last entry in the file; an `Entry::Leaf` moves the leaf to its `parent_id`) back to the root:

1. **Latest compaction boundary:** If the root-to-leaf path contains one or more `Entry::Compaction` entries, projection starts from the last compaction's `system_message` snapshot and `compactionSummary` custom message, emits its `preserved_user_ids` in path order, and then replays entries from `first_kept_entry_id` onward.
2. **Branch summaries:** Any `Entry::BranchSummary` on the active segment becomes a `branchSummary` custom message (lowered to a user message).
3. **`ContextEdit` application:** On the active path, the last `Entry::ContextEdit` targeting an entry ID wins. `replacement: None` removes that message from the projection (used to hide transient-error or overflow turns); `Some(ContextReplacement)` replaces its content blocks (used by `/shake` and automatic clearing) and zeroes provider token usage on assistant messages positioned before the last `context_edit` or `compaction` on the path.
4. **Incremental caching (`Projection::apply`):** `Session` maintains the projection incrementally as entries are appended, rebuilding from scratch only when navigating or when an edit/compaction invalidates earlier entries.

## Crash Recovery (`crates/axo/src/recovery.rs`)

When `Axo::open_session` opens a session file, `plan_recovery` inspects the log after the last `Entry::RunEnd`:

- **Unfinished tool calls:** If the last assistant message on the path has tool calls without matching `ToolResult` entries:
  - Tools with `Replay::Never` (`bash`, `edit`, and `write` to a scheme whose `write_replay` is `Never`) immediately receive a persisted synthetic `ToolResult` with `is_error: true` explaining that the process stopped before completion (and pointing to any partial `bash` output as `artifact://<n>`, backed by `<session>/artifacts/<n>.bash.<escaped-call-id>.log`).
  - Under `RecoveryPolicy::Resume` (default), tools with `Replay::Safe` (`read`, `grep`, `find`, `ask`, `wait`, and `write` to a plain path or `local://`) are left unanswered so `Agent::continue_run` re-executes them in the background.
  - Under `RecoveryPolicy::MarkInterrupted`, `Replay::Safe` calls also receive interrupted results, unconsumed submissions are not requeued, and `Entry::RunEnd { outcome: Interrupted }` is appended immediately.
- **Interrupted or failed assistant responses:**
  - If the last assistant response stopped with `StopReason::Length` and had tool calls, every unanswered call receives a truncated-output error result and the run continues.
  - If the last assistant response had a context-overflow signal on a `Stop` reply without tool calls, recovery keeps the reply and closes the run `done` if its maintenance checkpoint already committed (otherwise re-running maintenance). A retryable provider error or overflow with a failed turn hides the failed assistant turn with `Entry::ContextEdit { replacement: None }` and resumes the run.
  - A non-retryable failed or aborted response with unanswered tool calls gets not-executed (or `Operation aborted`) results before the run closes `Failed`/`Aborted`.
- **Unconsumed submissions:** Any `Entry::Submission` saved after the last `RunEnd` (or during an open run) whose `submission_id` has not yet been consumed by an `Entry::Message` is requeued in order (`Steer`, `FollowUp`, or starting a new `Prompt` run). Requeued messages are rendered again from the stored `parts`: a skill command is expanded from the current `SKILL.md`, and a saved image is re-read from `local://`.
- **Restart loop cap (`RESTART_LIMIT = 5`):** If a run accumulates 5 `Entry::RunResumed` markers in a row without an intervening `Entry::Message`, recovery appends `Entry::RunEnd { outcome: Interrupted, reason: "restart_limit" }` and stops resuming it.
- **Interrupted background jobs:** Before recovery, every job of this session that was admitted but has no `job_end` receives one (`is_error: true`): `Command was interrupted when the session closed; it may have partially run. Output so far: artifact://<n>` (or `… Its output is unavailable.` when the spool file is gone). The result becomes pending delivery; see [Background Job Durability](#background-job-durability).

## Attaching, Navigating & Forking (`Session`)

- **`session.attach(from_seq)`:** Atomically snapshots `Attach { missed, live, events }`:
  - `missed`: all `SeqEntry` items with `seq > from_seq` (or all entries when `from_seq` is `None`).
  - `live`: `LiveSnapshot` containing `run_id`, active `maintenance` (`Compact`, `Shake`, `BranchSummary`), `agent` (`AgentSnapshot` with `running`, partial `streaming` message from `Accumulator`, `pending_tool_calls`, `queued_steering`, `queued_follow_up`), projected `messages`, and `pending_questions`.
  - `events`: a `tokio::sync::broadcast::Receiver<SessionEvent>`. If a slow receiver lags (`RecvError::Lagged`), it can call `session.attach(Some(last_seen_seq))` to catch up without stalling the agent loop.
- **`session.navigate(target, NavigateOptions { summarize })`:**
  - Appends `Entry::Leaf { base: EntryBase { parent_id: target, .. } }`, records `ModelChange` / `ThinkingLevelChange` if the live selection differs from the target branch's historical selection, restores the file ledger and nested context tracker to the new projection, and updates the agent's transcript.
  - Running background jobs keep running. Their results are delivered on whichever branch is selected when they finish; the delivery names the originating command (`[bg_N] <command>`), so it reads correctly away from the branch that started it.
  - With `summarize: true`, first summarizes the abandoned branch with a separate tool-free request (`BRANCH_SUMMARY.md`) and appends `Entry::BranchSummary` at `target` in place of the `leaf` entry.
- **`session.fork(entry_id)`:**
  - Copies the root-to-`entry_id` ancestor chain into a new session file whose `Header` records `parent_session` and `forked_from`.
  - Copies the parent's side files into the fork's session directory: `artifacts/` (hard-linked read-only, copy fallback) and `local/` (copied), so `artifact://` and `local://` URLs, including saved prompt content, resolve in the fork. The output spool of a job that is still running (or retaining helpers) is never hard-linked: the fork gets a read-only snapshot copy, so later output appended by the parent's job does not show up in the fork. Copying is best effort; an entry that cannot be carried over is logged and skipped.
  - The fork gets its own empty job registry: no running job handles carry over, and copied `job_start` records of the parent's session are ignored (their `session_id` differs). New jobs in the fork number from one past the highest `bg_N` any raw record or receipt names (see Raw reconstruction below).
  - If `entry_id` is a user message, `fork` copies up to that user message's parent and returns `Fork::editable: Option<Vec<PromptPart>>` so a client can prefill its editor, chips included: the parts of the `submission` that produced the message, or, for a message without one (older sessions), one `Text` part joining its text blocks. The returned parts can be resubmitted as-is (`Saved` parts resolve to the copied `local://` files). Any open run on the copied path is closed with `RunEnd { outcome: Interrupted, reason: Some("fork") }` (a run whose last response already settled it is closed with that outcome instead).

## Background Job Durability

A `bash` call promoted to the background (see [Background Jobs](prompts-and-context.md#background-jobs-cratesaxosrcjobsrs) for the model-facing behavior) is tracked by the session's `JobRegistry` and journaled so its result reaches the transcript **exactly once**, across navigation, crashes, and reopen.

- **Admission:** Promotion writes `job_start` first; if that write fails, the call stays in the foreground. The promoted tool result then carries `admittedJobs: ["bg_N"]`. A job becomes deliverable only once that receipt commits; a `job_start` whose admitting message never committed (or whose command finished before promotion took effect) is ignored. A retained `job_start`, written when a foreground command exits leaving helpers running, has no admitting receipt either: on reopen it is not a job, but its `bg_N` is counted so the id stays reserved.
- **Result:** When the job's root command ends, `job_end` records its result (unless the session is shutting down, in which case reopen reconciles it as interrupted). A failed `job_end` write is logged, and the result still becomes pending in memory; a persistent journal failure then surfaces through the next receipt-bearing message write.
- **Delivery receipt:** A pending result is reserved for exactly one message: a `wait` tool result, or a `job_result` custom message (`Background job results:` …). That message's entry carries `deliveredJobs: ["bg_N", …]`. Only a committed entry consumes the result: if the write fails, or the reserving message is a synthetic result (aborted or never-executed `wait`), the reservation is released and the result is delivered again later. Delivery is counted at transcript commit, not at provider acceptance: once committed, the result is part of the transcript, and a later failed or retried provider request never resends it as a new message.
- **Raw reconstruction:** On open, the registry is rebuilt from **all raw entries** of the file, not the projected branch, so receipts on abandoned branches still count. For each `job_start` of this session: admitted and delivered → done; admitted and ended but not delivered → pending; admitted and not ended → reconciled as interrupted (see [Crash Recovery](#crash-recovery-cratesaxosrcrecoveryrs)). New jobs number from one past the highest `bg_N` any raw record or receipt names (`job_start`, `job_end`, `admittedJobs`, `deliveredJobs`), so an id the model has seen is never reused. A second open finds the `job_end` and writes nothing.
- **No wake on open:** Opening a session never starts a model run for pending or interrupted results. They reach the model with the next prompt (ahead of the user's message), at the steering point of a run that recovery resumes, or through `wait`.
- **Root result vs. retained helpers:** A command's result is its root shell's: it returns once the root exits plus a fixed 100 ms output drain. Descendants still alive in the command's container stay supervised as `bg_N` with status `helpers running`, keep writing to the same spool, and can be inspected or killed through `proc://`. Their later exit never produces a second result or delivery (see [Background Jobs](prompts-and-context.md#background-jobs-cratesaxosrcjobsrs)).
- **Idle wake:** When a pending result appears while the session is idle (no run, no maintenance), `Session` starts a run whose prompt is the `job_result` message. During a run, results are injected at the next turn boundary, after the current tool batch; they never count as a steer.
- **Stop latch:** `Session::abort` stops only the current turn and its foreground command; background jobs keep running. If a run was active, the Stop latches automatic continuation off. The latch is also set when a run settles with any outcome other than `Done` (error, abort, interruption, persist or journal failure). While latched, finished results stay pending instead of waking the session; the next `prompt`, `steer`, or `follow_up` clears the latch and carries them. A Stop while idle sets nothing.
- **Shutdown:** `Session::shutdown` waits for the active run, then stops every open job and awaits its task; dropping a `Session` kills every open job's container. Cleanup reaches what the selected container tracks: process-group mode cannot see descendants that called `setsid`, and both modes are best effort if the session process dies without running shutdown.

## Manual `/shake` (`Session::shake`)

`session.shake(mode)` shrinks projected context without calling a model. It is also the [built-in command](prompts-and-context.md#user-commands-cratesaxosrccommandsrs) `/shake <mode>` (`elide`, `images`, or `thinking`; `ShakeMode` serializes lowercase). Maintenance emits `SessionEvent::MaintenanceStart` and `SessionEvent::MaintenanceEnd`, and returns `Error::Busy` while a turn or maintenance is running (`Error::MaintenanceAborted` if cancelled):

| `ShakeMode` | Behavior |
| :--- | :--- |
| `ShakeMode::Elide` | Outside the newest `4,000` tokens, replaces tool-result text blocks and fenced/XML blocks in user or assistant text of `>= 400` tokens with `[shaken ~N tokens; full text: artifact://<n>]`, writing the original text to `<session>/artifacts/<n>.shake.txt` first. Skill spans (`<skill …>…</skill>`), `SKILL.md` read results, system messages, compaction summaries, and tool-call arguments are never touched |
| `ShakeMode::Images` | Replaces every `UserBlock::Image` in user, tool-result, and custom messages (except messages containing a skill span) with `[shaken image; full image: artifact://<n>]`, writing the decoded image bytes to `<session>/artifacts/<n>.shake.<ext>` |
| `ShakeMode::Thinking` | Replaces each `AssistantBlock::Thinking` with `[shaken thinking; full thinking: artifact://<n>]`, saving that block's JSON to `<session>/artifacts/<n>.shake.json` (skipped in messages containing a skill span) |

Each modified message receives one `Entry::ContextEdit`. Because edits are append-only, navigating to the entry before the shake restores the unshaken view.

## Automatic Compaction (`crates/axo/src/compaction/`)

### Window and trigger math (`Limits::resolve`)

```text
W           = min(model.context_window, compaction.max_context)
reply       = min(model.max_tokens, 32_768)
reserve     = compaction.reserve_tokens.unwrap_or(min(reply + 4_096, W / 4))
trigger     = W - reserve
keep_recent = compaction.keep_recent_tokens.unwrap_or(min(20_000, trigger / 4))
```

Context tokens are estimated from the latest non-errored assistant message's provider-reported usage plus local token estimates (`(utf16_units * 2).div_ceil(7)`, `4,800` per image) for messages after it, falling back to full local estimation when an edit invalidates earlier usage.

Automatic maintenance checks `estimate > trigger` in `prepare_request` (before a prompt and between tool turns) and in `finish_turn` (after a turn settles).

### One action per pass: clearing vs summarization

When `estimate > trigger`, `axo` evaluates two candidates locally:

1. **Recoverable clearing (`prepare_clearing`):** Protects the newest `40,000` tokens of tool-result content and elides older eligible tool-result text into `<session>/artifacts/<n>.shake.txt` markers (`[shaken ~N tokens; full text: artifact://<n>]`). Clearing is chosen **only** if the resulting context estimate is at most `trigger - 20,000`, and is skipped when `trigger < 20,000`.
2. **Checkpoint summarization (`Entry::Compaction`):**
   - **Cut point:** Walks backward from the end of the transcript accumulating `keep_recent` tokens and cuts at the first valid boundary (`User`, `Custom`, or `Assistant`, never a `ToolResult`). If a single turn exceeds the budget, the turn is split at an `Assistant` boundary and its prefix is summarized with `COMPACTION_SPLIT_TURN.md` and merged with the history summary.
   - **Live summary (`SummaryRequest::Live`):** Appends the summary prompt to the live uncleared conversation with `ToolChoice::None` so the request reuses the provider's prompt cache. If the response contains a tool call, stops with `Length` / `Error` / `Aborted`, is empty, or is not strictly smaller than the messages it replaces, it is rejected and `axo` falls back to a separate summary request.
   - **Separate summary (`SummaryRequest::Separate`):** Serializes the messages to summarize into `<conversation>` text and sends a tool-free request under `SUMMARY.md`.
   - **Carryover:** The committed `Entry::Compaction` stores the current `SystemMessage` snapshot, `preserved_user_ids` (recent whole user messages from the summarized region that fit within `keep_recent`), `details: {"readFiles": […], "modifiedFiles": […]}`, and the summary text. File lists come from successful `read`, `write`, and `edit` results and use absolute paths.
   - **Escalation bound:** Automatic maintenance attempts one clearing attempt, then at most 3 summary attempts (falling back to a separate request after a failure) before failing with `Error::Compaction`.

## Transient Retry & Overflow Recovery

- **Transient retry (`axo_ai::retry`):** When a turn ends with a retryable provider error (`HTTP 429`, `500`, `502`, `503`, `504`, `520`, `524`, stream transport errors, or `overloaded`/`rate_limit`/`too many requests`/`service_unavailable`/`server_error` without quota exhaustion), `Session` hides the failed assistant message with `Entry::ContextEdit { replacement: None }`, emits `SessionEvent::Retry { attempt, delay_ms }`, sleeps (`2000ms * 2^(attempt - 1)` or the provider's `Retry-After` / reset header up to `60,000ms`; a header exceeding 60 s is not retried), and retries up to 3 times.
- **One-shot overflow recovery (`axo_ai::overflow`):** When a turn fails with context overflow (or stops with `StopReason::Length` when output is 0 and input + cache_read >= 99% of context window, or output is below requested max_tokens) and `compaction.enabled` is `true`, `Session` hides the failed turn and any tool results it produced, runs recoverable clearing followed by a separate-request compaction pass if needed, and retries once within the same run.
