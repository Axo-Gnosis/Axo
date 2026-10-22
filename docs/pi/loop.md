# pi core loop + session orchestration (pi v1.3.0)

Paths are relative to `/tmp/pi-src/packages/`. `A/` = `agent/src/`, `S/` = `coding-agent/src/core/`, `X/` = `coding-agent/src/core/extensions/`, `AI/` = `ai/src/`.

---

## 1. Concepts

- **Two layers.** `Agent` (pi-agent-core) is a provider-agnostic, stateful wrapper around a pure loop function (`runLoop`). It owns the transcript, the steering and follow-up queues, the abort controller, and the event fan-out (`A/agent.ts:189-194`). `AgentSession` (coding-agent) wraps one `Agent` and adds persistence, extensions, compaction, retry, model and thinking management, prompt expansion, and user bash (`S/agent-session.ts:1-14`). It reaches the agent **only through the agent's public hook fields**, which it chain-wraps at construction (`S/agent-session.ts:511-519`).
- **Turn** = one assistant response plus the execution of its tool calls (`A/types.ts:520`). **Run** = `agent_start … agent_end`, i.e. one call of the loop. AgentSession adds a **settled** phase on top: one user prompt can span several runs (retry, overflow recovery, queued messages, `agent_before_settle` continuation) before `agent_settled` fires (`S/agent-session.ts:1898-1917`).
- **AgentMessage ⊇ LLM Message.** The transcript holds app-level messages, including custom roles. They are lowered to provider `Message[]` only at the LLM-call boundary, via `transformContext` and then `convertToLlm` (`A/agent-loop.ts:1-4, 397-406`).
- **System messages live in the transcript** (new in this pi). The leading `system` message is the prompt. Later system messages patch it (`content` appends, `sections` replace/remove named sections, `toolsAdded`/`toolsRemoved` change the tool set). Replaying them in order gives the current prompt and tools (`AI/types.ts:542-568`). The loop computes tool-loadout deltas itself, before every request (`A/agent-loop.ts:332-372`).
- **Canonical state is the session tree.** Before every request, AgentSession replaces the loop's context with `sessionManager.buildSessionProjection().messages` (`S/agent-session.ts:787-811`), and it re-syncs `agent.state.messages` from the projection after every out-of-band append (`S/agent-session.ts:938-944`). Agent state is therefore a cache of the session; persistence happens on `message_end`.

## 2. Types (trimmed)

### Loop config and hooks (`A/types.ts`)
```ts
type StreamFn = (model, context: TranscriptContext, opts?) => AssistantMessageEventStream | Promise<…>; // :36-40, must NOT throw; failures encoded as final msg stopReason "error"|"aborted" (:27-35)
type ToolExecutionMode = "sequential" | "parallel";            // :50
type QueueMode = "all" | "one-at-a-time";                       // :58
type AgentToolCall = Extract<AssistantMessage["content"][number], {type:"toolCall"}>; // :61 {type,id,name,arguments}
interface BeforeToolCallResult { block?; reason?; terminate? }  // :69-77
interface AfterToolCallResult { content?; details?; structuredContent?; isError?; usage?; terminate? } // :96-107  (field-wise replace, no deep merge)
interface BeforeToolCallContext { assistantMessage; toolCall; args /*validated*/; context: AgentContext } // :110-119
interface AfterToolCallContext  { …same; result: AgentToolResult; isError } // :122-135
interface AgentTurnContext { message: AssistantMessage; toolResults: ToolResultMessage[]; context; newMessages } // :138-147
type AgentTurnDecision = {action:"continue"} | {action:"end"};  // :150
type FinishTurn = (turn, signal?) => AgentTurnDecision | void | Promise<…>; // :158-161
interface AgentLoopTurnUpdate { context?; messages?; model?; thinkingLevel? } // :164-173
type PrepareRequest = ({context, model, thinkingLevel}, signal?) => Omit<AgentLoopTurnUpdate,"messages"> | void; // :176-190
interface AgentLoopConfig extends SimpleStreamOptions {        // :193-342
  model; convertToLlm(msgs: AgentMessage[]): Message[] | Promise;   // must not throw
  transformContext?(msgs, signal?): Promise<AgentMessage[]>;        // must not throw
  getApiKey?(provider): string|undefined|Promise;                   // per request (expiring OAuth tokens)
  finishTurn?; prepareRequest?; prepareNextTurn?(ctx: PrepareNextTurnContext): AgentLoopTurnUpdate|undefined;
  getSteeringMessages?(): Promise<AgentMessage[]>; getFollowUpMessages?(): Promise<AgentMessage[]>;
  toolExecution?: ToolExecutionMode /*default "parallel"*/; beforeToolCall?(ctx, signal?); afterToolCall?(ctx, signal?);
}
type ThinkingLevel = "off"|"minimal"|"low"|"medium"|"high"|"xhigh"|"max"; // :349
interface CustomAgentMessages {}   // declaration-merging extension point :365-367
type AgentMessage = Message | CustomAgentMessages[keyof CustomAgentMessages]; // :374
```

### Tool contract (`A/types.ts:423-499`)
```ts
interface AgentToolResult<T> {
  content: (TextContent|ImageContent)[];  // model-facing
  details: T;                             // UI/log only, persisted on ToolResultMessage.details, never sent to model
  structuredContent?: JsonValue;          // matches outputSchema; for programmatic callers, not the model
  usage?: Usage;                          // tool's own LLM usage (e.g. subagent); not context accounting
  isError?: boolean;                      // fail without throwing; details kept
  terminate?: boolean;                    // stop after batch iff ALL results in batch set it
}
type AgentToolUpdateCallback<T> = (partial: AgentToolResult<T>) => void; // calls after settle are ignored :457-463
interface AgentTool<P, D> extends Tool<P> /* name, description, parameters (TypeBox JSON schema) */ {
  label: string;
  prepareArguments?(raw: unknown): Static<P>;    // compat shim before validation
  outputSchema?: TSchema;
  execute(toolCallId, params: Static<P>, signal?, onUpdate?): Promise<AgentToolResult<D>>; // throw OR isError:true
  replay?: "never" | "safe";                    // recovery policy; not used by loop
  executionMode?: ToolExecutionMode;            // per-tool override
}
interface AgentToolCallOutcome { toolCall; result; isError; durationMs? } // :449-455
interface AgentContext { messages: AgentMessage[]; tools?: AgentTool[] }  // :502-507
```
The session-level `ToolDefinition` (`X/types.ts:579-668`) adds `promptSnippet`, `promptGuidelines`, `exposure: "direct"|"model-only"|"codemode"|"deferred"|"hidden"` (`:516`), `namespace`, `annotations` (MCP hints), `defaultActive`, `prepareLoadout`, `reachesTools`, `constrainedSampling`, render fns (TUI). It also takes a **5th `execute` arg `ctx: ExtensionToolContext`**, which has `tools` and `executeTool(name,args,opts) → AgentToolCallOutcome` (`X/types.ts:383-395, 650-656`).

### Events (`A/types.ts:516-539`)
```ts
type AgentEvent =
 | {type:"agent_start"} | {type:"agent_end"; messages: AgentMessage[] /* new msgs of this run */}
 | {type:"turn_start"} | {type:"turn_end"; message; toolResults: ToolResultMessage[]}
 | {type:"message_start"; message} | {type:"message_update"; message; assistantMessageEvent} /* assistant only */
 | {type:"message_end"; message}
 | {type:"tool_execution_start"; toolCallId; toolName; args}
 | {type:"tool_execution_update"; toolCallId; toolName; args; partialResult}
 | {type:"tool_execution_end"; toolCallId; toolName; result; isError; durationMs?};
```
Session adds (`S/agent-session.ts:196-237`): `agent_end{willRetry}`, `agent_settled{aborted}`, `queue_update{steering[],followUp[]}`, `compaction_start/end`, `entry_appended{entry}`, `session_info_changed`, `thinking_level_changed`, `auto_retry_start/end`, `summarization_retry_*`, `bash_execution_update{id?,delta}`. Tool events may also carry `parentToolCallId` (`:189-193`).

### Agent state (`A/types.ts:382-421`, `A/agent.ts:72-77`)
`systemPrompt` (derived from the transcript), `model`, `thinkingLevel`, `tools` (setter copies), `messages` (setter copies), `isStreaming`, `streamingMessage?`, `pendingToolCalls: Set<id>`, `errorMessage?`.

### LLM messages (`AI/types.ts`)
`SystemMessage{content, sections?, toolsAdded?, toolsRemoved?, timestamp}` (:552). `UserMessage{content: string|(Text|Image)[]}` (:570). `AssistantMessage{content:(Text|Thinking|ToolCall)[], api, provider, model, usage, stopReason, errorMessage?, thinkingLevel?, durationMs?, timestamp…}` (:576-607). `StopReason = "pending"|"stop"|"length"|"toolUse"|"error"|"aborted"|"deferred"` (:480). `ToolResultMessage{toolCallId, toolName, content, details?, usage?, nestedCalls?, isError, timestamp, durationMs?}` (:631-648).

### Coding-agent custom messages (`S/messages.ts:30-78`)
`bashExecution{command, output, exitCode, cancelled, truncated, fullOutputPath?, excludeFromContext?}`, `custom{customType, content, display, details?}`, `branchSummary{summary, fromId}`, `compactionSummary{summary, tokensBefore}`. All registered via declaration merging (`:71-78`).

## 3. Behavior

### 3.1 Loop algorithm (`A/agent-loop.ts`)
Entry points:
- `runAgentLoop(prompts, ctx, cfg, emit, signal, streamFn)` (`:111-135`):
  1. Run `declareToolChanges` on the prompts.
  2. Emit `agent_start`, `turn_start`, then `message_start`+`message_end` for each prompt message.
  3. Call `runLoop`.
  4. Return `newMessages`, which include the prompts.
- `runAgentLoopContinue` (`:137-160`): throws if the context is empty or the last message is `assistant`. Emits `agent_start` and `turn_start` only, with no new messages.

`runLoop` (`:172-330`):
0. `pending = await getSteeringMessages()` once at start (`:185`).
1. **Outer** `while(true)`; **inner** `while (hasMoreToolCalls || pending.length)` (`:188-192`).
2. If it is not the first turn of this loop:
   1. Call `prepareNextTurn(lastCompletedTurn)`. It may swap the context, model, or thinking level, or return `messages` to inject (`:194-209`). `thinkingLevel "off"` maps to `reasoning: undefined` (`:202-207`).
   2. **Re-poll steering only if `pending` is empty**, because preparation (e.g. compaction) can be long. This prevents one-at-a-time mode from delivering two messages in one turn (`:210-215`).
   3. Emit `turn_start` (`:216`).
3. Inject `[...prepared, ...pending]` through `declareToolChanges`. For each one: emit `message_start`/`message_end`, then push it to the context and to `newMessages` (`:219-226`).
4. `prepareRequest({context, model, thinkingLevel}, signal)` may replace the context, model, or thinking level for this and later requests (`:228-248`).
5. `streamAssistantResponse` (`:390-479`):
   1. `transformContext(messages, signal)` → `convertToLlm` → `normalizeContext` (`:397-406`).
   2. `getApiKey(provider) || config.apiKey`, called **every request** (`:409-410`).
   3. Call `streamFn`.
   4. On `start`: push the partial into `context.messages` and emit `message_start` (a copy).
   5. On each delta: replace the last element and emit `message_update` with a **shallow copy** (`:426-451`).
   6. On `done`/`error`: replace the element with `await response.result()` stamped with `thinkingLevel` (`:419`). Emit `message_start` only if no partial was ever added, then `message_end` (`:453-466`). A stream that ends without a terminal event takes the same path (`:470-478`).
6. If `stopReason ∈ {error, aborted}`: call `finishTurn` (its decision is ignored), then emit `turn_end{toolResults:[]}` and `agent_end`, and **return**. This is a hard exit; queues are not polled (`:254-265`).
7. Collect `toolCalls` from `content` (`:268`). If there are any:
   - If `stopReason === "length"`, **fail all of them without executing**. Each gets `tool_execution_start`, `tool_execution_end{isError}` and a result message with the text "…hit the output token limit… Re-issue…" (`:273-279, 488-513`).
   - Otherwise call `executeToolCalls` (§3.2).
   - Push the results to the context and to `newMessages`. Set `hasMoreToolCalls = !batch.terminate` (`:280-286`).
8. Call `finishTurn(turn)`, then emit `turn_end{message, toolResults}` (`:289-296`). Ordering: **finishTurn runs before `turn_end`**.
9. If the decision is `end`: emit `agent_end` and return (`:298-301`).
10. `explicitContinuation = decision==="continue"`. Poll steering. If there are tool results or steering, clear `explicitContinuation`, because a natural next request satisfies it (`:303-307`).
11. After the inner loop, poll follow-ups. If there are any, `pending = followUps` and loop again (`:311-317`).
12. Otherwise, if `explicitContinuation` is set, run one context-only turn (`:320-323`).
13. Otherwise emit `agent_end{messages:newMessages}` (`:329`).

**Stop conditions.** The loop stops when the response has no tool calls, no steering is queued, and no follow-ups are queued. It also stops when a batch has every result `terminate:true` and no steering or follow-ups remain, on `stopReason` error/aborted, or on `finishTurn → end`. There is no max-turns limit in the core.

**Tool-loadout declaration** (`:342-384`). Before every injection the loop compares the tools the transcript declares (`getCurrentTools`) with the executable `context.tools`:
- If they differ and the pending batch has no system message, it inserts `{role:"system", content:"", toolsAdded, toolsRemoved}` before the first non-system pending message.
- If the batch has a system message, it overwrites that message's tool fields with the real delta.

The transcript therefore always replays to exactly `context.tools`.

### 3.2 Tool execution (`A/agent-loop.ts:518-959`)
- **Mode selection.** Execution is sequential if `config.toolExecution==="sequential"` **or any** call in the batch targets a tool with `executionMode:"sequential"` (`:525-532`). Default is parallel (`A/agent.ts:262`).
- **prepareToolCall** (`:718-787`):
  1. Look the tool up by name. Unknown → immediate error "Tool X not found".
  2. `prepareArguments`, then `validateToolArguments` (schema), then `beforeToolCall`.
  3. If aborted after the hook → "Operation aborted". If `block` → error with `reason || "Tool execution was blocked"`, carrying `terminate` when set.
  4. **Any throw** (validation, hook) becomes an error result: the pipeline fails closed.
- **executePreparedToolCall** (`:831-867`):
  - Calls `execute(id, args, signal, onUpdate)`. `onUpdate` pushes `tool_execution_update` promises. After settle, updates are dropped (`acceptingUpdates=false`) and pending update emits are awaited **before** finishing.
  - `durationMs` is measured with `performance.now()`.
  - A throw becomes `{content:[text: err.message], details:{}}` with `isError:true`.
  - `isError = result.isError === true`.
- **finalizeExecutedToolCall** (`:869-920`):
  - `afterToolCall` overrides apply field by field.
  - If `content` is replaced without `structuredContent`, the structured content is **dropped**.
  - An `afterToolCall` throw becomes an error result.
- **Sequential** (`:540-594`). Per call: `tool_execution_start` → prepare → execute → finalize → `tool_execution_end` → toolResult `message_start/end`. Then `break` if aborted; **remaining calls get no result message**.
- **Parallel** (`:596-670`):
  1. **Preparation is sequential**: `tool_execution_start` and `prepareToolCall` (including `beforeToolCall`) in source order. Immediate outcomes emit `tool_execution_end` right away.
  2. Prepared calls become thunks, run together with `Promise.all`. Each thunk re-checks abort first ("Operation aborted").
  3. `tool_execution_end` is emitted **in completion order**.
  4. ToolResult `message_start/end` are emitted **afterwards, in assistant source order**.
  5. An abort during preparation `break`s, so later calls get neither start events nor results.
- **Result shaping** (`:940-954`): `ToolResultMessage{toolCallId, toolName, content: result.content ?? [], details, usage, isError, durationMs?, timestamp}`. `structuredContent` is **not** copied into the message.
- **Orphan safety net.** Orphaned tool calls (left by abort, or by a run that ended before results) get synthetic `"No result provided"` error results at provider-transform time. Errored or aborted assistant messages are skipped entirely when building the request (`AI/api/transform-messages.ts:179-216`).
- **`runToolCall(toolCall, {tools, assistantMessage, context, hooks, signal, onUpdate})`** (`:800-829`) runs the same prepare → execute → finalize pipeline with **no events and no messages**. It is used for nested tool calls (§3.8).

### 3.3 Steering vs follow-up
- Each queue is a `PendingMessageQueue`. `drain()` takes all messages in `"all"` mode, otherwise only the oldest (`A/agent.ts:156-186`). Both queues default to `"one-at-a-time"` (`:257-258`). The session sets the modes from settings (`S/sdk.ts:433-434`).
- **Steering is polled**:
  - at loop start (`agent-loop.ts:185`);
  - after each turn's tools finish and after `finishTurn` (`:304`);
  - in `prepareNextTurn`, but only if the earlier poll was empty (`:213-214`).

  Steering is therefore injected **after the current assistant message's tool calls all complete** and before the next LLM request. It never interrupts tool execution and never skips the remaining tool calls (`A/types.ts:285-292`).
- **Follow-up** is polled only when the inner loop would exit: no tool calls and no steering (`agent-loop.ts:311`).
- `continue()` from an assistant tail first drains steering. If there is any, it runs as a prompt with `skipInitialSteeringPoll`, so the first poll does not drain a second message. Otherwise it drains follow-ups, and otherwise throws (`A/agent.ts:403-417, 506-513`).
- `peekQueuedMessages()` returns steering if non-empty, else follow-ups (`:339-342`).
- **Session layer.** It keeps parallel string mirrors `_steeringMessages/_followUpMessages` for UI. On `message_start` of a user message, it removes the first text match and emits `queue_update` (`S/agent-session.ts:1122-1140`).
- `prompt()` while streaming **requires** `streamingBehavior` and throws otherwise (`:2137-2142`).
- Extension commands cannot be queued (`:2332-2342`).
- `sendCustomMessage` delivery (`:2357-2393`):
  - `deliverAs:"nextTurn"` → prepended with the next user prompt (`:1959-1961`).
  - streaming and `triggerTurn !== false` → `agent.steer` or `agent.followUp`.
  - idle with `triggerTurn` → starts a run.
  - streaming without a trigger → buffered in `_pendingCustomMessages` and flushed at `turn_end` (`:1195-1197`), so a message never lands between a tool call and its result.

### 3.4 Abort / cancellation
- `Agent.abort()` aborts the run's `AbortController` (`A/agent.ts:350-352`). The signal is passed to: `streamFn`, `transformContext`, `prepareRequest`, `finishTurn`, `before/afterToolCall`, `tool.execute`, and listeners (`processEvents` passes the signal, `:615-621`).
- **Mid-stream.** The `StreamFn` contract requires the stream to end with a final assistant message with `stopReason:"aborted"`, not to throw (`A/types.ts:27-35`). The loop then hard-exits with `turn_end` and `agent_end` (`agent-loop.ts:254-265`). The aborted assistant message **is** in the transcript and is persisted.
- **Mid-tool.** Tools must honor `signal`. After a tool returns, sequential mode stops the batch (`:585-587`). Parallel mode gives not-yet-started thunks "Operation aborted" (`:630-638`) and breaks preparation (`:651-653`). The loop then calls the next `streamAssistantResponse`. [INFERENCE] With an aborted signal the provider immediately returns `aborted`, which ends the run.
- **Unexpected throw in the loop** (contract violation by a hook). `runWithLifecycle` catches it and synthesizes an assistant failure message (`stopReason` aborted/error, empty text), emitting `message_start/end`, `turn_end`, `agent_end` (`A/agent.ts:533-558`). `finishRun` clears `isStreaming`, `pendingToolCalls`, `activeRun` **after** the `agent_end` listeners settle (`:560-566`, `waitForIdle` `:359-361`).
- **Session abort** (`S/agent-session.ts:2498-2508`):
  1. Sets `_agentRunAbortRequested`.
  2. Aborts retry sleep, compaction, and branch summary, and flags before-settle.
  3. Calls `agent.abort()`, then `waitForIdle()`.

  An abort requested before the run exists (e.g. during `before_agent_start`) is applied at `agent_start` (`:1104-1106`). The post-run loop checks the flag between every step (`:1900-1909`). `dispose()` aborts everything, including user bash (`:1412-1433`).

### 3.5 AgentMessage → LLM (`S/messages.ts:150-196`, `S/sdk.ts:296-331, 428-432`)
- **Order per request.** `transformContext` (session chain: extension `context` → `context_with_system` → hidden-declarations filter → forced-system-prompt projection; `S/agent-session.ts:1787-1822`, `X/runner.ts:1305-1366`), then `convertToLlm`, then `normalizeContext`.
- **Lowering of custom roles:**
  - `bashExecution` → user text `` Ran `cmd`\n```out``` ``, plus "(command cancelled)" / "Command exited with code N" / truncation note. Dropped when `excludeFromContext` (`!!`) (`messages.ts:83-99, 157-167`).
  - `custom` → user (string becomes a text block).
  - `branchSummary` / `compactionSummary` → user, wrapped in fixed prefix/suffix `<summary>` text (`:10-25`).
  - `system/user/assistant/toolResult` pass through.
- The default `convertToLlm` in Agent just filters to LLM roles (`A/agent.ts:39-47`).
- The `blockImages` setting replaces images with "Image reading is disabled." (deduped) at convert time (`S/sdk.ts:296-331`).
- **`context` hook** sees non-system messages only. Pi restores the system messages afterwards, and handlers may mutate in place. **`context_with_system`** sees everything and owns the result (`X/runner.ts:1309-1363`). Messages are `structuredClone`d first, so hooks cannot mutate the transcript (`:1307`).

### 3.6 Agent ↔ state reducer
`Agent.processEvents` (`A/agent.ts:575-622`):
- **reduces state first:** `message_start/update` → `streamingMessage`; `message_end` → push to `state.messages`; `tool_execution_*` → `pendingToolCalls`; `turn_end` → `errorMessage`;
- **then awaits listeners sequentially** in subscription order.

Consequences:
- Back-pressure: every event awaits all listeners, so slow listeners stall the loop.
- A `message_end` listener can mutate `event.message` in place and the change sticks in state. The session uses this for `message_end` replacement (`S/agent-session.ts:1286-1300, 1339-1352`).

The loop's `context` is a **snapshot copy** (`createContextSnapshot`, `A/agent.ts:469-474`), separate from `state.messages`.

### 3.7 What AgentSession adds (installs on Agent)
Installed hooks:
- `beforeToolCall` → ext `tool_call`
- `afterToolCall` → ext `tool_result` + image normalization
- `prepareNextTurnWithContext` → threshold compaction + system-prompt/tool-loadout patch
- `prepareRequest` → replace context with the session projection, route virtual models, threshold compaction
- `finishTurn` → ext `turn_end` boundary
- `transformContext` ×2

See `S/agent-session.ts:652-932, 1787-1822`.

**Per-event handler `_handleAgentEvent`** (`:1103-1198`):
1. Attach nested-call records and usage to the toolResult on `message_start`.
2. Dequeue the UI mirrors.
3. **Extension event first** (`_emitExtensionEvent`), then public listeners. `agent_end` gets `willRetry`.
4. **Persistence on `message_end`:**
   - `custom` → `appendCustomMessageEntry`;
   - system/user/assistant/toolResult → `appendMessage`;
   - `bashExecution` and summaries are persisted elsewhere (`:1147-1168`).
5. On assistant `message_end`: record `_lastAssistantMessage`, reset overflow flag; if non-error and retrying, emit `auto_retry_end{success:true}` (`:1170-1187`).
6. On `turn_end`, flush deferred custom messages.

**Run orchestration `_startRun`** (`:1873-1918`):
1. `_isAgentRunActive = true`.
2. `_prepareRun` (`:1926-1977`):
   1. pre-prompt `_checkCompaction(lastAssistant, includeAborted)`;
   2. `before_agent_start` (may add custom messages and force a system prompt);
   3. build `[systemPatch?, user|custom, ...nextTurnMsgs, ...handlerCustomMsgs]`.
3. Flush pending bash and custom messages (they go *before* the prompt).
4. `agent.prompt(messages)`.
5. Loop:
   - `_handlePostAgentRun()` (`:1979-2015`): retry → overflow/threshold compaction → `hasQueuedMessages`.
   - If true, `agent.continue()`.
   - Otherwise `_runBeforeSettleBoundary` (`:2017-2039`). If it asks to continue, `agent.continue()`, else break.
6. `finally`: flush bash and custom messages, then `agent_settled` (`:1078-1100`).
   - `prompt()` and `sendCustomMessage(triggerTurn)` called **during** `agent_settled` are deferred until after it (`:2093-2096, 2379-2381`).

**Auto-retry** (`:3769-3862`):
- Retryable means `isRetryableAssistantError` and **not** context overflow.
- `_prepareRetry`:
  1. `attempt++`, capped at `maxRetries`;
  2. emit `auto_retry_start{attempt,max,delayMs,errorMessage}`;
  3. **durably omit the failed assistant and its tool results from the projection** via `context_edit(target, null)` entries (`_omitRecoveryAttempt`, `:1244-1272`);
  4. abortable sleep with `retryDelayMs(settings, attempt)`.
- The failed message becomes `_failedResponse`, which the next `prepareRequest` passes to the virtual-model router (`:790-791, 823`).
- The counter resets on the first non-error assistant message. Final failure emits `auto_retry_end{success:false}` (`:1998-2006`).
- Provider-level transport retries are separate: `maxRetries`, `maxRetryDelayMs` and timeouts are passed to the stream (`S/sdk.ts:340-358`).

**Auto-compaction triggers** (`:2990-3304`; algorithm details belong to the session report):
- (a) **pre-prompt**, on the last assistant message, aborted included (`:1929-1932`);
- (b) **post-run**, `_checkCompaction(lastAssistant)`:
  - overflow error, or recoverable `length` → omit the attempt, compact, **retry once** (`_overflowRecoveryAttempted` guard, `:3070-3106`);
  - a successful response over the window → compact without retry;
  - threshold → compact without retry, using usage tokens or an estimate when the message has an error or zero usage (`:3109-3143`);
- (c) **mid-run, between turns**: `prepareNextTurn` → `_compactBeforeNextAssistantResponse` (threshold, `:776-785, 905`);
- (d) in `prepareRequest` for virtual models, against the routed model (`:838-841`).

Compaction emits `compaction_start/end` and ext `session_before_compact` (cancel or provide the result) / `session_compact` / `session_compact_failed`. After compaction, `hasQueuedMessages()` → continue (`:3269-3271`). Manual `compact()` aborts the run first (`:2828-2831`). `prompt()` throws during manual compaction (`:2110-2114`).

**Model / thinking switching** (`:2541-2749`). Mutations only take effect on the **next request**: `prepareNextTurn` and `prepareRequest` re-read `agent.state.model/thinkingLevel` (`:804-813, 928-929`).
- `setModel`:
  1. checks auth;
  2. sets `agent.state.model`;
  3. `sessionManager.appendModelChange`;
  4. picks the thinking level: explicit, else per-model setting, else global default;
  5. emits ext `model_select` (if changed).
- `setThinkingLevel`:
  1. clamps to the model's supported levels;
  2. if changed: `appendThinkingLevelChange`, `thinking_level_changed`, ext `thinking_level_select`.

**User bash** (`!cmd`) (`:3901-3993`):
- `executeBash` runs `prefix\ncommand` in the session cwd and streams `bash_execution_update`.
- `recordBashResult` creates a `bashExecution` message. While streaming it is **deferred** (it would break tool_use/tool_result adjacency) and flushed at run end or before the next prompt; otherwise `appendMessage` + refresh immediately.
- The ext `user_bash` hook (may supply `operations` or a full `result`) is emitted by the modes, not the session (`modes/rpc/rpc-mode.ts:562`).

**Prompt expansion** (`prompt()`, `:2092-2175`), in order:
1. **Extension slash command**: runs immediately even while streaming, and consumes the input.
2. ext `input` hook: `continue | transform{text,images} | handled` (`X/types.ts:1184-1187`).
3. `/skill:name args` → `<skill name location>…</skill>\n\nargs` (`:2211-2235`).
4. `/template args` → `substituteArgs`. Supports `$1`, `$@`, `$ARGUMENTS`, `${N:-def}`, `${@:N[:L]}`; non-recursive (`S/prompt-templates.ts:63-108, 309-325`).
5. If streaming → queue.
6. Otherwise validate model and auth → `_startRun`.

Steering and follow-ups use the same pipeline minus command dispatch (`:2237-2264`).

**Turn-end boundary** (`_dispatchTurnEndBoundary`, `:846-884`):
- The ext `turn_end` handler receives `BoundaryState{entries (drafts), continue, context preview, outcome}` and returns `{entries?, continue?}` (`X/runner.ts:1036-1085`).
- Drafts (`custom`, `custom_message`, `context_edit`, `compaction`) are previewed against an in-memory copy of the session, then committed.
- `continue` is only honored if the context can continue: it ends in non-assistant, or messages are pending (`:999-1022`).
- `agent_before_settle` works the same way after the run.

### 3.8 Nested tool calls (`S/nested-tool-calls.ts`)
- `ctx.executeTool(name,args)` from inside a tool → `NestedToolCallRunner.execute(callerId,…)`.
  - The call gets id `<callerId>/<n>`.
  - It emits `tool_execution_start/update/end` with `parentToolCallId`.
  - It runs `runToolCall` with the session hooks, so `tool_call`/`tool_result` fire with `parentToolCallId` (`S/agent-session.ts:730-764`).
  - It resolves against **callable** tools: exposure `codemode`/`deferred`, or `direct`+active (`:1582-1586`).
- Exclusive calls (sequential agent, or a sequential tool) are serialized on a promise queue. A call already holding the queue does not re-wait, which avoids deadlock (`nested-tool-calls.ts:198-218`).
- Nested results are **not** in the transcript. A bounded `nestedCalls` record is kept: max 256 calls, 8 KiB of args per call, 32 KiB total, 500-char errors (`:25-30`). Nested usage is summed into the parent toolResult's `usage` at the parent's `message_start` (`S/agent-session.ts:1108-1118`).

### 3.9 Misc
- `event-bus.ts` (33 lines) is a string-channel pub/sub for extension↔extension messaging. Handler errors are logged and swallowed (`S/event-bus.ts:12-32`).
- `output-guard.ts` redirects `process.stdout.write` to stderr while a protocol owns stdout. `writeRawStdout` serializes writes and retries on `EAGAIN`/`ENOBUFS` (`S/output-guard.ts:47-108`). This is relevant to the ACP binary (stdout = protocol).
- `AgentSessionRuntime` owns the current session plus cwd-bound services. Replacement (`newSession`/`switchSession`/`fork`) runs in this order:
  1. `session_before_switch|fork` (cancellable);
  2. `abort()` (so the aborted turn persists);
  3. `session_shutdown`;
  4. `dispose()`;
  5. create the new runtime;
  6. `session_start{reason}` (`S/agent-session-runtime.ts:139-195`).

## 4. Extension hook points (names, when, effect)

Agent-core hooks (Rust seams), all optional, all chainable:
| Hook | Fires | Can |
|---|---|---|
| `transformContext` | each request, before convert (`agent-loop.ts:399`) | rewrite AgentMessage[] (request-only) |
| `convertToLlm` | each request | lower custom roles |
| `getApiKey` | each request (`:409`) | refresh token |
| `prepareRequest` | before **every** request incl. first (`:228`) | swap context/model/thinking |
| `prepareNextTurn` | before turns 2..n, after previous `turn_end` (`:195`) | swap context/model/thinking, inject messages |
| `finishTurn` | after tool results, **before** `turn_end` (`:295`) | `end` / `continue` |
| `beforeToolCall` | after validation (`:738`) | block (+terminate) |
| `afterToolCall` | after execute, before `tool_execution_end` (`:880`) | patch result |
| `getSteeringMessages` / `getFollowUpMessages` | drain points §3.3 | inject |
| `subscribe(listener)` | every AgentEvent, awaited | observe, mutate on message_end |

Session/extension events (`X/types.ts:1405-1437`) and their firing points:
- `input` (before expansion, `S/agent-session.ts:2117`)
- `before_agent_start` (per run start; add custom messages, force system prompt, edit prompt sections and tools; `:1935`)
- `agent_start` / `agent_end` / `turn_start{turnIndex}` / `message_start` / `message_update` / `message_end` (can replace; same role required, `X/runner.ts:1163`) / `tool_execution_start|update|end` (mirrored from AgentEvents, `S/agent-session.ts:1303-1381`)
- `turn_end` (boundary: entries + continue; `:846`)
- `agent_before_settle` (boundary; `:2017`)
- `agent_settled{aborted}` (`:1084`)
- `tool_call` (block/terminate; mutate `input` in place; handler throw blocks, `X/runner.ts:1249-1267`, `S/agent-session.ts:675-680`)
- `tool_result` (chain-patch, `X/runner.ts:1190-1247`)
- `context`, `context_with_system` (per request)
- `before_provider_request` (replace payload), `before_provider_headers` (mutate), `after_provider_response{status,headers}`, `provider_stream_event` (raw) (`S/sdk.ts:353-401`)
- `model_select`, `thinking_level_select`
- `session_before_compact` / `session_compact` / `session_compact_failed`
- `session_start` / `session_shutdown` / `session_before_switch` / `session_before_fork` / `session_before_tree` / `session_tree` / `session_info_changed`
- `user_bash`
- `resources_discover`, `project_trust`, `mcp_servers_change`, `cache_warming_decision`, `ui_prompt_start/end` (UI)

Dispatch semantics: handlers run sequentially in extension order. Errors are reported via `emitError` and swallowed, **except `tool_call`, which propagates** (fail-closed). `session_before_*` short-circuits on `cancel` (`X/runner.ts:1096-1125`).

## 5. Skip list
- `renderCall/renderResult/renderShell`, `ToolRenderContext`, message/entry renderers, `ui_prompt_*`, theme, `exportToHtml`: TUI.
- `output-guard.ts` stdout hijack: Node-specific. Rust writes the protocol to a dedicated handle; keep only "logs → stderr".
- `cacheWarmer` / `cache_warming_decision`: provider prompt-cache keepalive, optional later.
- Virtual models / model routing (`isVirtualModel`, `resolveModel`, `VIRTUAL_MODEL_STATE_ENTRY`): provider-zoo.
- `scopedModels` / `cycleModel` / `cycleThinkingLevel`: Ctrl+P UI affordances.
- `identity`/attribution headers, `PI_ATTRIBUTION`, telemetry identity.
- `summarizeForBugReport`, bug-report upload, `getSessionStats` cost UI.
- `setDefaultStreamFn` global fallback: back-compat.
- `prepareNextTurn(signal)` vs `prepareNextTurnWithContext` duplication: legacy; keep one.
- `defaultConvertToLlm`, `DEFAULT_MODEL` placeholder.
- `proxy.ts`: pi's remote proxy stream.
- PowerShell tool events; MCP allowlist subtleties (`_allowlistFiltersMcp`); `codemode`/`deferred`/`tool_search` exposure: defer to extensions.
- `transport`, `thinkingBudgets`, `onProviderStreamEvent`: provider details, see the ai report.
- `AgentSessionRuntime` cwd-bound service recreation, `/import`, `extensionFlagValues`: CLI.
- Skill-block parsing for UI (`parseSkillBlock`).
- `replay` field on AgentTool: unused by the loop.

## 6. Rust port notes
- **Loop as one async fn** over `&mut Context` with a `&dyn LoopHooks` trait. Each hook has a default no-op: `prepare_request`, `prepare_next_turn`, `finish_turn`, `before_tool_call`, `after_tool_call`, `transform_context`, `convert_to_llm`, `api_key`, `steering`, `follow_up`. Events go out through `&dyn Fn(AgentEvent) -> BoxFuture<()>`, or an `async fn emit` that awaits subscribers, to keep pi's back-pressure and in-place `message_end` mutation. [INFERENCE] A bounded `tokio::mpsc` would lose the "listener mutates the message before persistence" property. Prefer awaited callbacks with `&mut AgentMessage` on `message_end`.
- **Messages:**
  ```rust
  enum AgentMessage { System(..), User(..), Assistant(..), ToolResult(..), BashExecution(..), Custom{custom_type, content, display, details: serde_json::Value}, BranchSummary(..), CompactionSummary(..) }
  ```
  `#[serde(tag="role")]`. A closed enum plus a `Custom` arm replaces TS declaration merging. `convert_to_llm(&[AgentMessage]) -> Vec<Message>` is a plain fn. Keep `transform_context` request-only, on a borrowed or `Cow` slice. Pi `structuredClone`s per request; Rust can avoid the clone unless a hook edits (`Cow<'_, [AgentMessage]>`).
- **Tools:**
  ```rust
  #[async_trait] trait Tool: Send+Sync { fn name(&self)->&str; fn description(&self)->&str; fn parameters(&self)->&serde_json::Value /*JSON Schema*/; fn execution_mode(&self)->Option<Mode>; fn prepare_arguments(&self, v: Value)->Value {v}; async fn execute(&self, id:&str, args: Value, cancel: CancellationToken, on_update: &dyn Fn(ToolResult), ctx:&ToolCtx) -> Result<ToolResult, ToolError>; }
  ```
  `ToolResult{content: Vec<Content>, details: Value, structured: Option<Value>, usage: Option<Usage>, is_error: bool, terminate: bool}`. Validate with the `jsonschema` crate. Tools may also accept typed args via `serde` + `schemars`, generating the schema from the struct.
- **Cancellation:** `tokio_util::sync::CancellationToken` per run, child tokens for tools. The provider stream must finish with an `Aborted` assistant message on cancel (`tokio::select!` on `token.cancelled()`), never by returning `Err`. Mirror pi's "never throw" contracts with infallible hook signatures; tool panics/errors become `is_error` results.
- **Parallel tools:**
  1. sequential prepare loop;
  2. `FuturesUnordered` for execution, emitting `ToolExecutionEnd` as each completes;
  3. collect outcomes into a `Vec<Option<_>>` indexed by source position;
  4. emit ToolResult messages in source order.

  Match pi's choice to leave unprepared calls without results on abort, relying on the provider transform's synthetic "No result provided". [INFERENCE] It may be cleaner to synthesize aborted results for every call so the persisted transcript is self-consistent; that would be a deliberate divergence.
- **Queues:** `Mutex<VecDeque<AgentMessage>>` ×2 + `QueueMode`, behind `Agent::steer/follow_up`, safe to call from other tasks while running. Keep the subtle rules:
  - no second steering poll in `prepare_next_turn` if one is already pending;
  - `skip_initial_steering_poll` on continue-from-assistant.
- **Session as the source of truth.** Rebuild the request context from the session projection in `prepare_request`, as pi does. Persist synchronously in the `message_end` handler before the next request. A single-owner `Session` struct (no `Rc<RefCell>`), with the agent loop borrowing hooks implemented by the session, fits Rust better than pi's field-monkeypatching chain. [INFERENCE] Model "previous hook" chaining as a `Vec<Box<dyn Hook>>` pipeline in the extension layer.
- **Post-run state machine.** The `_startRun` loop maps to a small explicit enum: `PostRun::{Retry, Compact{retry}, Queued, BeforeSettle, Settle}`. Keep the guards: one overflow retry, retry counter reset on success, abort checks between steps, deferral of prompts issued from `agent_settled`.
- **Tool-loadout-in-transcript** (`declareToolChanges`) is load-bearing for resume and branch consistency. Port it into the loop core (diff the declared set from replayed `System` messages against the executable set before each injection).
- Pitfalls:
  - (a) `finish_turn` runs **before** `turn_end`;
  - (b) `message_start` for assistant fires on the stream's `start` event, with the partial in context, so the transcript temporarily holds a partial assistant;
  - (c) `length` stop → fail all tool calls;
  - (d) `structured_content` is dropped when only `content` is patched;
  - (e) custom messages and user-bash output must never be inserted between a tool call and its result; defer to `turn_end`/run end;
  - (f) a `tool_call` hook error must block (fail-closed);
  - (g) error/aborted assistant messages stay persisted but are excluded from requests.
- Crates: `tokio`, `tokio-util` (CancellationToken), `futures` (FuturesUnordered, BoxFuture), `serde`/`serde_json`, `jsonschema`, `schemars` (optional), `async-trait` (or native AFIT + `dyn` via boxing).

## 7. Size (non-test TS lines)
| File | Lines |
|---|---|
| agent/src/agent-loop.ts | 959 |
| agent/src/agent.ts | 623 |
| agent/src/types.ts | 539 |
| agent/src/stream-fn.ts | 20 |
| coding-agent/src/core/agent-session.ts | 4458 (≈1500 relevant to the loop; the rest is tools registry, tree nav, export, extension binding) |
| coding-agent/src/core/agent-session-runtime.ts | 447 |
| coding-agent/src/core/agent-session-services.ts | 233 |
| coding-agent/src/core/messages.ts | 196 |
| coding-agent/src/core/nested-tool-calls.ts | 263 |
| coding-agent/src/core/event-bus.ts | 33 |
| coding-agent/src/core/output-guard.ts | 108 |
| (context) core/extensions/runner.ts | 1565 |
| (context) core/extensions/types.ts | 2304 (mostly UI/registration API) |
| (context) core/sdk.ts | 479 |

The core loop and Agent state machine come to about 2.1k TS lines. [INFERENCE] An idiomatic Rust port of loop + agent + tool pipeline is roughly 1.2–1.8k lines. The session orchestration on top (persistence, retry, compaction triggers, queues, boundaries, prompt expansion) is a similar amount again.
