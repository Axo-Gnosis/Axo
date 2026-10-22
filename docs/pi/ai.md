# pi-ai core (model/provider layer): port report

Source: `/tmp/pi-src/packages/ai/src` (pi v1.3.0). All paths below are relative to that directory unless prefixed.

---

## 1. Concepts

- **One normalized transcript, many wire formats.** Callers pass a `Context` (`systemPrompt`, `messages`, `tools`). `normalizeContext()` folds the prompt and tools into a leading **`SystemMessage`**, producing a branded `TranscriptContext`, which is the only thing adapters accept (`types.ts:777-794`, `utils/transcript.ts:30-34`). Later `SystemMessage`s in the transcript *patch* the prompt (`content` appends, `sections` replace or remove by name, `toolsAdded`/`toolsRemoved` change the tool set) (`types.ts:542-568`). Adapters either send them in place (when `compat.supportsMidConvoSystemMessages`) or collapse them into one leading message (`utils/transcript.ts:108-120`). **This is new compared to older pi:** tools and the system prompt live *inside* the message list.
- **Adapter = `StreamFunction`.** `(model, TranscriptContext, options) -> AssistantMessageEventStream`, returned synchronously; all failures after it returns are encoded *in the stream* as a terminal `error` event carrying an `AssistantMessage` with `stopReason: "error" | "aborted"` (`types.ts:385-400`). There are two entry points per API: `stream` takes API-specific options; `streamSimple` takes a provider-neutral `reasoning` level and maps it (`types.ts:317-330`, `374-383`).
- **Partial message accumulation.** The adapter owns one mutable `AssistantMessage` (`output`), pushes content blocks into it, and emits events whose `partial` *is that same object*, not a snapshot (`types.ts:796-811`).
- **Provider vs API.** An *API* is a wire format (`"anthropic-messages"`, `"openai-completions"`, …) (`types.ts:62-74`). A *Provider* is a runtime unit with an id, a base URL, auth, a model list and API implementations (`models.ts:158-247`). One provider can mix APIs; `createProvider({api: {...}})` dispatches on `model.api` (`models.ts:1077-1162`).
- **`Models` collection.** It owns provider registration (`setProvider`), auth resolution (credential store, then ambient env), and request dispatch. `stream()` resolves auth, overlays `apiKey`/`headers`/`baseUrl` onto the request, and delegates to the provider (`models.ts:856-919`).
- **Cross-model history.** `transformMessages()` makes a transcript produced by model A replayable to model B: it drops foreign thinking, strips signatures, normalizes tool-call IDs, synthesizes missing tool results, and drops errored or aborted turns (`api/transform-messages.ts:91-256`).
- **Retry / overflow / cost** are pure functions over the final `AssistantMessage`: `isRetryableAssistantError`, `isContextOverflow`, and `calculateCost` (`utils/retry.ts:266`, `utils/overflow.ts:142`, `models.ts:1236`). HTTP-level retry is a separate, abortable wrapper around SDK calls (`utils/provider-retry.ts:106-128`).

---

## 2. Types (trimmed, near-verbatim)

```ts
// types.ts:420-455
interface TextContent     { type:"text"; text:string; textSignature?:string }       // OpenAI Responses msg id / TextSignatureV1 JSON
interface ThinkingContent { type:"thinking"; thinking:string;
                            thinkingSignature?:string;   // opaque replay data (Anthropic signature, OpenAI reasoning item JSON, reasoning_details JSON, or the *field name* "reasoning_content")
                            redacted?:boolean }          // payload lives in thinkingSignature
interface ImageContent    { type:"image"; data:string /*base64*/; mimeType:string }
interface ToolCall        { type:"toolCall"; id:string; name:string; arguments:JsonObject;
                            thoughtSignature?:string;    // Google only
                            namespace?:string;           // OpenAI Responses only
                            argumentsError?:string }     // set when final args text was not a JSON object; arguments = {}

// types.ts:457-480
interface Usage { input; output; cacheRead; cacheWrite: number;
                  cacheWrite1h?: number;   // subset of cacheWrite, Anthropic only
                  reasoning?: number;      // subset of output
                  totalTokens: number;
                  cost: { input; output; cacheRead; cacheWrite; total: number } }   // USD
type StopReason = "pending"|"stop"|"length"|"toolUse"|"error"|"aborted"|"deferred";

// types.ts:552-650
interface SystemMessage { role:"system"; content:string|TextContent[];
                          sections?:Record<string,string|null>; toolsAdded?:Tool[]; toolsRemoved?:{name:string}[];
                          timestamp:number }
interface UserMessage   { role:"user"; content:string|(TextContent|ImageContent)[]; timestamp:number }
interface AssistantMessage {
  role:"assistant"; content:(TextContent|ThinkingContent|ToolCall)[];
  api:Api; provider:string; model:string;
  responseModel?:string; responseId?:string;
  providerThinkingLevel?:string; thinkingLevel?:ModelThinkingLevel;
  diagnostics?:AssistantMessageDiagnostic[];
  usage:Usage; stopReason:StopReason; errorMessage?:string; rawStopReason?:string;
  endTurn?:boolean; timestamp:number; durationMs?:number;
  deferred?:DeferredHandle }                                  // skip
interface ToolResultMessage { role:"toolResult"; toolCallId; toolName; content:(TextContent|ImageContent)[];
  details?:JsonValue; usage?:Usage; nestedCalls?:NestedToolCalls; isError:boolean; timestamp; durationMs? }
type Message = SystemMessage | UserMessage | AssistantMessage | ToolResultMessage;

// types.ts:760-781
interface Tool { name; description; parameters: TSchema /*JSON Schema*/;
                 constrainedSampling?: false | {type:"json_schema"; strict:"prefer"|"require"}
                                             | {type:"grammar"; variants:{openai_lark?;openai_regex?}} }
interface Context { systemPrompt?:string; messages:Message[]; tools?:Tool[] }
type TranscriptContext = { messages: Message[] } & Brand;

// types.ts:812-828
type AssistantMessageEvent =
 | {type:"start"; partial}
 | {type:"text_start"|"thinking_start"|"toolcall_start"; contentIndex; partial}
 | {type:"text_delta"|"thinking_delta"|"toolcall_delta"; contentIndex; delta:string; partial}
 | {type:"text_end"|"thinking_end"; contentIndex; content:string; partial}
 | {type:"toolcall_end"; contentIndex; toolCall:ToolCall; partial}
 | {type:"done"; reason:"stop"|"length"|"toolUse"|"deferred"; message:AssistantMessage}
 | {type:"error"; reason:"aborted"|"error"; error:AssistantMessage};

// types.ts:831-878 + providers/model-schema.ts:3-49
interface Model<TApi> { id; name; api:TApi; provider; baseUrl; input:("text"|"image")[];
  inputLimits?:{maxRequestBytes?; images?:{resize?;maxPerMessage?;maxPerRequest?}};
  cost:{ input; output; cacheRead; cacheWrite /*USD per 1M tokens*/; tiers?:{inputTokensAbove;input;output;cacheRead;cacheWrite}[] };
  headers?:Record<string,string>;
  reasoning:boolean;
  thinkingLevelMap?: Partial<Record<"off"|"minimal"|"low"|"medium"|"high"|"xhigh"|"max", string|null>>; // null = unsupported
  promptCache?: {short?:seconds; long?:seconds};
  contextWindow:number; maxTokens:number;
  samplingParams?:Record<string,unknown>; samplingParamsByThinkingLevel?:…;
  compat?: per-API compat struct }       // providers/compat-schema.ts:188-501

// types.ts:157-268, 375-383 (trimmed)
interface StreamOptions { signal?; apiKey?; headers?:Record<string,string|null> /*null suppresses default*/;
  onPayload?(payload, model) -> replacement?; onResponse?({status,headers}, model); onProviderStreamEvent?(data, model);
  timeoutMs?; maxRetries?; maxRetryDelayMs? /*default 60000*/; env?:Record<string,string>;
  temperature?; maxTokens?; samplingParams?; cacheRetention?:"none"|"short"|"long"; sessionId?; metadata? }
interface SimpleStreamOptions extends StreamOptions { toolChoice?:"auto"|"none"; reasoning?:ThinkingLevel; thinkingBudgets?:{minimal?;low?;medium?;high?} }

// auth/types.ts:7-41, 64-95, 98-110, 151-246
interface ModelAuth { apiKey?:string; headers?:ProviderHeaders; baseUrl?:string }   // "if it can't be expressed as these, it is config, not auth"
type Credential = {type:"api_key"; key?:string; env?:Record<string,string>}
                | {type:"oauth"; refresh:string; access:string; expires:number; [k:string]:unknown};
interface CredentialStore { read(id); list(); modify(id, fn(current)->next?) /*only write path, serialized per id*/; delete(id) }
interface AuthContext { env(name):Promise<string?>; fileExists(path):Promise<bool> }
interface AuthResult { auth:ModelAuth; env?:ProviderEnv; source?:string }
interface ApiKeyAuth { name; login?(interaction)->ApiKeyCredential; check?(…); resolve({ctx, credential?, signal})->AuthResult? }
interface OAuthAuth  { name; isSubscription?; login(interaction, opts)->OAuthCredential;
                       refresh(cred, signal)->OAuthCredential; toAuth(cred)->ModelAuth }
interface ProviderAuth { apiKey?:ApiKeyAuth; oauth?:OAuthAuth }   // at least one

// models.ts:158-247 (chat subset)
interface Provider { id; name; baseUrl?; headers?; auth:ProviderAuth;
  getModels(): Model[]; refreshModels?(ctx); filterModels?(models, cred);
  stream(model, TranscriptContext, opts); streamSimple(model, TranscriptContext, opts) }
```

---

## 3. Behavior a port must keep

### 3.1 Event stream (`utils/event-stream.ts`)
- The stream is an unbounded FIFO queue with async iteration. `push` after `done` is ignored; the first `done`/`error` event sets `done` and resolves `result()` (`:43-58`). `end(result?)` wakes all waiters (`:60-70`).
- `durationMs` is stamped on the final message from a monotonic clock started at stream creation, unless already set or `timestamp < stream start` (forwarded deferred results) (`:97-130`).
- Ordering contract (`types.ts:796-811`): `start` comes before any update or `done`. An `error` may come *without* `start` (setup failure). Text and thinking blocks start empty and grow only through deltas. `*_end` is authoritative. Redacted thinking may be complete at `thinking_start`, with no deltas.
- `lazyStream` returns the outer stream synchronously, runs async setup (auth resolution), then forwards events. A setup throw becomes `{type:"error", reason:"error"}` with an empty-usage message and **no `start`** (`api/lazy.ts:61-88`).

### 3.2 Common adapter skeleton (identical in both adapters)
1. Create `output` with zero usage, `stopReason:"pending"`, `timestamp: Date.now()` (`api/anthropic-messages.ts:581-597`, `api/openai-completions.ts:313-329`).
2. Build params, then `onPayload` may replace them (`anthropic:650-653`, `completions:367-370`).
3. Send via `retryProviderRequest` with SDK `maxRetries:0` (`anthropic:638-662`, `completions:371-383`).
4. Call `onResponse`, **then** push `start` (`anthropic:690-691`, `completions:384-385`). `start` therefore means "HTTP accepted".
5. Iterate events and mutate `output`.
6. After the loop: if the signal is aborted, throw. If the stop reason is still `pending`, throw. If the stop reason is `error`/`aborted`, throw. Otherwise push `done`, then `end` (`anthropic:894-919`, `completions:688-709`).
7. `catch`: strip scratch fields (`index`, `partialJson`, `partialArgs`, …) from blocks, set `stopReason = signal.aborted ? "aborted" : "error"`, set `errorMessage`, push `error`, then `end` (`anthropic:920-930`, `completions:710-733`). **Partial content is kept** on the error message.

### 3.3 Tool-call argument streaming
- Each delta is appended to a scratch string, and `arguments = parseStreamingJson(buf)` updates the live view: strict parse, then repair, then `partial-json`, falling back to `{}` (`utils/json-parse.ts:129-149`).
- At block end, `parseToolCallArguments(buf)` runs strict-with-repair. If the result is not an object, `arguments = {}` and `argumentsError = "<err>\n\nReceived arguments:\n<text>"` (`utils/json-parse.ts:106-123`; used at `anthropic:836`, `completions:465`).
- `repairJson` escapes raw control characters inside strings and doubles invalid backslashes (`utils/json-parse.ts:33-84`). Anthropic's SSE event JSON also goes through `parseJsonWithRepair` (`anthropic:551`).

### 3.4 Anthropic Messages adapter (`api/anthropic-messages.ts`)
- **Transport:** the adapter uses its own SSE decoder (`:400-528`) rather than the SDK's stream. Only the six `message_*`/`content_block_*` event types are passed on, and an `event: error` frame throws its data (`:530-569`). If `message_start` arrived but `message_stop` never did, it throws `"Anthropic stream ended before message_stop"` (`:566-568`; this text is matched by the retry regex). The abort signal is checked before each read (`:482-484`).
- **Block mapping:** each Anthropic `event.index` is tracked on the block, and deltas look up `blocks.findIndex(b => b.index === event.index)`, so `contentIndex` = position in `output.content`, not the Anthropic index (`:693-847`). `fallback` content blocks are skipped; one arriving after content has started throws (`:723-728`).
  - `thinking`: `thinkingSignature` starts as `""`, and `signature_delta` appends to it **without emitting an event** (`:808-815`).
  - `redacted_thinking` → `{thinking:"[Reasoning redacted]", thinkingSignature:data, redacted:true}` (`:746-755`).
  - `tool_use`: under OAuth the name is mapped back from Claude-Code casing to the caller's tool name (`:756-768`, `:125-132`).
- **Usage:** `message_start` sets all fields, including `cacheWrite1h`. `message_delta` overwrites only non-null fields (proxies omit input). `totalTokens = input+output+cacheRead+cacheWrite`. `calculateCost` runs after every update, priced with the *fallback model's* cost if the server switched models (`:698-721`, `:848-891`).
- **Stop reasons** (`:1664-1690`): `end_turn`/`pause_turn`/`stop_sequence` → `stop`; `max_tokens` → `length`; `tool_use` → `toolUse`; `refusal` → `error` with `stop_details.explanation`; `sensitive` → `error`; anything unknown **throws**.
- **Request building** (`buildParams :1160-1332`):
  - The system prompt is a `[{type:"text", text, cache_control}]` block. With OAuth, the first block is `"You are Claude Code, Anthropic's official CLI for Claude."` (`:1227-1252`).
  - `max_tokens = options.maxTokens ?? model.maxTokens`.
  - Temperature is sent only when thinking is off and `compat.supportsTemperature` holds (`:1254-1262`).
  - Thinking: `{type:"disabled"}` when explicitly off; `{type:"adaptive", display}` + `output_config.effort` when `compat.forceAdaptiveThinking`; otherwise `{type:"enabled", budget_tokens, display:"summarized"}` (`:1278-1309`).
  - Tool choice is a string or `{type:"tool", name}` (`:1318-1324`).
- **Cache control** (`:79-93`, `:1529-1555`, `:1659`): `{type:"ephemeral", ttl?:"1h"}` (`long` retention plus compat; default retention `short`; `PI_CACHE_RETENTION=long` env). Breakpoints go on the system blocks, the **last tool**, and the **last block of the last user/system message**; string content is promoted to a block. Retention `none` sends no markers.
- **Betas** (`:1116-1158`): an `anthropic-beta` header in model or options headers *overrides* the computed list entirely (`null` → none). Otherwise: OAuth → `claude-code-20250219,oauth-2025-04-20`; tools and no eager streaming → `fine-grained-tool-streaming-2025-05-14`; budget thinking → `interleaved-thinking-2025-05-14`; plus several 2026 betas that this report skips.
- **Message conversion** (`convertMessages :1353-1558`):
  - User text blocks that are whitespace-only are dropped, and an empty user message is skipped (`:1400-1430`).
  - Assistant blocks:
    - Whitespace-only text is dropped.
    - Redacted thinking → `{type:"redacted_thinking", data}`.
    - Thinking with an empty signature becomes plain `text` (an aborted stream leaves no signature), unless `compat.allowEmptySignature`.
    - Thinking with empty text and no signature is dropped.
    - Tool calls become `tool_use` with `input: arguments` (`:1431-1491`).
  - Consecutive `toolResult`s merge into **one** user message of `tool_result` blocks. Images in *error* tool results are split out and appended after all results with a caption (`:1500-1524`, `transform-messages.ts:31-40`).
  - Tool result content: text-only results are sent as a joined string. Image-only results get a `"(see attached image)"` text first (`:148-188`).
  - Tool IDs are normalized to `[a-zA-Z0-9_-]{≤64}` (`:1335-1337`).
- **Strict-tool downgrade:** a 400 matching the grammar-size messages retries once without preferred strict tools and records a diagnostic (`:645-689`). Optional.
- **Auth:**
  - `apiKey`, or an `authorization`/`x-api-key` header, is required (`:316-327`).
  - An OAuth token is recognized by substring `sk-ant-oat` and switches to Bearer auth, Claude-Code identity headers (`user-agent: claude-cli/<v>`, `x-app: cli`), tool renaming, and the system preamble (`:1011-1069`).
  - Header merge order is identity → defaults → session-affinity → `model.headers` → `options.headers`, later wins (`:1078-1087`).
- **`streamSimple`** (`:961-1009`):
  - Thinking off → `thinkingEnabled:false`.
  - Adaptive models → effort from `thinkingLevelMap`, else minimal/low→low, medium→medium, else high.
  - Older models → `adjustMaxTokensForThinking` (budget per level: 1024/2048/8192/16384; `maxTokens = min(base+budget, model.maxTokens)`), then `clampMaxTokensToContext` (`contextWindow − estimated tokens − 4096`), then `budget = min(budget, maxTokens−1024)` (`api/simple-options.ts:18-109`).

### 3.5 OpenAI Chat Completions adapter (`api/openai-completions.ts`)
- **Auth:** `apiKey`, or an `authorization` header (then the key is `"unused"`), else it throws `"No API key for provider: X"` (`:87-91`). Header order is identity → `model.headers` → session-affinity → `options.headers` (`:770-795`).
- **Params** (`:806-1016`):
  - `stream:true` and `stream_options.include_usage` (unless compat says otherwise).
  - `store:false` when supported.
  - `max_completion_tokens`, or `max_tokens` per `compat.maxTokensField`.
  - `prompt_cache_key = sessionId`; it is set only when (base URL is api.openai.com and retention ≠ none) or retention is long.
  - Tools via `convertTools`, or `tools: []` when the history has tool calls but no current tools (LiteLLM→Anthropic proxies).
  - Reasoning parameters chosen per `compat.thinkingFormat` (11 variants, `:886-983`). The plain `openai` path sends `reasoning_effort = thinkingLevelMap[level] ?? level`, or the map's `off` value when reasoning is off.
  - `samplingParams` are merged **last**, so they override named fields (`:1009-1013`).
- **Streaming state machine** (`:387-686`):
  - There is at most **one** text block and **one** thinking block per response (`ensureTextBlock`/`ensureThinkingBlock`); tool calls are keyed by `delta.tool_calls[].index`, falling back to `id`.
  - Every chunk: `responseId ||= chunk.id`; if `chunk.model ≠ model.id`, set `responseModel`; `chunk.usage` (or `choice.usage` for Moonshot) **replaces** usage.
  - Reasoning is processed **before** content in the same delta. The reasoning text is the first non-empty field of `reasoning_content`, `reasoning`, `reasoning_text`. **The field name is stored as `thinkingSignature`** so replay writes the same field back (`:592-626`, replay at `:1349-1359`).
  - OpenRouter `reasoning_details[]` deltas are merged and stored as JSON in `thinkingSignature` when the block finishes (`:331-338`, `:628-639`).
  - All blocks are finished (end events) **after** the chunk loop, not as they close (`:688-690`).
  - Termination: aborted → throw. With no `finish_reason` and `!compat.supportsFinishReason`, the stop reason is inferred as `toolUse` if any tool call exists, else `stop`. A missing `finish_reason` otherwise throws `"Stream ended without finish_reason"` (`:691-706`).
  - Error messages go through `normalizeProviderError` + `formatProviderError`, and OpenRouter `error.metadata.raw` is appended (`:721-730`).
- **Usage** (`:1529-1570`): `cacheRead = prompt_tokens_details.cached_tokens ?? prompt_cache_hit_tokens ?? cached_tokens`, `cacheWrite = prompt_tokens_details.cache_write_tokens`, `input = max(0, prompt − cacheRead − cacheWrite)`, `output = completion_tokens` (includes reasoning), `reasoning = completion_tokens_details.reasoning_tokens`.
- **Stop reasons** (`:1572-1596`): `stop`/`end`/`null` → `stop`; `length` → `length`; `tool_calls`/`function_call` → `toolUse`; `content_filter`/`network_error`/anything else → `error` with `"Provider finish_reason: X"`.
- **Message conversion** (`:1199-1490`):
  - Instruction role: `developer` if `model.reasoning && compat.supportsDeveloperRole`, else `system`. Later system messages are rendered with `renderSystemMessageUpdate` (`utils/text.ts:28-40`).
  - User images → `image_url` with a `data:` URL.
  - Assistant content is sent as a **plain string**, never parts (DeepSeek-via-NIM mirrors parts). Thinking goes back into the named reasoning field, or into `reasoning_details`. `compat.requiresThinkingAsText` prepends the thinking as text. DeepSeek needs `reasoning_content:""` on every assistant message. Assistant messages with no content and no tool calls are skipped.
  - Tool calls: `function.arguments = JSON.stringify(arguments)`.
  - Tool results become one `role:"tool"` message each. Images cannot go in tool messages, so they are gathered into a following user message `"Attached image(s) from tool result:"`. Empty results → `"(no tool output)"`; image-only → `"(see attached image)"`.
  - `compat.requiresAssistantAfterToolResult` inserts `"I have processed the tool results."`.
  - Tool IDs: Responses-style `call|item` IDs are sanitized and hashed down to ≤40 characters; on provider `openai`, IDs are truncated to 40 (`:1208-1232`).
- **Cache control for OpenAI-compatible endpoints** (`:1083-1197`): only when `compat.cacheControlFormat === "anthropic"` (auto-detected for OpenRouter `anthropic/*`). Markers go on the first system/developer message, the last tool, and the last text part of the last user/assistant/tool message.
- **Compat auto-detection** from provider id and base-URL substrings, then overridden field by field by `model.compat` (`:1603-1744`). Defaults for an unknown local server such as llama.cpp: `supportsStore` / `supportsDeveloperRole` / `supportsReasoningEffort` true, `max_completion_tokens`, thinking format `openai`, no strict mode, `supportsFinishReason` true.

### 3.6 OpenAI Responses (brief)
- Tool call ids are `${call_id}|${item.id}` (`api/openai-responses-shared.ts:567,588`). This is why the other adapters normalize `|` ids.
- A reasoning item is stored **whole as JSON** in `thinkingSignature`, with `encrypted_content` back-filled from `response.completed` when Azure omits it (`:612-628`, `:770`). The request uses `include:["reasoning.encrypted_content"]` with `store:false` for stateless replay (`api/openai-responses.ts:413`).
- `textSignature` carries the output message id/phase (`TextSignatureV1`, `types.ts:414-424`).

### 3.7 Cross-provider conversion (`api/transform-messages.ts:91-256`)
- First, `null` content becomes `[]`. If the target model lacks `"image"` input, image runs in user and tool messages are replaced with one placeholder each (`:62-84`).
- `isSameModel` means provider, api and model id all match (`:122-125`).
  - Foreign thinking is **dropped entirely**, not converted to text (`:132`).
  - Same-model thinking keeps redacted or signed blocks even when the text is empty, and drops blocks with empty text and no signature (`:135-137`).
  - Foreign text loses `textSignature`; foreign tool calls lose `thoughtSignature` and get adapter-normalized IDs.
  - Tool results are re-keyed through the id map.
- Second pass:
  - Assistant messages with `stopReason` `error`/`aborted` are **skipped** (`:222`).
  - Orphaned tool calls get a synthetic `{isError:true, content:"No result provided"}` result when the next assistant message, the next user message, or the end of the transcript arrives (`:183-205`, `:214`, `:245`, `:253`).
  - System messages between a call and its results are held and emitted after the results.

### 3.8 Auth resolution (`auth/resolve.ts`, `models.ts:754-899`)
- Resolution order:
  1. An explicit `options.apiKey` override goes through `apiKey.resolve` as a synthetic credential.
  2. Otherwise a **stored credential owns the provider**: OAuth → refresh-if-needed then `toAuth`; api_key → `resolve`. A credential type with no handler yields `undefined` and there is **no env fallback** (`:47-89`).
  3. Otherwise ambient `apiKey.resolve(ctx)` from env vars and files.
- OAuth refresh: a token is "expiring" within 5 minutes. The refresh runs **inside `CredentialStore.modify`** (per-provider lock) and re-checks expiry under the lock. The caller's signal cancels only the lock wait; the refresh itself has a 15 s timeout and ignores the abort, so a rotated refresh token is never lost (`:91-151`).
- `applyAuth`:
  - Explicit `options.apiKey` beats the resolved key, and `ModelAuth.baseUrl` **replaces `model.baseUrl`** for the request.
  - Headers merge case-insensitively: OpenCode session headers → `auth.headers` (which already include `model.headers`) → `options.headers`.
  - `transformHeaders` runs **last** on the fully assembled headers.
  - Env is `resolution.env` overlaid by `options.env` (`models.ts:856-899`, `384-398`, `765-772`).
- Provider auth examples:
  - `envApiKeyAuth(name, vars)`: stored key wins, else the first set env var (`auth/helpers.ts:9-31`).
  - Anthropic: `ANTHROPIC_AUTH_TOKEN` → `Authorization: Bearer` header; then `ANTHROPIC_OAUTH_TOKEN`/`ANTHROPIC_API_KEY` as the API key (`providers/anthropic.ts:19-73`).
  - OAuth `toAuth` implementations return `{apiKey: access}` (Anthropic, OpenAI), `{headers:{Authorization}}` (Kimi), or `{apiKey, baseUrl}` (Copilot) (`auth/oauth/anthropic.ts:304`, `kimi-coding.ts:293`, `github-copilot.ts:501-506`).

### 3.9 Where a plugin injects auth, headers or base URL
- `ProviderAuth.oauth.{login, refresh, toAuth}` and `apiKey.resolve` return a `ModelAuth{apiKey, headers, baseUrl}`. **This is the seam** axo's design calls for: the extension owns auth, the core streams.
- `StreamOptions.headers` (where `null` deletes a default), `onPayload` (rewrite the body), `onResponse`, `onProviderStreamEvent` (raw events), and `ModelsRequestTransforms.transformHeaders` (`models.ts:120-123`).
- A coding-agent extension calls `pi.registerProvider(name, ProviderConfig)` with `{baseUrl, apiKey ("$ENV" / "!cmd"), api, headers, authHeader, models[], refreshModels, oauth:{login, refreshToken, getApiKey, modifyModels}, streamSimple?}` (`/tmp/pi-src/packages/coding-agent/src/core/extensions/types.ts:1931-1987`). A native `Provider` is also accepted (`types.ts:1858`).
- **Caveat for axo:** Anthropic subscription OAuth is *not* just headers. The adapter body also switches the system preamble, tool-name casing and betas when it sees `sk-ant-oat` (`anthropic-messages.ts:1011-1013`, `1142`, `1228-1242`, `124`). Either keep that branch in the core or expose a flag on `ModelAuth`.

### 3.10 HTTP retry (`utils/provider-retry.ts`)
- Applies to errors with `status`/`headers` only.
  - `x-should-retry` decides when present.
  - Otherwise retry on no status, 408, 409, 429, or ≥500.
- Delay:
  - Taken from `retry-after-ms`, else `retry-after` (seconds or HTTP date).
  - A server-requested delay above `maxRetryDelayMs` (60 s default; 0 disables the cap) **throws immediately** with `"Server requested Ns retry delay"`.
  - Otherwise `min(0.5·2^i, 8) s × (1 − rand·0.25)`.
- The sleep is abortable. `maxRetries` defaults to **0** (`:106-128`).
- This covers only the request until headers arrive. Failures after the stream has started are never retried here.

### 3.11 Turn-level retry (`utils/retry.ts`)
- `isRetryableAssistantError`: `stopReason === "error"`, not matching the quota/billing patterns, and matching the transient patterns (overloaded, 429/5xx as standalone numbers, network/socket/timeout, "ended without", …) (`:3-117`, `:266-271`). The pattern list is the real contract; copy it.
- `retryAssistantCall(produce, policy, signal, callbacks)`:
  - Exponential `baseDelayMs·2^(n−1)`, capped at 60 s.
  - An abort during backoff returns the last response with `stopReason:"aborted"`.
  - Callbacks: `onRetryScheduled`, `onRetryAttemptStart`, `onRetryFinished` (`:142-254`).
- The coding-agent checks overflow first: "Context overflow is handled by compaction, not retry" (`/tmp/pi-src/packages/coding-agent/src/core/agent-session.ts:3770-3772`).

### 3.12 Context overflow (`utils/overflow.ts:142-186`)
`isContextOverflow(msg, contextWindow?)` is true in three cases:
1. The message is `error`, its text does not match a non-overflow pattern (throttling, rate limit, too many requests), and it matches one of 26 overflow regexes (`:38-68`), including a **leading** `413`. Cerebras bodyless 400/413 also counts.
2. `stop` with `input + cacheRead > contextWindow` (silent overflow).
3. `length` with `output == 0` and `input + cacheRead ≥ 0.99·contextWindow`.

`isRecoverableLength(msg, desiredMax)` is `length` with `output < desiredMax`.

### 3.13 Estimation (`utils/estimate.ts`)
- 3.5 characters per token; an image counts as 4800 characters (`:15-16`).
- `estimateContextTokens` starts from the last assistant message whose usage applies to the current prefix (not error/aborted, non-zero, and its timestamp ≥ every earlier message's timestamp so a compaction summary invalidates it), then adds estimates for the messages after it (`:70-117`). Used for `clampMaxTokensToContext`.

### 3.14 Cost (`models.ts:1236-1256`)
- Rates are USD per 1M tokens.
- Tier selection: the highest `inputTokensAbove` that is below `input + cacheRead + cacheWrite` applies to the whole request.
- `cacheWrite` cost is `cacheWrite·(cacheWrite − cacheWrite1h) + input·2·cacheWrite1h`, so 1-hour writes are billed at 2× the base input rate.
- `total` is the sum of the parts. Adapters recompute cost on every usage update.

### 3.15 Thinking levels (`models.ts:1258-1290`)
- Supported levels:
  - Non-reasoning models support only `off`.
  - `xhigh`/`max` are supported only when the map has an entry for them.
  - A `null` map entry disables that level.
- `clampThinkingLevel` picks the nearest supported level, searching upward first, then downward.

### 3.16 Model catalog
- Generated JSON is grouped `{api: {"chat:<id>": Model}}` and flattened per provider by `type` (`model-catalog.ts:55-81`). The data files are generated and **not present in the clone**.
- `createProvider` merges static baseline models with a dynamic overlay matched by (type, id), and persists to a `ModelsStore` keyed by provider id with `{models, lastModified, checkedAt, etag}` (`models.ts:1092-1156`, `models-store.ts:3-26`).
- `builtinModels()` registers all 42 built-in providers (`providers/all.ts:135-190`).

### 3.17 Text sanitization
`sanitizeSurrogates` strips unpaired UTF-16 surrogates from all outgoing text (`utils/sanitize-unicode.ts:21-25`).

---

## 4. Skip list

- **Image-generation and classifier models/APIs.** `ImageModel`, `ClassifierModel`, `images*.ts`, `*-classify.ts`, `openai-decisions.ts`, `typesafe-system-one`.
- **Deferred responses.** `DeferredHandle`, `fetchDeferred`/`cancelDeferred`, `"deferred"` stop reason, Anthropic batch.
- **Provider zoo.** Bedrock, Google, Vertex, Mistral, Codex, Azure, `pi-messages`, Cloudflare (bindings and gateway), and 30+ OpenAI-compatible vendor providers (keep only compat flags).
- **11 `thinkingFormat` variants.** Keep `openai`, `qwen-chat-template`/`chat-template` (llama.cpp/Qwen) and maybe `deepseek`. Drop zai, baseten, together, ant-ling, string-thinking and openrouter for v1.
- **Routing.** OpenRouter/Vercel routing objects, `vllmPriority`, z.ai `tool_stream`.
- **GitHub Copilot.** Dynamic headers and vision header (`anthropic:613-620`, `completions:771-778`).
- **Anthropic extras.**
  - Workload-identity federation and the cached federation client (`anthropic:343-377`, `1088-1102`).
  - Server-side fallback models with fallback cost; `fallback` blocks.
  - Mid-conversation effort system messages, `insertThinkingLevelMessages`, `providerThinkingLevel` (`:1564-1578`).
  - Inline tool `tool_addition`/`tool_removal`, deferred-placeholder tool, `inline-tools` beta (`:201-213`, `1384-1398`). Revisit only if axo adopts mid-conversation tool changes.
  - Strict-tool fallback loop and grammar/constrained sampling (`api/constrained-sampling.ts`). Defer.
- **Telemetry and identity attribution.** Telemetry spans, `telemetry.ts`, `observeModelRequest`, `ClientIdentity` attribution headers (keep a plain `User-Agent`).
- **Transport and runtime plumbing.** WebSocket transport, `websocketConnectTimeoutMs`, Node HTTP proxy, `lazyApi`/lazy module loading (a JS bundle concern), browser-safe `AuthContext` file probing.
- **Legacy and CLI.** `legacy-api-aliases.ts`, `compat.ts`, `compat/extension-oauth-types.ts` (legacy extension callback shape), `cli.ts`, `bun-oauth.ts`, `utils/oauth-page.ts`.
- **Typing machinery.** `JsonRepresentation`/`IsJsonCompatible` type gymnastics; TypeBox schemas (use serde instead).
- **Catalog refresh.** Model refresh generations and publication chains (`models.ts:498-625`), `ModelsStore` ETag handling. Optional; static catalog plus host-supplied models first.
- **`faux.ts` provider.** Do not port as a feature, but its idea (a scripted provider) is the right test double. [INFERENCE]

---

## 5. Rust port notes

### Crate shape (`axo-ai`)
- **`types.rs`:**
  - `enum Message { System(SystemMessage), User(UserMessage), Assistant(AssistantMessage), ToolResult(ToolResultMessage) }` with `#[serde(tag = "role", rename_all = "camelCase")]`.
  - Content as `enum AssistantBlock { Text{..}, Thinking{..}, ToolCall{..} }` and `enum UserBlock { Text, Image }` with `#[serde(tag = "type")]`; `tag = "toolCall"` needs an explicit rename.
  - `UserMessage.content` as `enum Content { Text(String), Blocks(Vec<UserBlock>) }` with `#[serde(untagged)]`, or always normalize to `Vec`. [INFERENCE]: always store `Vec`, accept a string on deserialize.
  - `StopReason` as a plain enum. `Usage { input, output, cache_read, cache_write, cache_write_1h: Option<u64>, reasoning: Option<u64>, total_tokens, cost: Cost }` with `u64` counts and `f64` cost.
  - `ToolCall.arguments: serde_json::Map<String, Value>`.
- **Streaming contract** [INFERENCE]:
  - `trait Api: Send + Sync { fn stream(&self, model: &Model, ctx: &Transcript, opts: StreamOptions, cancel: CancellationToken) -> BoxStream<'static, AssistantEvent>; }`.
  - Return the stream synchronously and do auth/setup inside it (`async_stream::stream!` or a spawned task feeding `mpsc`), mirroring `lazyStream`. The terminal event carries the owned final `AssistantMessage`.
  - Prefer a `tokio::sync::mpsc` channel (bounded is fine; the TS queue is unbounded) plus a `JoinHandle`, so dropping the receiver cancels the request.
- **Do not copy `partial` per event.** In TS `partial` is a shared mutable reference, which is free. In Rust:
  - Events carry `{content_index, delta}`.
  - The consumer rebuilds state with a reusable `fn apply(&mut AssistantMessage, &Event)` accumulator, exposed as an `Accumulator` the loop and the ACP layer share.
  - Only `Start` and `Done`/`Error` carry a full message. This matters for axo's memory goal. [INFERENCE]
- **Abort:** `tokio_util::sync::CancellationToken`, raced with `tokio::select!` against each `bytes_stream().next()`. On cancel: `stop_reason = Aborted`, keep the partial content, emit `Error`.
- **HTTP:** `reqwest` with the `stream` feature, plus hand-rolled SSE. Pi already hand-rolls Anthropic's SSE in about 120 lines (`anthropic:400-528`); `eventsource-stream` over `bytes_stream()` also fits. Do not use `reqwest-eventsource`, because it has its own retry and reconnect semantics. [INFERENCE]
- **Retry:** port `provider-retry` (≈60 lines) around the request future only, with `tokio::time::sleep` raced against cancellation. Port `retry.rs` and `overflow.rs` as pure functions plus `regex` `RegexSet`s. **Pitfall:** pattern `(?<![a-z0-9])(?:429|…)(?![a-z0-9])` (`utils/retry.ts:44`) uses lookaround, which the `regex` crate rejects. Rewrite it as `(?:^|[^a-z0-9])(?:429|500|…)(?:$|[^a-z0-9])`, or use `fancy-regex` for that one pattern.
- **Partial JSON:** `jiter` has a partial mode (`PartialMode::TrailingStrings`) [INFERENCE: verify API]. Otherwise write the ~80-line "close open strings, brackets and braces" fixer. Port `repairJson` verbatim (≈50 lines). Note that `parseStreamingJson` runs on **every delta**, which is O(n²) over a long argument string. Either throttle it (parse only when a consumer asks) or accept the cost; the axo loop only needs the final arguments plus raw deltas for UI. [INFERENCE]
- **JSON key order: real pitfall.** Enable `serde_json` `preserve_order`.
  - Without it, `Map` is a `BTreeMap` and reorders tool arguments and schemas.
  - Replayed `tool_calls[].function.arguments = JSON.stringify(args)` (`completions:1388`) and Anthropic `input` would then differ byte-for-byte from what the model produced. That breaks the prompt-cache prefix and changes what the model sees.
- **Surrogates:** Rust `String` cannot hold unpaired surrogates, so `sanitizeSurrogates` disappears on output. On *input*, `serde_json` rejects lone `\uD800` escapes. Provider SSE may contain them in broken streams; decide whether to decode lossily or fail the stream. [INFERENCE]
- **Token estimates:** TS `.length` counts UTF-16 units. Use `chars().count()` or bytes/4; it is only an estimate, but keep the 3.5 divisor for parity.
- **Compat:** `#[derive(Deserialize, Default)] struct OpenAiCompletionsCompat { supports_store: Option<bool>, … }` plus `fn resolve(model) -> ResolvedCompat` (detect, then override). Keep it a flat struct of `Option`s; no trait per vendor.
- **Auth:**
  - `trait CredentialStore { async fn read(..); async fn modify(&self, id, f: Box<dyn FnOnce(Option<Credential>) -> BoxFuture<Option<Credential>>>); … }`.
  - `trait ApiKeyAuth { async fn resolve(&self, ctx, cred) -> Result<Option<AuthResult>> }`.
  - `trait OAuthAuth { async fn refresh(..); async fn to_auth(..) -> ModelAuth; async fn login(..) }`.
  - Per-provider `tokio::sync::Mutex` map for `modify`; refresh under the lock; refresh future not cancelled by the caller (spawn it, then await the `JoinHandle` with `select!` on cancel).
  - The Bun extension implements `OAuthAuth` over the bridge, and only `ModelAuth` crosses it. This matches design.md's split exactly, with the Claude-Code OAuth caveat in §3.9.
- **Registry:** `struct Models { providers: HashMap<String, Arc<Provider>>, credentials: Arc<dyn CredentialStore> }` and `struct Provider { id, base_url, headers, auth: ProviderAuth, models: Vec<Model>, apis: HashMap<ApiId, Arc<dyn Api>> }`. `ApiId` is a string newtype so extensions can register custom APIs.
- **Crates:** `serde`/`serde_json` (preserve_order), `reqwest` (rustls, stream), `tokio`, `tokio-util`, `futures`, `async-stream` (optional), `regex`, `thiserror`, `sha2` or a small hash (completions ID shortening, `shortHash`), `jiter` (optional). Do not use the `async-openai`/Anthropic SDK crates: pi bypasses SDK retries and parses SSE itself anyway. [INFERENCE]
- **Other pitfalls:**
  - Anthropic `contentIndex` ≠ the wire `index`; map it.
  - Signatures arrive via `signature_delta` with **no event**, so a consumer that snapshots only on deltas misses them. Use the `Done` message as the source of truth.
  - Completions emits all `*_end` events at stream end, not interleaved.
  - `start` is emitted only after HTTP headers arrive; setup errors come with no `start`.
  - An unknown Anthropic stop reason throws; mirror it with an error rather than defaulting to `stop`.
  - Preserve `rawStopReason`, `responseId` and `responseModel`; they are cheap and useful.

---

## 6. Size

Counts from `wc -l` (raw) / non-blank, non-comment lines (code):

| File | raw | code |
|---|---:|---:|
| types.ts | 905 | 563 |
| utils/event-stream.ts | 136 | 107 |
| api/lazy.ts | 124 | 101 |
| **api/anthropic-messages.ts** | 1691 | 1417 |
| **api/openai-completions.ts** | 1745 | 1513 |
| api/openai-responses.ts + openai-responses-shared.ts | 460 + 888 | 396 + 782 |
| api/transform-messages.ts | 257 | 185 |
| utils/transcript.ts | 264 | 167 |
| utils/text.ts | 41 | 29 |
| utils/json-parse.ts | 150 | 112 |
| api/simple-options.ts | 110 | 94 |
| api/constrained-sampling.ts (defer) | 299 | 262 |
| utils/provider-retry.ts | 129 | 102 |
| utils/retry.ts | 272 | 158 |
| utils/overflow.ts | 194 | 67 |
| utils/estimate.ts | 118 | 94 |
| utils/error-body.ts | 150 | 82 |
| models.ts (Models, createProvider, cost, thinking clamp) | 1300 | 1014 |
| model-catalog.ts / models-store.ts | 82 / 47 | 69 / 31 |
| providers/model-schema.ts / compat-schema.ts | 100 / 645 | 88 / 601 |
| auth/types + resolve + credential-store + helpers + context | 256+218+68+60+46 | 109+171+55+44+34 |
| providers/all.ts / providers/anthropic.ts | 191 / 91 | 166 / 81 |

The whole `packages/ai/src` is 20,643 raw lines. Large unrelated files: Codex Responses 1724, Bedrock 1428, Mistral 952, compat-schema 644.

### Verdict: Rust lines to port [INFERENCE]

Counts are core-only, following the skip list. Serde derives and `match` compress the TS a lot; the vendor branches dominate the adapters.

| Slice | TS code in scope | Rust estimate |
|---|---:|---:|
| Type model (messages, blocks, usage, model, compat structs, events, accumulator) | ~800 | **450–600** |
| Transcript, system replay, transform-messages, text, json-parse/repair, estimate, cost, thinking clamp, simple-options | ~750 | **500–650** |
| Retry (HTTP + turn), overflow, error normalization | ~410 | **250–350** |
| Auth (store trait + in-memory store, resolve, OAuth locked refresh, env helper) + `Models`/`Provider` registry + header merge | ~600 of 1,400 | **400–550** |
| **Anthropic Messages** (SSE, params, convert, stream state, cache control, thinking/redacted/signature, betas, OAuth branch; no federation, fallback, mid-conversation effort/tools or strict fallback) | ~900 of 1,417 | **650–850** |
| **OpenAI Chat Completions** (params, 3–4 thinking formats, reasoning fields and `reasoning_details`, tool deltas by index, usage, compat detect, convert, cache control; no grammar tools or vendor routing) | ~1,000 of 1,513 | **650–850** |
| **Total** | | **≈2,900–3,850** |

Add about 400–600 lines for OpenAI Responses later. About 150 lines of focused regression checks are worth keeping:
- `transformMessages` orphan and foreign-thinking rules.
- Usage parsing.
- Overflow/retry regex parity.
- Partial-JSON repair.
