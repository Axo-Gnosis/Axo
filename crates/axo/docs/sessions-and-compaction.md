# Sessions, Durability & Compaction

`axo` persists every session as an append-only JSONL entry tree (`crates/axo/src/store/` and `crates/axo/src/session.rs`) and maintains context size through recoverable artifact elision and model summaries (`crates/axo/src/compaction/`).

## Session Storage (`JsonlStore` & `SessionStore`)

By default, sessions are stored under `<AXO_DIR>/sessions/--<encoded-cwd>--/<timestamp>_<uuidv7>.jsonl`, where `<encoded-cwd>` replaces `/`, `\`, and `:` with `-`.

- **Lazy file creation:** `Axo::create_session` reserves a planned path and `Header` in memory. The first submission or non-system message creates the file and parent directory, flushing earlier buffered metadata. A built-in command that only changes model or thinking settings does not by itself create a new session file.
- **Single-writer lock:** Opening an existing session for writing (`JsonlStore::open_write`) acquires an exclusive non-blocking OS file lock (`std::fs::File::try_lock`) on the session file itself, held while the writer is open. A second writer receives `Error::SessionBusy`. Read-only opens (`Axo::open_session_read`, `Axo::list_sessions`, and `Axo::schemes`, which reads only the header) take no lock.
- **Torn-line repair:** If a crash leaves a final line without a trailing newline (or an unparseable truncated final line), `open_write` truncates the file back to the last complete line before appending.
- **Sequence numbers (`SeqEntry`):** Every entry loaded or appended receives a 0-based monotonic `seq: u64` corresponding to its line index after the `Header`.

### JSONL format (`FORMAT_VERSION = 1`)

Line 1 is the `Header` (`{"type":"session","version":1,"id":"…","timestamp":"…","cwd":"…","parentSession":…,"forkedFrom":…}`). Subsequent lines are `Entry` variants sharing `EntryBase { id, parent_id, timestamp }`:

| `type` | Variant | Role in projection |
| :--- | :--- | :--- |
| `message` | `Entry::Message { base, message, submission_id }` | Appends `AgentMessage` (`System`, `User`, `Assistant`, `ToolResult`, or `Custom`) |
| `thinking_level_change` | `Entry::ThinkingLevelChange { base, thinking_level }` | Updates `Projection::thinking` |
| `model_change` | `Entry::ModelChange { base, provider, model_id }` | Updates `Projection::model` (`"{provider}/{model_id}"`) |
| `usage` | `Entry::Usage { base, kind, provider, model, usage, note }` | Recorded for accounting (such as summary requests); never enters model context |
| `compaction` | `Entry::Compaction { base, summary, first_kept_entry_id, preserved_user_ids, tokens_before, details, usage, from_hook, system_message }` | Replaces messages before `first_kept_entry_id` with the checkpoint `system_message`, preserved user messages, and the `<compacted-history>` summary message |
| `branch_summary` | `Entry::BranchSummary { base, from_id, summary, details, usage, from_hook }` | Injects a `<branch-summary>` user message summarizing an abandoned branch |
| `custom` | `Entry::Custom { base, custom_type, data }` | Host metadata; never enters model context |
| `custom_message` | `Entry::CustomMessage { base, custom_type, content, details, display }` | Lowered to a user message in model requests |
| `context_edit` | `Entry::ContextEdit { base, target_id, replacement }` | Replaces `target_id`'s content (`Some(ContextReplacement)`) or hides `target_id` from the projection (`None`) |
| `label` | `Entry::Label { base, target_id, label }` | Labels an entry in `Projection::labels` |
| `session_info` | `Entry::SessionInfo { base, name }` | Updates `Projection::name` |
| `leaf` | `Entry::Leaf { base }` | Moves the active branch pointer to `base.parent_id` (`None` = before the first entry) |
| `run_start` | `Entry::RunStart { base, run_id }` | Marks the start of an agent run |
| `run_end` | `Entry::RunEnd { base, run_id, outcome, reason }` | Marks a settled run (`done`, `aborted`, `error`, `interrupted`, `parked`) |
| `run_resumed` | `Entry::RunResumed { base, run_id, attempt }` | Records a crash recovery resume attempt |
| `submission` | `Entry::Submission { base, submission_id, kind, parts, request_id }` | Durable `prompt`, `steer`, or `follow_up` input recorded before queueing. `parts` holds the prepared [prompt parts](prompts-and-context.md#prompt-parts-cratesaxosrcinputrs) as sent (`Saved` instead of `Content`, skill commands unexpanded); the model-facing text lives on the user `message` entry. Older files wrote `content` (text blocks); it loads through `#[serde(alias = "content")]` as `Text` parts. Built-in commands write no submission |
| *(unknown)* | `Entry::Unknown(Value)` | Preserved verbatim on load; ignored by projection |

## Projection (`crates/axo/src/store/project.rs`)

`project(entries, leaf)` walks the ancestor chain from the current leaf (`None` uses the last entry in the file; an `Entry::Leaf` moves the leaf to its `parent_id`) back to the root:

1. **Latest compaction boundary:** If the root-to-leaf path contains one or more `Entry::Compaction` entries, projection starts from the last compaction's `system_message` snapshot, emits its `preserved_user_ids` in path order, emits the `<compacted-history>` message (which includes the session `.jsonl` path so the model can inspect omitted lines), and then replays entries from `first_kept_entry_id` onward.
2. **Branch summaries:** Any `Entry::BranchSummary` on the active segment becomes a `<branch-summary>` user message.
3. **`ContextEdit` application:** On the active path, the last `Entry::ContextEdit` targeting an entry ID wins. `replacement: None` removes that message from the projection (used to hide transient-error or overflow turns); `Some(ContextReplacement)` replaces its content blocks (used by `/shake` and automatic clearing) and invalidates provider token usage recorded on or before the edit timestamp.
4. **Incremental caching (`ProjectedLog`):** `Session` maintains the projection incrementally as entries are appended, rebuilding from scratch only when navigating or when an edit/compaction invalidates earlier entries.

## Crash Recovery (`crates/axo/src/recovery.rs`)

When `Axo::open_session` opens a session file, `plan_recovery` inspects the log after the last `Entry::RunEnd`:

- **Unfinished tool calls:** If the last assistant message on the path has tool calls without matching `ToolResult` entries:
  - Tools with `Replay::Never` (`bash`, `edit`, and `write` to a scheme whose `write_replay` is `Never`) immediately receive a persisted synthetic `ToolResult` with `is_error: true` explaining that the process stopped before completion (and pointing to any partial `bash` output as `artifact://<n>`, backed by `<session>/artifacts/<n>.bash.<escaped-call-id>.log`).
  - Under `RecoveryPolicy::Resume` (default), tools with `Replay::Safe` (`read`, `grep`, `find`, `ask`, and `write` to a plain path or `local://`) are left unanswered so `Agent::continue_run` re-executes them in the background.
  - Under `RecoveryPolicy::MarkInterrupted`, `Replay::Safe` calls also receive interrupted results and `Entry::RunEnd { outcome: Interrupted }` is appended immediately.
- **Interrupted or failed assistant responses:**
  - If the last assistant response stopped with `StopReason::Length` and had tool calls, every unanswered call receives a truncated-output error result and the run continues.
  - If the last assistant response was a silent stop (`StopReason::Stop` with empty content and no tool calls), a retryable provider error, or a context overflow, recovery hides the failed assistant turn with `Entry::ContextEdit { replacement: None }` and resumes the run (consuming one retry budget slot or running overflow compaction).
  - A non-retryable failed or aborted response with unanswered tool calls gets not-executed (or `Operation aborted`) results before the run closes `Failed`/`Aborted`.
- **Unconsumed submissions:** Any `Entry::Submission` saved after the last `RunEnd` (or during an open run) whose `submission_id` has not yet been consumed by an `Entry::Message` is requeued in order (`Steer`, `FollowUp`, or starting a new `Prompt` run). Requeued messages are rendered again from the stored `parts`: a skill command is expanded from the current `SKILL.md`, and a saved image is re-read from `local://`.
- **Restart loop cap (`MAX_CONSECUTIVE_RESUMES = 5`):** If a run accumulates 5 `Entry::RunResumed` markers in a row without an intervening `Entry::Message`, recovery appends `Entry::RunEnd { outcome: Parked }` and stops resuming it.

## Attaching, Navigating & Forking (`Session`)

- **`session.attach(from_seq)`:** Atomically snapshots `Attach { missed, live, events }`:
  - `missed`: all `SeqEntry` items with `seq > from_seq` (or all entries when `from_seq` is `None`).
  - `live`: `LiveSnapshot` containing `run_id`, active `maintenance` (`Compact`, `Shake`, `BranchSummary`), `agent` (`AgentSnapshot` with `running`, partial `streaming` message from `Accumulator`, `pending_tool_calls`, `steering_queue`, `follow_up_queue`), projected `messages`, and `pending_questions`.
  - `events`: a `tokio::sync::broadcast::Receiver<SessionEvent>`. If a slow receiver lags (`RecvError::Lagged`), it can call `session.attach(Some(last_seen_seq))` to catch up without stalling the agent loop.
- **`session.navigate(target, NavigateOptions { summarize })`:**
  - Appends `Entry::Leaf { base: EntryBase { parent_id: target, .. } }`, records `ModelChange` / `ThinkingLevelChange` if the live selection differs from the target branch's historical selection, restores the file ledger and nested context tracker to the new projection, and updates the agent's transcript.
  - With `summarize: true`, first summarizes the abandoned branch (using a live summary request when navigating away from the active tip, or a separate request otherwise) and appends `Entry::BranchSummary` at `target`.
- **`session.fork(entry_id)`:**
  - Copies the root-to-`entry_id` ancestor chain into a new session file whose `Header` records `parent_session` and `forked_from`.
  - Copies the parent's side files into the fork's session directory: `artifacts/` (hard-linked read-only, copy fallback) and `local/` (copied), so `artifact://` and `local://` URLs, including saved prompt content, resolve in the fork. Copying is best effort; an entry that cannot be carried over is logged and skipped.
  - If `entry_id` is a user message, `fork` copies up to that user message's parent and returns `Fork::editable: Option<Vec<PromptPart>>` so a client can prefill its editor, chips included: the parts of the `submission` that produced the message, or, for a message without one (older sessions), one `Text` part joining its text blocks. The returned parts can be resubmitted as-is (`Saved` parts resolve to the copied `local://` files). Any open run on the copied path is closed with `RunEnd { outcome: Interrupted, reason: Some("fork") }`.

## Manual `/shake` (`Session::shake`)

`session.shake(mode)` shrinks projected context without calling a model. It is also the [built-in command](prompts-and-context.md#user-commands-cratesaxosrccommandsrs) `/shake <mode>` (`elide`, `images`, or `thinking`; `ShakeMode` serializes lowercase). Like `compact` and `navigate`, it returns `Error::Busy` while a turn or maintenance is running:

| `ShakeMode` | Behavior |
| :--- | :--- |
| `ShakeMode::Elide` | Outside the newest `4,000` tokens (`PROTECTED_TAIL_TOKENS`), replaces tool-result text blocks and fenced/XML blocks in user or assistant text of `>= 400` tokens (`MIN_ELIDE_TOKENS`) with `[shaken ~N tokens; full text: artifact://<n>]`, writing the original text to `<session>/artifacts/<n>.shake.txt` first. Skill spans (`<skill …>…</skill>`), `SKILL.md` read results, system messages, compaction summaries, and tool-call arguments are never touched |
| `ShakeMode::Images` | Replaces every `UserBlock::Image` in user messages and tool results with `[shaken image; full image: artifact://<n>]`, writing the decoded image bytes to `<session>/artifacts/<n>.shake.<ext>` |
| `ShakeMode::Thinking` | Strips `AssistantBlock::Thinking` blocks from assistant messages, saving them to `<session>/artifacts/<n>.shake.json` and adding `[shaken thinking; full thinking: artifact://<n>]` if an assistant message had only thinking |

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

1. **Recoverable clearing (`prepare_clearing`):** Protects the newest `40,000` tokens (`CLEARING_PROTECTED_TOOL_TOKENS`) of tool-result content and elides older eligible tool-result text of `>= 400` tokens into `<session>/artifacts/<n>.shake.txt` markers (`[shaken ~N tokens; full text: artifact://<n>]`). Clearing is chosen **only** if the resulting context estimate is at most `trigger - 20,000` (`CLEARING_HEADROOM_TOKENS`), avoiding repeated prompt-cache invalidations.
2. **Checkpoint summarization (`Entry::Compaction`):**
   - **Cut point:** Walks backward from the end of the transcript accumulating `keep_recent` tokens and cuts at the first valid boundary (`User`, `Custom`, or `Assistant`, never a `ToolResult`). If a single turn exceeds the budget, the turn is split at an `Assistant` boundary and its prefix is summarized with `COMPACTION_SPLIT_TURN.md` and merged with the history summary.
   - **Live summary (`SummaryRequest::Live`):** Appends the summary prompt to the live uncleared conversation with `ToolChoice::None` so the request reuses the provider's prompt cache. If the response contains a tool call, stops with `Length` / `Error` / `Aborted`, is empty, or is not strictly smaller than the messages it replaces, it is rejected and `axo` falls back to a separate summary request.
   - **Separate summary (`SummaryRequest::Separate`):** Serializes the messages to summarize into `<conversation>` text and sends a tool-free request under `SUMMARY.md`.
   - **Carryover:** The committed `Entry::Compaction` stores the current `SystemMessage` snapshot, `preserved_user_ids` (recent whole user messages from the summarized region that fit within `keep_recent`), `details: {"readFiles": […], "modifiedFiles": […]}`, and the summary text. File lists come from successful `read`, `write`, and `edit` results and use absolute paths.
   - **Escalation bound:** Automatic maintenance attempts at most 3 actions per check before failing with `Error::Compaction`.

## Transient Retry & Overflow Recovery

- **Transient retry (`axo_ai::retry`):** When a turn ends with a retryable provider error (`HTTP 408`, `429`, `500`, `502`, `503`, `504`, `529`, stream transport errors, or `overloaded`/`rate_limit`/`too many requests`/`service_unavailable`/`server_error` without quota exhaustion) or a silent stop, `Session` hides the failed assistant message with `Entry::ContextEdit { replacement: None }`, emits `SessionEvent::Retry { attempt, delay_ms }`, sleeps (`2000ms * 2^(attempt - 1)` or the provider's `Retry-After` / reset header up to `60,000ms`), and retries up to 3 times.
- **One-shot overflow recovery (`axo_ai::overflow`):** When a turn fails with context overflow (or stops with `StopReason::Length` when input + output reaches `model.context_window`) and `compaction.enabled` is `true`, `Session` hides the failed turn and any tool results it produced, runs a separate-request compaction pass (advancing the omission boundary if a single oversized tool group prevents shrinking), and retries once within the same run.
