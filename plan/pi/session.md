# pi core: sessions, compaction, branch summaries, settings

Paths are relative to `/tmp/pi-src/packages/coding-agent/` unless prefixed. `SM` = `src/core/session-manager.ts`, `CMP` = `src/core/compaction/compaction.ts`, `BS` = `src/core/compaction/branch-summarization.ts`, `UT` = `src/core/compaction/utils.ts`, `AS` = `src/core/agent-session.ts`, `MSG` = `src/core/messages.ts`.

There is no `migrations.ts` in this tree. Session migrations live in `SM:286-352`. The current format version is **3** (`SM:41`).

Note: this pi fork is ahead of upstream pi. It has `system` role messages that carry prompt/tool deltas, `context_edit` entries, `usage` entries, and a `systemMessage` checkpoint on compaction. Port these as they are written here.

---

## 1. Concepts

- **A session is an append-only JSONL file holding a tree.** Line 1 is a header. Every other line is an entry with `id` and `parentId`. Branching never rewrites history: it moves an in-memory **leaf** pointer, and the next append becomes a child of that leaf (`SM:976-986`).
- **The LLM context is a projection.** It is computed from the root→leaf path. The latest compaction on the path replaces older entries with a summary plus a "kept tail". `context_edit` entries then patch or omit individual messages. Some entries are state only (model/thinking changes, labels, custom state, usage) and produce no messages (`SM:439-583`).
- **Compaction** adds a `compaction` entry holding an LLM summary of everything before `firstKeptEntryId`. Nothing is deleted. Repeated compactions update the previous summary iteratively (`CMP:577-579`, `CMP:876-882`).
- **Branch summarization** runs on `/tree` navigation. It summarizes the abandoned path (old leaf back to the common ancestor) into a `branch_summary` entry that is attached at the new position (`BS:108-146`, `AS:4024-4216`).
- **Fork and clone** copy a root→entry path into a *new* file whose `parentSession` points at the source (`SM:1632-1748`, `src/core/agent-session-runtime.ts:262-350`).
- **Prompt and tools live in the transcript.** System messages persist prompt `sections` and tool deltas. Replaying them yields the current prompt (`docs/session-format.md:80-87`, `packages/ai/src/utils/transcript.ts:73-96`). A compaction snapshots the replay as `systemMessage` (`SM:1270-1283`).

## 2. Types (persisted contract)

```ts
// SM:41-50
const CURRENT_SESSION_VERSION = 3;
interface SessionHeader { type: "session"; version?: number /* absent = v1 */; id: string; timestamp: string /* ISO */; cwd: string; parentSession?: string /* path of source file */ }

// SM:57-62
interface SessionEntryBase { type: string; id: string /* 8 hex chars; UUID fallback */; parentId: string | null; timestamp: string /* ISO */ }

// SM:64-180
interface SessionMessageEntry       { type: "message"; message: AgentMessage }   // roles persisted: system|user|assistant|toolResult|bashExecution (AS:1158-1166, AS:3957)
interface ThinkingLevelChangeEntry  { type: "thinking_level_change"; thinkingLevel: string }
interface ModelChangeEntry          { type: "model_change"; provider: string; modelId: string }
interface UsageEntry                { type: "usage"; kind: string; provider: string; model: string; usage: Usage; note?: string }   // never in context
interface CompactionEntry<T>        { type: "compaction"; summary: string; firstKeptEntryId: string; tokensBefore: number;
                                      details?: T; usage?: Usage; fromHook?: boolean; systemMessage?: SystemMessage }
interface BranchSummaryEntry<T>     { type: "branch_summary"; fromId: string /* old leaf id or "root" */; summary: string; details?: T; usage?: Usage; fromHook?: boolean }
interface CustomEntry<T>            { type: "custom"; customType: string; data?: T }                  // extension state, NOT in context
interface CustomMessageEntry<T>     { type: "custom_message"; customType: string; content: string | (TextContent|ImageContent)[]; details?: T; display: boolean } // IS in context
interface ContextEditEntry          { type: "context_edit"; targetId: string; replacement: { content: ContextEditableContent } | null }
interface LabelEntry                { type: "label"; targetId: string; label: string | undefined }
interface SessionInfoEntry          { type: "session_info"; name?: string }

type FileEntry = SessionHeader | SessionEntry;   // SM:183-197
```

Default `details` for both summary kinds is `{ readFiles: string[]; modifiedFiles: string[] }` (`CMP:52-55`, `BS:44-47`).

Projected/runtime message types (in memory only; never persisted as `message` entries, see `SM:1198-1203`):

```ts
// MSG:29-66
interface BashExecutionMessage { role: "bashExecution"; command; output; exitCode?; cancelled; truncated; fullOutputPath?; timestamp: number; excludeFromContext? }  // this one IS persisted as a message entry
interface CustomMessage        { role: "custom"; customType; content; display; details?; timestamp: number }
interface BranchSummaryMessage { role: "branchSummary"; summary; fromId: string | null; timestamp: number }
interface CompactionSummaryMessage { role: "compactionSummary"; summary; tokensBefore; timestamp: number }
```

Runtime projection types:

```ts
// SM:199-227
interface SessionTreeNode { entry; children: SessionTreeNode[]; label?; labelTimestamp? }
interface ProjectedSessionEntry { sourceEntry: SessionEntry; messages: AgentMessage[] }
interface SessionProjection { entries: ProjectedSessionEntry[]; messages: AgentMessage[]; thinkingLevel: string; model: {provider, modelId} | null }
interface SessionContext { messages; thinkingLevel; model }
// SM:229-243 SessionInfo (listing): path, id, cwd, name?, parentSessionPath?, created, modified, messageCount, firstMessage, allMessagesText
```

Compaction types (`CMP:105-130`, `CMP:421-428`, `CMP:759-775`):

```ts
interface CompactionSettings { enabled: boolean; reserveTokens: number; keepRecentTokens: number }  // defaults true/16384/20000
interface CompactionResult<T> { summary; firstKeptEntryId; tokensBefore; estimatedTokensAfter?; usage?; details?: T }
interface CutPointResult { firstKeptEntryIndex: number; turnStartIndex: number; isSplitTurn: boolean }
interface CompactionPreparation { firstKeptEntryId; messagesToSummarize: AgentMessage[]; turnPrefixMessages: AgentMessage[];
  isSplitTurn; tokensBefore; previousSummary?; fileOps: FileOperations; settings }
interface FileOperations { read: Set<string>; written: Set<string>; edited: Set<string> }   // UT:12-16
```

## 3. Behavior

### 3.1 File location, naming, ids

- Default directory: `<agentDir>/sessions/--<cwd without leading separator, with / \ : replaced by ->--/` (`SM:589-594`). `agentDir` is `~/.pi/agent`, overridable with `PI_CODING_AGENT_DIR` (`src/config.ts:585-608`). `getSessionsDir()` = `agentDir/sessions` (`src/config.ts:649-651`). A custom directory can come from `--session-dir`, `PI_CODING_AGENT_SESSION_DIR`, or the `sessionDir` setting; the CLI flag wins (`docs/sessions.md:50`, `settings-manager.ts:690-693`).
- File name: `${isoTimestamp.replace(/[:.]/g,"-")}_${sessionId}.jsonl` (`SM:1079-1080`).
- The session id is a UUIDv7 by default (`SM:264-266`). A caller-supplied id must match `^[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?$` (`SM:268-274`).
- Entry id: the first 8 characters of a random UUID, checked against `byId` for collisions; after 100 failures it falls back to a full UUID (`SM:277-284`).

### 3.2 Loading and migration

- `loadEntriesFromFile` streams 1 MiB chunks and splits on `\n`. Blank or malformed lines are **silently skipped** (`SM:616-624`, `SM:627-657`).
- If the first parsed entry is not `{type:"session", id:string}`, the result is `[]` (`SM:661-666`).
- A trailing partial line with no newline gets a `\n` appended to the file. This repairs a torn write so the next append starts on a fresh line. The partial line itself is still parsed if it is valid JSON (`SM:668`).
- `_setSessionFile` (`SM:1029-1055`):
  - File exists but parses to nothing and size > 0: throws "not a valid session". The file is never overwritten.
  - Empty file: a new header is written into it.
  - Missing file: a new in-memory session at that explicit path.
- Migration runs when `header.version ?? 1 < 3`. The file is then **rewritten in place** (`SM:337-347`, `SM:1092-1094`, `SM:1124-1134`).
  - v1→v2: assigns ids/parentIds as a linear chain and converts `firstKeptEntryIndex`→`firstKeptEntryId` (`SM:287-313`).
  - v2→v3: message role `hookMessage`→`custom` (`SM:316-331`).
- `_buildIndex`: `byId` covers every non-header entry. **The leaf is the last entry in file order** (`SM:1103-1122`).
- Labels resolve last-writer-wins in file order, globally rather than per branch (`SM:1112-1120`).

### 3.3 Leaf, appends, tree

- Every `appendX` builds an entry with `parentId = this.leafId`, pushes it, indexes it, sets `leafId = entry.id`, then persists it (`SM:1191-1196`).
- `appendMessage` refuses nothing at the type level. By convention, compaction and branch summaries go through their own methods (`SM:1198-1214`).
- **The leaf pointer is not persisted.** On reload it is the last line in the file (`SM:1111`). `branch(id)` and `resetLeaf()` only mutate memory (`SM:1579-1593`). So a `/tree` navigation without a summary or label is lost if the process exits before the next append. [INFERENCE from code; no persistence of leafId exists.]
- `getBranch(fromId?)` walks parents up to the root and reverses the result (`SM:1469-1479`). `buildSessionPath` does the same and stops at a missing parent, so it tolerates broken chains (`SM:390-416`).
- `getTree()`: an entry is a root when its `parentId` is null, equals its own id, or points at a missing parent (an orphan). Children are sorted by timestamp. Traversal is iterative to avoid deep recursion (`SM:1529-1567`).
- `appendLabelChange` requires the target to exist. The label entry is a real tree node and **advances the leaf** (`SM:1441-1462`).
- `appendSessionInfo` replaces newlines with spaces and trims. `getSessionName` returns the last `session_info` in file order; an empty name clears it (`SM:1304-1329`).
- `appendContextEdit` validation (`SM:1360-1398`):
  - The replacement must be `null` or `{content: string|array}`.
  - The target must exist and be **on the active branch**.
  - The target must be a `custom_message` entry or a `message` with role user, assistant or toolResult.
  - String content for assistant/toolResult targets is normalized to `[{type:"text",text}]`.
- `appendCompaction(summary, firstKeptEntryId|null, tokensBefore, details?, fromHook?, usage?)` (`SM:1261-1287`):
  - `null` firstKept → the compaction's own id. This is a "retain-none" compaction.
  - It snapshots `systemMessage = getCurrentSystemMessage(currentProjection.messages)` with timestamp = the compaction time.
- `branchWithSummary(fromId|null, ...)` (`SM:1600-1625`):
  - `fromId` field = old leaf, or `"root"` when there is no leaf.
  - Sets the leaf to the target, then appends a `branch_summary` whose `parentId` is the target.
  - A `null` target makes a **new root**, so multiple roots are legal (`docs/session-format.md:211-216`).

### 3.4 Context rebuild (canonical algorithm)

`buildContextEntries(entries, leafId?)` (`SM:476-512`):

1. `path = buildSessionPath(leaf)`. The leaf is `undefined` → last entry; `null` → empty path.
2. `compaction` = the **last** compaction on the path. If there is none, return the path.
3. Output is `[compaction]`, then the entries in `path[0..compactionIdx)` from the first one whose id equals `firstKeptEntryId`, **excluding `message` entries with role `system`**, then all of `path[compactionIdx+1..]`.
4. When `firstKeptEntryId` is not found in the pre-compaction path (retain-none, or a broken id), no pre-compaction entries are kept.

`buildSessionProjection` (`SM:543-573`):

- thinkingLevel and model come from the **full path**, not the compacted list (`SM:418-433`).
  - thinkingLevel defaults to `"off"`.
  - `model_change` sets the model, and so does every assistant message (its `provider`/`model`).
- `edits`: map `targetId → latest context_edit`. The map is built **only from the selected context entries**, in order, so later edits win (`SM:551-554`).
- Each selected entry becomes `ProjectedSessionEntry` with `messages = projectContextEntry(entry, edits.get(id))`.
- A compaction entry at index > 0 contributes `[]`. This is an older compaction that happens to sit inside the newest one's kept range (`SM:555-566`).
- `sessionEntryToContextMessages` (`SM:439-466`):
  - `message` → the stored message. Null content is normalized to `""` for system and `[]` for user/assistant/toolResult.
  - `custom_message` → `CustomMessage` (timestamp from entry ISO → ms).
  - `branch_summary` with a non-empty summary → `BranchSummaryMessage`.
  - `compaction` → `[systemMessage?, CompactionSummaryMessage]`.
  - Every other entry type → `[]`.
- `projectContextEntry` (`SM:519-540`):
  - An edit with `replacement:null` → `[]`.
  - Otherwise the content of user/assistant/toolResult/custom messages is replaced, keeping all other fields.
  - A string replacement for assistant/toolResult is wrapped as a text block.
- `buildSessionContext` = the projection's `{messages, thinkingLevel, model}` (`SM:576-583`).

Conversion to the provider is `convertToLlm` (`MSG:148-196`):

- `bashExecution` → user text (skipped if `excludeFromContext`; text format in `MSG:72-89`).
- `custom` → user message (string content wrapped as a text block).
- `branchSummary` → user text `BRANCH_SUMMARY_PREFIX + summary + "</summary>"`.
- `compactionSummary` → user text `"The conversation history before this point was compacted into the following summary:\n\n<summary>\n" + summary + "\n</summary>"` (`MSG:11-24`).
- `system`, `user`, `assistant` and `toolResult` pass through unchanged.
- System messages are later collapsed into one leading system message for providers without mid-conversation system support (`packages/ai/src/utils/transcript.ts:108-120`).

When the runtime calls this:

- On every request: `prepareRequest` rebuilds `messages` from `buildSessionProjection()` (`AS:787-811`).
- After compaction, navigation or bash, `_refreshFinalizedContext` sets `agent.state.messages = projection.messages` and maps message object → entry id (`AS:938-944`).

### 3.5 Persistence, flush and crash safety

- **Lazy file creation** (`SM:1160-1189`). Entries stay in memory until `fileEntries` contains a `user` or `assistant` message.
  - Then the whole buffer is written with `openSync(path,"wx")` (exclusive create; fails if the file exists).
  - After that, every entry is one `appendFileSync(path, JSON.stringify(entry)+"\n")`.
  - Starting at the user message (rather than the first reply) keeps the prompt on disk even if the first turn never finishes (`SM:1160-1165`).
- No fsync, no lock and no atomic rename for sessions. Writes are synchronous, one write call per line. A crash can leave at most a torn last line, which the next load skips and newline-terminates (`SM:668`).
- Two full rewrites are non-atomic (`openSync(...,"w")`): after migration (`SM:1092-1094`) and when an empty file is initialized (`SM:1043`). [INFERENCE: a crash mid-rewrite during migration could lose data; a Rust port should write to a temp file and rename.]
- What the runtime persists:
  - On `message_end`: `custom` → `appendCustomMessageEntry`; system, user, assistant and toolResult → `appendMessage` (`AS:1146-1168`). Each message is persisted only when it is *finished*; streaming partials are never written.
  - Bash `!` executions are appended directly, or deferred while a run streams (`AS:3957`, `AS:3988-3992`).
  - A new session writes `model_change` and `thinking_level_change` at startup. A resumed session without a thinking entry gets one appended (`src/core/sdk.ts:440-451`).
- The `persist=false` (in-memory) mode is identical minus the I/O (`SM:1803-1806`). It is used for boundary previews: a throwaway in-memory manager is seeded with header + branch (`AS:987-993`).

### 3.6 Listing and discovery

- `list(cwd, dir?)` (`SM:941-974`, `SM:1900-1915`):
  - Reads `*.jsonl` and builds `SessionInfo` for each with 10-way concurrency.
  - Info is built by streaming each file line by line: the header must come first; the name is the latest `session_info`; messageCount counts all `message` entries; firstMessage is the first user text; `modified` is the max user/assistant message timestamp, falling back to header time, then mtime (`SM:799-882`).
  - Results are sorted by `modified` descending.
  - Filters by header `cwd` only when a non-default directory is used.
- `listAll` scans every subdirectory of `sessions/`, with 64-way concurrency for stat (`SM:1927-2012`).
- `findMostRecentSession`: sorts by mtime and returns the first file whose bounded header scan (≤1 MiB) yields a valid header, cwd-filtered when applicable (`SM:685-768`).
- `findById`: header scan for an exact id (`SM:1874-1892`).
- `continueRecent` = the most recent session, or a new one (`SM:1793-1801`).
- `open(path)` takes cwd from the header unless overridden; the session directory defaults to the file's parent (`SM:1766-1786`). A missing cwd raises `MissingSessionCwdError` for the host to handle (`src/core/session-cwd.ts:14-59`).

### 3.7 Branching, fork, clone, navigation

**`/tree`: `navigateTree(targetId, {summarize, customInstructions, replaceInstructions, label})`** (`AS:4024-4216`):

1. Refuses while streaming or compacting. A no-op if the target is already the leaf. Throws if the target is unknown.
2. `collectEntriesForBranchSummary(old, target)` (`BS:108-146`). The common ancestor is the deepest node of the target path that is also on the old path. Entries are collected from the old leaf up to, but excluding, the ancestor, in chronological order. Compaction entries are included.
3. The `session_before_tree` hook may cancel, supply a summary, or override instructions/label (`AS:4084-4111`).
4. New leaf:
   - user message target → its parent, and the message text is returned as `editorText` for re-editing;
   - `custom_message` target → its parent, text returned the same way;
   - anything else → the target itself (`AS:4152-4163`).
5. With a summary: `branchWithSummary(newLeaf, ...)`, and the label is attached to the summary entry.
   Without a summary: `resetLeaf()` if the new leaf is null, otherwise `branch(newLeaf)`. The label is attached to the target (`AS:4165-4194`).
6. Refreshes context and the tool loadout from the transcript, then emits `session_tree` (`AS:4196-4207`).
7. A summarizer abort → `{cancelled:true, aborted:true}`. A summarizer error throws (`AS:4130-4135`).

**Fork: `fork(entryId, {position})`** (`src/core/agent-session-runtime.ts:262-350`):

- `position "before"` (`/fork`): the entry must be a user message. The target leaf is its parent, and its text is returned to the editor.
- `position "at"` (`/clone`): the target leaf is the entry itself; clone passes the current leaf (`src/modes/interactive/interactive-mode.ts:5519`).
- A null target leaf → a fresh `newSession({parentSession: current})`.
- Otherwise `SessionManager.open(current).createBranchedSession(targetLeaf)`. A persisted session must exist on disk first (`agent-session-runtime.ts:312-314`).
- The `session_before_fork` hook can cancel.

**`createBranchedSession(leafId)`** (`SM:1632-1748`):

- Copies the root→leaf path into a new file with a new id and `parentSession = old file`.
- **Drops `label` entries** and re-chains `parentId` across them. A compaction `firstKeptEntryId` that pointed at a dropped label is remapped to the next retained entry.
- Re-emits the resolved labels for path entries as fresh label entries chained after the last entry (with original label timestamps).
- Writes immediately if the path has a conversation. Otherwise `flushed=false` and the lazy rule applies.
- The manager switches to the new file. In-memory mode does the same without a path.

**`forkFrom(sourcePath, targetCwd)`** (cross-project, SDK/CLI `--fork`):

- Writes a new header (`wx`) with the target cwd and `parentSession = source`.
- Then appends every source entry verbatim, keeping the whole tree, not just one path (`SM:1815-1866`).

**`newSession({id?, parentSession?})`**: resets everything; the file is not created until a conversation exists (`SM:1057-1083`).

### 3.8 Compaction

**Settings.**

- Defaults: `{enabled: true, reserveTokens: 16384, keepRecentTokens: 20000}` (`CMP:126-130`, `src/core/settings-defaults.ts:9-13`).
- Per-model overrides `compaction.modelOverrides["provider/modelId"]` take priority over ordinary values, which take priority over defaults (`settings-manager.ts:833-880`).

**Trigger.**

- `shouldCompact = enabled && contextTokens > contextWindow - reserveTokens` (`CMP:267-270`). `reserveTokens` only sets the trigger; the summary uses the model's output limit (`CMP:647-648`, `docs/compaction.md:35`).
- Call sites:
  1. Before a new prompt: `_prepareRun` → `_checkCompaction(lastAssistant, skipAborted=false)` (`AS:1926-1932`).
  2. Between turns inside a run, after tool results are appended: `prepareNextTurnWithContext` → `_compactBeforeNextAssistantResponse` (threshold estimate on the projection) (`AS:776-785`, `AS:904-906`).
  3. After a run ends: `_checkCompaction(lastAssistant)` (`AS:2008`).
  4. Manual `compact(instructions)` aborts the current run first (`AS:2828-2851`).
- `_checkCompaction` (`AS:3011-3145`):
  - Skipped when compaction is disabled, when the message was aborted (unless pre-prompt), or when the message is **older than the latest compaction** (timestamp ≤ compaction time), which prevents re-triggering on stale usage.
  - **Overflow or recoverable length stop** from the same model:
    - If `stopReason` is `"stop"`: compact without retry.
    - Otherwise, if recovery was already attempted this prompt: emit failure.
    - Otherwise: set `_overflowRecoveryAttempted`, persist `context_edit(null)` omissions for the failed assistant and its tool results (`AS:1244-1272`), compact with reason `overflow` and `willRetry=true`, then the loop retries once. The flag resets on a new user message or a non-error/non-length assistant (`AS:1122-1123`, `AS:1173-1175`).
  - **Threshold**: `contextTokens` is computed as follows.
    - If the projection has context edits → `estimateProjectedContextTokens`.
    - Else, on error or zero usage → `estimateContextTokens(agent messages)`, returning false if the usage came from before the compaction.
    - Else → `calculateContextTokens(assistant.usage)`.
- `_runAutoCompaction` (`AS:3157-3304`):
  - Order: `prepareCompaction` → `session_before_compact` hook (cancel, or supply a `CompactionResult`) → default `compact()` → `appendCompaction` → refresh context → `session_compact` → `compaction_end`.
  - Returns whether to `agent.continue()`: willRetry, or queued messages.
  - Failures emit `compaction_end{errorMessage}` and `session_compact_failed`. Nothing is appended.

**Token estimation** (`CMP:136-349`):

- `calculateContextTokens(u) = u.totalTokens || input+output+cacheRead+cacheWrite`.
- Usable assistant usage: not aborted, not error, total > 0.
- `estimateContextTokens(msgs)` = last valid usage total + chars/4 for each later message. With no usage, it is chars/4 of everything.
- `estimateProjectedContextTokens(projection, branch)` (`CMP:227-262`):
  - Trusts the usage only when its source entry comes after the latest `context_edit`/`compaction` on the branch.
  - Otherwise: the replayed current system message once, plus chars/4 of every non-system message.
- `estimateTokens`: `ceil(chars/4)`.
  - system: content + section strings + `JSON(toolsAdded)`.
  - user, custom, toolResult: text, with 4800 chars per image.
  - assistant: text + thinking + toolCall `name + JSON(args)`.
  - bashExecution: command + output.
  - summaries: summary text.

**Cut point** (`findProjectedCutPoint`, `CMP:789-857`). It works over **projected** entries in `[boundaryStart, end)`:

- Valid cut points: entries whose projected messages include user, assistant, bashExecution, custom, branchSummary or compactionSummary. **toolResult never**, and compaction entries never (`CMP:351-364`).
- Walk backwards, summing projected-message tokens and skipping zero-token entries. At the first entry where the sum reaches `keepRecentTokens`, the cut is the first valid cut point ≥ that index, else the last cut point.
- No valid cut points → `firstKept = startIndex`, which means nothing to summarize.
- Budget never reached → cut = the first cut point; everything is kept.
- Recovery-omission advance (`CMP:818-843`). The cut moves one entry forward when **all** of these hold:
  - the budget was exceeded;
  - the suffix after the cut contains an omitted assistant attempt;
  - every intrinsically visible suffix entry is omitted;
  - no `context_edit` with a non-null replacement in the suffix targets an entry that is not omitted.
- The cut then moves backwards over adjacent entries that have no projected messages (metadata), stopping at a compaction or a visible entry (`CMP:845-849`). This keeps state entries such as model changes with the kept tail.
- `isSplitTurn`: the cut entry is not a turn start, and a turn start exists earlier in range. Turn starts are user, bashExecution, custom, branchSummary and compactionSummary; **assistant is not** (`CMP:366-379`, `CMP:850-856`).

**`prepareCompaction(pathEntries, settings)`** (`CMP:859-923`):

- `undefined` if the last path entry is already a compaction (manual compact reports "Already compacted": `AS:2845-2851`).
- `projection = buildSessionProjection(path)`. `prevCompactionIndex` = the projected compaction with non-empty messages, which is always index 0 when present. `previousSummary` = its summary, and `boundaryStart = prevCompactionIndex+1`. So entries kept by the previous compaction are eligible to be summarized again.
- `tokensBefore = estimateProjectedContextTokens(...)`.
- `historyEnd = isSplitTurn ? turnStartIndex : firstKeptIndex`.
- `messagesToSummarize` = projected messages in `[boundaryStart, historyEnd)`. `turnPrefixMessages` = `[turnStart, firstKept)`. Both exclude compaction entries and **system messages** (`CMP:98-102`).
- Both empty → `undefined`.
- `fileOps`: carried from the previous compaction's `details` when it is not `fromHook`, plus tool calls in the summarized messages and turn prefix (`CMP:60-88`, `CMP:903-911`). The tool calls come from `read`/`write`/`edit` with a string `args.path`, including `toolResult.nestedCalls` (`UT:30-61`).

**`compact(prep, model, ...)`** (`CMP:952-1057`):

- Split turn with a prefix:
  - the history summary (only when `messagesToSummarize` is non-empty; otherwise `previousSummary ?? "No prior history."`), plus
  - the turn-prefix summary (separate prompt, `CMP:929-942`), merged as `${history}\n\n---\n\n**Turn Context (split turn):**\n\n${prefix}`.
  - Usage is combined.
- Otherwise a single `generateSummaryWithUsage`.
- Then the summary gets `formatFileOperations(readFiles, modifiedFiles)` appended (`<read-files>` / `<modified-files>` blocks; read-only = read minus modified; lists sorted) (`UT:67-87`).
- `details = {readFiles, modifiedFiles}`.

**Summary request** (`CMP:507-753`):

- System prompt `SUMMARIZATION_SYSTEM_PROMPT` (`UT:161-163`).
- A single user message:
  - `<conversation>\n${serializeConversation(convertToLlm(msgs))}\n</conversation>\n\n`;
  - then `<previous-summary>…</previous-summary>\n\n` if one exists;
  - then `SUMMARIZATION_PROMPT` (sections Goal / Constraints & Preferences / Progress (Done/In Progress/Blocked) / Key Decisions / Next Steps / Critical Context), or `UPDATE_SUMMARIZATION_PROMPT` when there is a previous summary;
  - plus `\n\nAdditional focus: ${custom}`.
- `serializeConversation` (`UT:120-155`) produces `[User]: …`, `[Assistant]: <text>`, `[Assistant tool calls]: name(k=JSON(v), …); …` and `[Tool result]: <truncated to 2000 chars + "[... N more characters truncated]">`. **Thinking is omitted.** System messages are dropped implicitly because there is no case for them.
- Options: `reasoning = thinkingLevel` when the model reasons and the level is not off. `cacheRetention: "none"`. A fresh `sessionId` is used when none is given (`CMP:595-638`).
- Wrapped in `retryAssistantCall` (`CMP:618-638`).
- Fails (throws) on: `stopReason` `error`; `length` (a partial summary must never become a checkpoint); any `toolCall` block (`CMP:585-593`, `CMP:742-748`).

**Recorded fields**: `summary`, `firstKeptEntryId`, `tokensBefore`, `details`, `usage`, `fromHook` (extension-supplied), `systemMessage` (`SM:1272-1284`). `estimatedTokensAfter` is runtime-only (`AS:3240`).

**Boundary drafts**: extension `turn_end` boundaries can append `compaction` drafts, always with `fromHook=true`. `tokensBefore` is recomputed from the projection (`AS:965-978`).

### 3.9 Branch summarization (`BS`)

- Budget: `contextWindow (default 128000) - reserveTokens`. `reserveTokens` comes from `branchSummary.reserveTokens`, default 16384 (`BS:311-313`, `src/core/settings-defaults.ts:14-17`).
- `prepareBranchEntries` (`BS:195-247`):
  - Pass 1 collects file operations from **all** non-`fromHook` `branch_summary.details`, even ones over budget.
  - Pass 2 walks newest→oldest. It converts entries with `getMessageFromEntry`, which **skips toolResults** and turns compaction and branch_summary entries into summary messages (`BS:156-180`). It extracts file ops and stops when the budget would be exceeded. A summary entry is still squeezed in when the total is below 90% of the budget.
  - Note: this path uses raw entries. It ignores `context_edit` and does not route system messages specially. [INFERENCE: system-role `message` entries pass through `getMessageFromEntry` and are dropped later by `serializeConversation`.]
- Prompt: `<conversation>…</conversation>\n\n` plus `BRANCH_SUMMARY_PROMPT` (Goal / Constraints / Progress / Key Decisions / Next Steps; **no** Critical Context). With `replaceInstructions`, the custom text replaces the prompt; otherwise it is appended as "Additional focus" (`BS:258-285`, `BS:328-337`).
- The result is prefixed with the preamble `"The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n"`, and file lists are appended (`BS:253-256`, `BS:367-373`).
- An empty message list → `"No content to summarize"`. Abort → `{aborted}`. Error, length stop or tool call → `{error}`; it does not throw (`BS:315-317`, `BS:356-365`).

### 3.10 Settings manager (only the session-relevant parts)

- Two JSON files, global `<agentDir>/settings.json` and project `<cwd>/.pi/settings.json`. They are deep-merged with project winning; arrays replace except `defaultTools` modifiers (`settings-manager.ts:65-147`, `settings-manager.ts:310-321`).
- Writes:
  - Only fields marked modified are written. Each write is a read-modify-write of the current file under `proper-lockfile` (10 attempts at 20 ms) (`settings-manager.ts:197-252`, `settings-manager.ts:598-627`).
  - Writes go through a serialized promise queue; `flush()` awaits it (`settings-manager.ts:576-588`, `settings-manager.ts:670-672`).
  - Project writes require a trusted project (`settings-manager.ts:555-559`).
  - Load errors disable saving for that scope (`settings-manager.ts:632-634`).
- Session-relevant keys: `compaction.{enabled,reserveTokens,keepRecentTokens,modelOverrides}`, `branchSummary.{reserveTokens,skipPrompt}`, `sessionDir`, `retry` (`src/core/settings-schema.ts:13-42`, `src/core/settings-schema.ts:242-243`, `src/core/settings-schema.ts:389-392`).

### Persisted contract vs runtime

- **Persisted (format contract):**
  - the header and all 11 entry shapes;
  - the id/parentId tree;
  - `firstKeptEntryId` semantics (including self-id = retain none);
  - `systemMessage` checkpoint;
  - `fromHook` and `details` conventions;
  - context_edit last-wins and branch-relative semantics;
  - label and session_info last-wins in file order;
  - file naming and directory encoding.
- **Runtime only:**
  - the leaf pointer (derived as the last line on load);
  - projection and message objects, including the `compactionSummary`/`branchSummary`/`custom` roles;
  - `SessionInfo`;
  - `estimatedTokensAfter`;
  - overflow-recovery flags;
  - compaction reasons and events;
  - the `entryIdsByMessage` map (`AS:1216-1237`);
  - prefix/suffix wrapper text (`MSG:11-24`), applied at convert time and never stored.

## 4. Skip list

- Pre-v3 migrations (`SM:286-347`). axo starts its own format at v1 with no compatibility (`plan/design.md:27`).
- `SessionInfo.allMessagesText`, progress callbacks, partial-publish intervals and 10/64 concurrency tuning (`SM:884-1000`): picker UI.
- `listAll` across projects with `onProgress` overloads (`SM:1921-2012`): TUI `/resume`.
- `getTree()` timestamp sorting and `labelTimestamp` (`SM:1529-1567`): `/tree` renderer. Keep a minimal children index instead.
- `display` on custom messages: a TUI rendering flag, though it is persisted. [INFERENCE: keep the field for format fidelity only if TS extensions rely on it.]
- `bashExecution` `!`/`!!` user shell messages (`MSG:29-89`, `AS:3957-3992`): interactive-mode feature.
- Virtual-model routing and `pi.virtual-model-state` custom entries (`AS:814-842`, `docs/session-format.md:174`).
- Cache warming `usage` kind `cache_warm` (`src/core/sdk.ts:414-421`). Keep `UsageEntry` itself as generic accounting.
- Codemode `nestedCalls` file-op extraction (`UT:31-35`): only if axo grows codemode.
- HTML export, `/share`, `/bug` (`src/core/session-export.ts`, `docs/sessions.md:56-66`).
- `findCutPoint`/`findTurnStartIndex` over raw entries (`CMP:412-501`). These are exported but unused in `src`; the projected variant is canonical.
- `generateSummary` (text-only wrapper) and the unused `_reserveTokens` parameter (`CMP:644-679`): legacy API.
- `getLastAssistantUsage(entries)` (`CMP:166-175`): unused in `src`.
- `branchSummary.skipPrompt`, `treeFilterMode`, and the rest of the TUI settings in the settings manager.
- `proper-lockfile` busy-wait (`settings-manager.ts:214-217`): use a proper file lock if settings are ported at all.
- Missing-cwd prompt text (`src/core/session-cwd.ts:35-42`). Keep only the error type.

## 5. Rust port notes

- **Format types.** Use `#[serde(tag = "type", rename_all = "snake_case")] enum FileLine { Session(Header), Message{..}, ThinkingLevelChange{..}, ModelChange{..}, Usage{..}, Compaction{..}, BranchSummary{..}, Custom{..}, CustomMessage{..}, ContextEdit{..}, Label{..}, SessionInfo{..} }` with a shared `EntryBase { id, parent_id: Option<EntryId>, timestamp }` via `#[serde(flatten)]`.
  - Use `serde_json::Value` (or `Box<RawValue>`) for `details` and `data`.
  - Unknown `type` values: [INFERENCE] pi drops nothing on unknown types because it is untyped JSON. Add an `Unknown(Value)` variant via a custom deserializer so a newer file round-trips, rather than silently losing lines.
- **Ids.** `EntryId` should be a small inline string, e.g. `compact_str`/`smol_str`, since they are 8 hex chars. Session ids are `uuid::Uuid::now_v7()`.
- **In-memory store.** Keep:
  - `Vec<Entry>` in file order;
  - `HashMap<EntryId, u32>` from id to index;
  - `leaf: Option<u32>`;
  - a labels map.
  Avoid pi's per-call `getEntries()` copy and repeated path rebuilds (`SM:1494`, `SM:1520-1522`); `buildSessionProjection` runs on every request (`AS:793`). [INFERENCE] Cache the projection keyed by `(leaf, entry_count)`, or update it incrementally when appending to the current leaf. Memory per session matters (`plan/design.md:19`).
- **Projection API.** A pure function `project(entries, leaf) -> Projection { entries: Vec<(EntryIdx, Range<usize>)>, messages: Vec<AgentMessage>, thinking_level, model }`. Borrowing `Cow<'_, AgentMessage>` avoids cloning unedited messages [INFERENCE]. Keep it a free function, as pi does, so compaction and previews can run it on a slice.
- **Storage trait** (design: "Storage behind a trait"): `append(&Entry)`, `rewrite(&[FileLine])`, `load() -> Vec<FileLine>`. JSONL impl:
  - buffered reader; skip malformed lines but report a count;
  - newline repair on a torn tail;
  - lazy create with `OpenOptions::create_new(true)`;
  - append with `O_APPEND` and one `write_all` per line.
  - [INFERENCE] Do migration/rewrites atomically (tempfile + rename). Optionally `sync_data` after compaction entries, which are expensive to regenerate.
- **Leaf durability.** Decide explicitly. pi loses pure leaf moves on restart (`SM:1111`). [INFERENCE] Either accept that, or persist the leaf as a tiny state entry. Persisting changes the format and makes the "leaf = last line" rule false, so choose before v1.
- **Compaction module.** Split it like pi:
  - pure `prepare_compaction(&[Entry], &Settings) -> Option<Preparation>`;
  - async `compact(prep, &dyn Provider, opts) -> Result<CompactionResult>`;
  - `SessionManager::append_compaction`.
  Keep the hook seam between prepare and compact: `session_before_compact` may cancel or replace the result (`AS:3184-3204`). This is the extension point the core must expose, together with `session_compact`, `session_compact_failed`, `session_before_tree`, `session_tree` and `session_before_fork` (`src/core/extensions/types.ts:778-860`, `src/core/extensions/types.ts:1511-1520`).
- **Pitfalls to preserve:**
  - never cut at a toolResult;
  - drop system messages from the kept tail when the compaction carries `systemMessage` (`SM:506`);
  - an older compaction inside the kept range contributes nothing (`SM:562`);
  - edits are applied only from selected entries;
  - stale-usage guard: do not re-trigger on assistant messages timestamped ≤ the latest compaction (`AS:3027-3035`);
  - `length` stop is a failed summary;
  - a summary containing tool calls is an error;
  - the recovery path appends `context_edit(null)` for the failed attempt *and its tool results* before compacting, so no orphaned tool result reaches the provider (`AS:1244-1272`).
- **Timestamps.** Entries use ISO-8601 strings; messages use epoch ms. Use `jiff` or `time` with millisecond RFC 3339 output to match `toISOString()` (`...sssZ`) [INFERENCE].
- **Directory encoding.** `--{cwd.trim_start_matches(['/','\\']).replace(['/','\\',':'],"-")}--` (`SM:592`). This is lossy (`a-b` and `a/b` collide). That is acceptable because the header `cwd` is authoritative and listing filters on it for custom directories.
- **Concurrency.** pi has no session-file lock. Two processes appending to the same file would interleave lines. [INFERENCE] For axo, with many sessions in one process, a per-session `Mutex` around the writer is enough. Cross-process locking (`fs4`/`fd-lock`) only if the ACP binary and the library may share files.
- **Crates:** `serde`/`serde_json`, `uuid` (v7), `rand` (short ids), `tokio::fs` or blocking std I/O on a dedicated thread (writes are tiny), `tempfile` (atomic rewrite), `jiff`.
- **Branch summary and compaction share** `serialize_conversation`, file-op tracking and summary-request plumbing. Put them in one `summarize` module. The prompts are static `&str`.

## 6. Size (non-test TS lines)

| File | Lines | Port relevance |
|---|---|---|
| `src/core/session-manager.ts` | 2013 | core; ~45% is listing/discovery/legacy |
| `src/core/compaction/compaction.ts` | 1104 | core; ~250 lines are prompts/legacy wrappers |
| `src/core/compaction/branch-summarization.ts` | 380 | core |
| `src/core/compaction/utils.ts` | 163 | core |
| `src/core/compaction/index.ts` | 7 | re-exports |
| `src/core/messages.ts` | 196 | core (summary/custom message roles, convertToLlm) |
| `packages/ai/src/utils/transcript.ts` | 263 | system-message replay used by compaction checkpoint |
| `src/core/session-cwd.ts` | 59 | minor |
| `src/core/settings-manager.ts` | 1443 | mostly UI settings; ~150 lines relevant |
| `src/core/settings-schema.ts` / `settings-defaults.ts` | 482 / 67 | few keys relevant |
| `src/core/agent-session.ts` (compaction/tree/persist parts) | ~700 of 4458 | runtime orchestration: `AS:766-845`, `1146-1272`, `2782-2973`, `2990-3304`, `4024-4216` |
| `src/core/agent-session-runtime.ts` (fork) | ~90 of 447 | fork/clone |
| docs: `session-format.md` / `compaction.md` / `sessions.md` | 293 / 463 / 66 | spec reference |
