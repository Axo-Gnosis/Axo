# Message models: pi-ai vs genai vs Rig vs Vercel AI SDK vs raw wire formats

Sources (clones in `/tmp/axo-research-MessageModels/`):
- **pi-ai** v1.3.0: `/tmp/pi-src/packages/ai/src` (paths below are relative to it unless prefixed).
- **genai** `0.7.1-WIP`, commit `06eca21` (2026-10-19): `genai/src/...`
- **Rig** workspace `0.45.0`, commit `65aec1f` (2026-10-21): `rig/crates/rig-core/src/...`
- **Vercel AI SDK** `ai@7.0.148`, commit `36e4ce7` (2026-10-21). The provider spec is `LanguageModelV4`: `vercel-ai/packages/...`

## 1. Shapes

### pi-ai (what axo's `core.md` already adopts)
```ts
Message = SystemMessage | UserMessage | AssistantMessage | ToolResultMessage     // types.ts:552-650
SystemMessage    { content: string|Text[]; sections?: {name: string|null}; toolsAdded?: Tool[]; toolsRemoved?; timestamp }
UserMessage      { content: string|(Text|Image)[]; timestamp }
AssistantMessage { content: (Text|Thinking|ToolCall)[]; api; provider; model; responseModel?; responseId?;
                   thinkingLevel?; diagnostics?; usage: Usage; stopReason; errorMessage?; rawStopReason?; timestamp; durationMs? }
ToolResultMessage{ toolCallId; toolName; content: (Text|Image)[]; details?; isError; timestamp; durationMs? }
Text     { text; textSignature? }                                  // types.ts:420-424
Thinking { thinking; thinkingSignature?: string; redacted?: bool }  // types.ts:426-434, one opaque string slot
Image    { data: base64; mimeType }                                 // types.ts:436-440
ToolCall { id; name; arguments: JsonObject; thoughtSignature?; namespace?; argumentsError? }  // types.ts:442-455
Usage    { input; output; cacheRead; cacheWrite; cacheWrite1h?; reasoning?; totalTokens; cost{input,output,cacheRead,cacheWrite,total} } // :457-478
StopReason = pending|stop|length|toolUse|error|aborted|deferred     // :480
```
Design: a **flat, provider-neutral transcript with typed per-provider string slots** (`thinkingSignature`, `textSignature`, `thoughtSignature`, `namespace`). Every assistant message records which `api/provider/model` produced it, and that record drives the replay rules.

### genai
```rust
ChatRequest { system: Option<String>, messages: Vec<ChatMessage>, tools: Option<Vec<Tool>>, previous_response_id, store }  // chat_request.rs:11-31
ChatMessage { role: System|User|Assistant|Tool, content: MessageContent /*Vec<ContentPart>*/, options: Option<MessageOptions{cache_control}> } // chat_message.rs:9-171
ContentPart = Text(String) | Binary(Binary{content_type, source: Url|Base64, name}) | ToolCall(ToolCall)
            | ToolResponse(ToolResponse) | ThoughtSignature(String) | ReasoningContent(String) | Custom(CustomPart{model_iden?, data: Value}) // content_part/common.rs:13-38
ToolCall     { call_id, fn_name, fn_arguments: Value, thought_signatures: Option<Vec<String>> }   // tool/tool_call.rs:9-28
ToolResponse { call_id, fn_name: Option<String>, content: String }                              // tool/tool_response.rs:8-20
ChatResponse { content, reasoning_content: Option<String>, model_iden, provider_model_iden, stop_reason, usage, captured_raw_body, response_id } // chat_response.rs:103-133
Usage { prompt_tokens, prompt_tokens_details{cache_creation_tokens, cache_creation_details{5m,1h}, cached_tokens, audio}, completion_tokens,
        completion_tokens_details{reasoning_tokens,…}, total_tokens }   // usage.rs:15-99, OpenAI-shaped, all Option<i32>
ChatStreamEvent = Start | Chunk | ReasoningChunk | ThoughtSignatureChunk | ToolCallChunk(ToolCall) | End(StreamEnd) | Heartbeat // chat_stream.rs:157-179
```
Design: a **request/response client**. The message is a bag of parts with no provenance. Reasoning text and signatures are *separate parallel parts* that are paired by position at replay (`anthropic/adapter_shared.rs:266-315`). All metadata lives on `ChatResponse`, not on the message, so it is lost once the response becomes a `ChatMessage`.

### Rig (rig-core 0.45)
```rust
Message = System{content: String} | User{content: Vec<UserContent>} | Assistant(AssistantMessage)   // completion/message.rs:15-25
AssistantMessage { content: Vec<AssistantContent>, origin: Option<Origin>, stop: Option<StopReason> } // :45-54
UserContent      = Text | ToolResult | Image | Audio | Video | Document                                // :186-199 (tool results ride in a user message)
AssistantContent = Text | ToolCall | Reasoning | Image | Opaque                                        // :206-217
Reasoning { text, redacted: bool, native: Option<Native> }                                             // :373-386
ToolCall  { id: CallId(Provider|Local uuid), function: ToolFunction{name, arguments: Map, invalid_arguments: Option<String>}, native } // :480-562
ToolResult{ call, name, content: Vec<Text|Image|Json{value}>, is_error }                              // :403-429
Native { item: serde_json::Value /*verbatim provider item*/, fingerprint: Fingerprint /*FNV of the canonical block*/ } // message/native.rs:238-243
Opaque { item: Value, replay: bool }                                   // native.rs:315-321 (hosted-tool steps, compaction records)
Origin { api, provider, model, response_model?, response_id?, context: Option<Fingerprint /*tools+system*/> } // native.rs:61-81
StopReason = Stop | Length | ToolUse | Error(String) | Aborted(String)  // native.rs:111-122
Usage { input, output, total, cached_input, cache_creation_input, tool_use_prompt, reasoning, cost: Option<Cost> } // completion/request.rs:679-713
StreamEvent = Start{part,kind} | Text | Reasoning | Arguments{raw JSON fragment} | End{part, content}   // streaming/event.rs:81-121
```
Design: **canonical blocks, each carrying the verbatim wire item it was decoded from plus a fingerprint**. An edited block stops replaying its stale item (`native.rs:231-236`). A 30-method `ReplayTarget` trait and `history::adapt` (`completion/history.rs:178-483`) do the cross-model work. Rig 0.45 looks heavily pi-influenced: the same `same_model(api, provider, model)` rule (`native.rs:97-101`), the same `"No result provided"` (`conversation/pairing.rs:76`), and the same `"(see attached image)"` / `"Attached image(s) from tool result:"` strings (`history.rs:103-106`).

### Vercel AI SDK 7 (`ModelMessage` → `LanguageModelV4Prompt`)
```ts
LanguageModelV4Message = ({role:'system', content:string} | {role:'user', content:(Text|File)[]}
  | {role:'assistant', content:(Text|File|Custom|Reasoning|ReasoningFile|ToolCall|ToolResult)[]}
  | {role:'tool', content:(ToolResult|ToolApprovalResponse)[]}) & { providerOptions? }        // provider/src/language-model/v4/language-model-v4-prompt.ts:20-59
ReasoningPart { text; providerOptions? }   // :83-97, signature/redactedData/itemId/encryptedContent all live in providerOptions[providerName]
ToolCallPart  { toolCallId; toolName; input: unknown; providerExecuted?; providerOptions? }         // :194-224
ToolResultOutput = text | json | execution-denied | error-text | error-json | content[(text|file|custom)] // :288-410
Usage { inputTokens{total,noCache,cacheRead,cacheWrite}; outputTokens{total,text,reasoning}; raw? }  // v4-usage.ts:6-59
FinishReason { unified: stop|length|content-filter|tool-calls|error|interrupted|other; raw }        // v4-finish-reason.ts:8-35
StreamPart: text-*/reasoning-*/tool-input-start|delta|end/tool-call/finish{usage,finishReason}/error/raw // v4-stream-part.ts:15-119
```
User-facing `ModelMessage` has the same roles; each part and each message carries only `providerOptions` (`provider-utils/src/types/*-model-message.ts`). Design: **a provider-neutral skeleton plus an open `providerOptions: {[provider]: {...}}` side-channel** on every part. Each provider package reads only its own namespace. Model, usage, finish reason and timestamps live on `StepResult` (`ai/src/generate-text/step-result.ts:160,309-340`), not on messages.

### Raw wire format as canonical
- **OpenAI Chat Completions:** roles `system|developer|user|assistant|tool`. The assistant has `content` (a string) and `tool_calls[{id, function:{name, arguments: string}}]`. A tool message is `{tool_call_id, content}` holding text only. There is no standard reasoning field: vendors add `reasoning_content`, `reasoning`, `reasoning_text` or `reasoning_details[]` (pi handles all four: ai.md §3.5). Usage is `prompt_tokens(_details.cached_tokens)` and `completion_tokens(_details.reasoning_tokens)`.
- **Anthropic Messages:** `system` is top-level. Messages are `user|assistant` with blocks `text|image|document|tool_use|tool_result|thinking{thinking,signature}|redacted_thinking{data}`. Tool results are user blocks and may contain images. Usage is `input_tokens`, `output_tokens`, `cache_creation_input_tokens` (with a 5m/1h split) and `cache_read_input_tokens`, with no reasoning-token count. Mid-conversation system messages and tool changes exist only behind 2026 betas (Vercel uses `mid-conversation-tool-changes-2026-07-01`: `anthropic/src/convert-to-anthropic-prompt.ts:184-289`).
- Neither format can hold the other's replay data, such as OpenAI Responses reasoning items or Gemini `thoughtSignature`, except as a JSON blob on the side. Neither has cost, timestamps, abort, or origin.

## 2. Property table

| Property | pi-ai | genai | Rig 0.45 | Vercel AI SDK 7 | Raw OpenAI Chat | Raw Anthropic |
|---|---|---|---|---|---|---|
| **Thinking + opaque signature** | `Thinking{thinking, thinkingSignature}`; signature built from `signature_delta` (ai.md §3.4) | Separate `ReasoningContent` + `ThoughtSignature` parts, paired **by position**; a count mismatch drops all thinking with a warn (`anthropic/adapter_shared.rs:289-315`) | `Reasoning{text}` + `native.item` holds the whole wire block; blob located per API (`native.rs:251-299`) | `reasoning` part + `providerOptions.anthropic.signature` (`convert-to-anthropic-prompt.ts:829-841`) | None (no signed reasoning) | Native `thinking{signature}` |
| **Redacted thinking replay** | `redacted:true`, payload in `thinkingSignature` (`types.ts:430-433`) | Bedrock only (`bedrock/converse.rs:153-157`); no `redacted` handling in the Anthropic adapter (`git grep` finds none) [INFERENCE: dropped] | `redacted:bool` + native `/data` (`native.rs:253`) | `providerOptions.anthropic.redactedData` → `redacted_thinking` (`:842-853`) | n/a | Native `redacted_thinking{data}` |
| **Per-provider opaque replay** (Responses reasoning items, `reasoning_content` field name, OpenRouter `reasoning_details`, Gemini `thoughtSignature`) | Packed into the one string slot: a Responses item as JSON, the **field name** for `reasoning_content`/`reasoning`, `reasoning_details` JSON; `ToolCall.thoughtSignature`; `textSignature` for Responses message ids (ai.md §3.5-3.6) | Responses `encrypted_content` → `ThoughtSignature` parts, replayed as `{type:"reasoning"}` (`openai_resp/adapter_impl.rs:542-571`); always echoes `reasoning_content` (Mistral: `thinking` chunk) (`openai/adapter_shared.rs:476-487`); `reasoning_details` → `Custom` parts. **Blobs carry no origin.** | Whole verbatim item in `Native` (any API); `Opaque` for items with no canonical meaning; `needs_next`, `identity`, `call_id_slot` hooks (`history.rs:252-312`) | `providerOptions.openai.{itemId, reasoningEncryptedContent}` (`openai/src/responses/convert-to-openai-responses-input.ts:1275-1395`); openai-compatible echoes `reasoning_content` (`convert-to-openai-compatible-chat-messages.ts:252-288`) | Whichever vendor field the provider uses | Only Anthropic's own |
| **Tool-arg streaming** | `toolcall_delta` + live `arguments` parsed through strict→repair→partial-json (ai.md §3.3) | `ToolCallChunk` with the **accumulated** args as `Value::String` (`anthropic/streamer.rs:195-256`) | `Arguments{raw fragment}` + `parse_partial_arguments` helper (`streaming/event.rs:110-116`) | `tool-input-start/delta/end` (raw string deltas), then `tool-call` (`v4-stream-part.ts:55-80`) | Wire deltas only | Wire `input_json_delta` only |
| **Malformed args** | `arguments={}` + `argumentsError` with raw text; JSON repair first (`types.ts:450-454`) | Falls back to `Value::String(raw)` + warn (`anthropic/streamer.rs:490-502`); non-streaming OpenAI returns an error (`openai/adapter_impl.rs:278-283`) | `arguments` Map + `invalid_arguments: Option<String>` (`message.rs:557-561`) | Optional `repairToolCall` hook, else an invalid call marked `invalid:true` + error (`ai/src/generate-text/parse-tool-call.ts:61-127`) | `arguments` is a string, so anything goes | `input` must be an object |
| **Usage: cache read/write, reasoning** | `cacheRead`, `cacheWrite`, `cacheWrite1h`, `reasoning`, all normalized (`types.ts:457-470`) | Yes, OpenAI-shaped `Option<i32>`s incl. 5m/1h (`usage.rs:15-99`) | Yes + `tool_use_prompt_tokens` (`request.rs:679-702`) | Yes: `inputTokens{noCache,cacheRead,cacheWrite}`, `outputTokens{reasoning}`, `raw` (`v4-usage.ts`) | read + reasoning; write only from some vendors | read/write (+5m/1h); no reasoning count |
| **Cost** | `Usage.cost` USD, recomputed on every usage update from `Model.cost`, tiers, 1h=2× (ai.md §3.14) | None (no pricing code; only doc links) | `Usage.cost: Option<Cost>` from catalog list price or provider figure; separate `CacheCost` (`request.rs:703-712`, `cache_cost.rs`) | Not in core [INFERENCE: no `cost` in step-result] | No | No |
| **Images in user / tool results** | User ✓; tool result ✓ (`(Text\|Image)[]`); adapters split them out for wires that can't carry them, and placeholders are used for non-vision models (ai.md §3.5, §3.7) | User ✓ (`Binary`); tool result ✗ (`content: String`) | User ✓ (+audio/video/doc); tool result ✓; `adapt` moves or replaces them per `Accepts` (`history.rs:77-147`) | User ✓; tool result ✓ via `content[file]` (`prompt.ts:344-409`) | User ✓; tool ✗ | User ✓; tool ✓ |
| **System prompt / tool changes in transcript** | ✓ First-class: `SystemMessage{sections, toolsAdded, toolsRemoved}` patches; sent in place or collapsed per compat (ai.md §1) | System role allowed but Anthropic **hoists** all of it into top-level `system` (`anthropic/adapter_shared.rs:175-178`); tools only on the request | `Message::System` anywhere; `LaterSystem::{InPlace, Leading, UserText}` per target (`history.rs:331-381`); tools on the request only, with an `Origin.context` fingerprint of tools+system that invalidates native items (`native.rs:74-80`) | System anywhere; Anthropic provider sends mid-conversation system + `toolChanges` via providerOptions under a beta, else hoists (`convert-to-anthropic-prompt.ts:184-289`) | Any position (role `system`/`developer`), tools on the request | Top-level only (betas aside) |
| **Cross-provider transform** | `transformMessages`: drop foreign thinking, strip foreign signatures, normalize ids via an adapter callback, synthesize `"No result provided"` for orphans, skip error/aborted turns, hold system msgs between call and result (`transform-messages.ts:91-256`) | None central. Only per-adapter defenses (unpaired-signature drop). No orphan or id handling (`git grep orphan\|normalize_tool` finds none) | `history::adapt`: same-model keeps natives; **foreign reasoning → plain text by default** (or reasoning if `foreign_reasoning`); id normalize + dedupe; orphan calls get `"No result provided"`, orphan results dropped; failed turns dropped (`history.rs:457-483`) | None central. Each provider ignores other namespaces and **warns+drops** unsigned reasoning (`convert-to-anthropic-prompt.ts:854-864`); missing results **throw** `MissingToolResultsError` (`ai/src/prompt/convert-to-language-model-prompt.ts`) | n/a (you'd write it) | n/a |
| **Per-message metadata** | On the message: `api, provider, model, responseModel, responseId, stopReason, rawStopReason, usage, timestamp, durationMs` (+ `timestamp` on every role) | None on `ChatMessage`; `model_iden`, `provider_model_iden`, `stop_reason`, `usage`, `response_id` on `ChatResponse` only | `Origin{api, provider, model, response_model, response_id}` + `stop` on the message; **no usage, timestamps or duration** on the message | None on `ModelMessage`; `StepResult` has model, usage, finish reason, response `{id, timestamp, modelId}` | None | None |
| **Abort / error in message** | `stopReason: error\|aborted` + `errorMessage`, **partial content kept**; the stream never throws after it returns (ai.md §3.2) | Stream yields `Err`; no aborted state; partial content only if capture is on | `StopReason::Error(String) \| Aborted(String)`; such turns are kept but never replayed (`native.rs:104-128`) | Stream `error` part; unified finish `error`/`interrupted`; not stored on `ModelMessage` | No | No |

## 3. pi-ai providers by wire API

The `KnownApi` chat set is defined at `types.ts:62-72`. Providers are from `providers/*.ts`, registered in `providers/all.ts:135-180` (42 total).

| Wire API (`api/*.ts`) | Providers using it |
|---|---|
| `openai-completions` (`api/openai-completions.ts`) | ant-ling, azure, baseten, cerebras, cloudflare-ai-gateway, cloudflare-workers-ai, deepseek, fireworks, github-copilot, groq, huggingface, kimi-coding, moonshotai, moonshotai-cn, nvidia, opencode, opencode-go, openrouter, qwen-token-plan(-cn, -individual), together, xiaomi, xiaomi-token-plan-{ams,cn,sgp}, zai, zai-coding-cn |
| `openai-responses` (`api/openai-responses.ts`, shared `openai-responses-shared.ts`) | openai, meta, xai, github-copilot, cloudflare-ai-gateway, opencode, opencode-go |
| `azure-openai-responses` | azure |
| `openai-codex-responses` | openai-codex (ChatGPT subscription) |
| `anthropic-messages` | anthropic, minimax, minimax-cn, fireworks, github-copilot, cloudflare-ai-gateway, opencode, opencode-go, openrouter, vercel-ai-gateway |
| `bedrock-converse-stream` | amazon-bedrock |
| `google-generative-ai` (shared `google-shared.ts`) | google, opencode, opencode-go |
| `google-vertex` | google-vertex |
| `mistral-conversations` | mistral |
| `pi-messages` (pi's own event protocol over SSE, `api/pi-messages.ts:1-10`) | radius (and any custom `models.json` provider) |
| Non-chat (skip for axo) | `openrouter-images` (openrouter); classifiers `llama-cpp-classify`, `typesafe-system-one`, `cloudflare-workers-ai-system-one`; `openai-decisions` (openai) |

Wrappers rather than APIs: `cloudflare-stream.ts` wraps the completions, responses and anthropic adapters (`providers/cloudflare-ai-gateway.ts:21-24`); `faux.ts` is a test provider. So **five wire families** cover nearly everything: Chat Completions, Responses (×3 variants), Anthropic Messages, Google (×2), and Bedrock Converse, plus Mistral and pi-messages.

For comparison, genai's `AdapterKind` has ~36 variants (`src/adapter/adapter_kind.rs`), and Rig's chat wires are `openai.chat`, `openai.responses`, `anthropic.messages`, `gemini.generate_content`, `gemini.interactions`, `vertexai.generate_content`, `bedrock.converse`, `cohere.chat`, `ollama.chat` (`Api::from_static` grep).

## 4. Assessment per alternative

**genai.** Good HTTP coverage and active, but its model is the weakest fit:
- Signatures are positional parallel parts with no origin. An OpenAI encrypted blob and an Anthropic signature are both `ThoughtSignature(String)`, so a cross-provider replay can't tell them apart [INFERENCE: it can mis-pair if counts happen to match].
- Tool results are text only.
- There is no cost, no abort state, and no message metadata, and the system prompt is hoisted.

axo would need its own transcript type anyway, plus a conversion layer with a lossy middle. The design.md concern about plugin auth injection is secondary to this.

**Rig 0.45.** The most sophisticated model of the alternatives, and it converged on pi's rules (same-model sameness, orphan synthesis, failed-turn skipping, tool-image relocation).
- Ahead of pi:
  - It keeps the **whole verbatim provider item + fingerprint**, which is more general than pi's single `thinkingSignature` string and survives edits safely.
  - It types `CallId::Local` for missing ids.
  - Its `Origin.context` fingerprint guards against replaying items across tool/system changes.
- Behind pi for axo:
  - There are no usage, timestamps or duration on messages.
  - There is no in-transcript tool add/remove.
  - Foreign reasoning becomes **plain text by default**, the opposite of pi's deliberate drop (`transform-messages.ts:129-132`).
  - The 30-hook `ReplayTarget` surface and pre-1.0 churn (0.45, `MIGRATING.md`) make depending on it costly.

Worth **borrowing ideas from**, not depending on.

**Vercel AI SDK.** It is TypeScript, so the only option is to copy its shape. `providerOptions[provider]` is a clean, extensible side-channel. But:
- Cross-provider handling is "each provider ignores others and warns", there is no central transform, and orphans throw.
- Metadata lives outside the message (`StepResult`), which conflicts with axo's "the session tree is the source of truth" (design.md:165).

**Raw wire as canonical.** OpenAI Chat can't carry signed thinking, redacted thinking or tool-result images. Anthropic can't carry Responses items, Gemini signatures, reasoning-token counts or the `reasoning_content` field name. Neither carries cost, timestamps, abort or origin. Every non-native provider becomes a lossy translation, and history portability is worse than any of the above. Rejected.

## 5. Recommendation for axo

1. **Keep pi-ai's message model as canonical** (as `core.md` already says). It is the only candidate that has all of these on the message itself: in-transcript system/tool patches, full metadata, cost, and abort/error with partial content. Those are exactly what axo's session tree, compaction and branching need. No library type can replace it without a second model and a lossy conversion.
2. **Port pi's adapters rather than wrap genai or Rig.** Neither library's types survive the round-trip (genai: positional signatures, text-only tool results; Rig: no usage or timestamps on messages, foreign reasoning as text).
3. **Borrow two ideas from Rig, cheaply, inside pi's shape:**
   - Make the opaque slot structured instead of overloading one string. For example, `Thinking.signature: Option<Replay>` with `Replay { api: Api, data: serde_json::Value }`, or keep pi's string but always tag it with the producing `api`. This removes pi's "field name stored as signature" trick (ai.md §3.5), which a Rust enum can express directly: `ReasoningField(String) | AnthropicSig(String) | ResponsesItem(Value) | OpenRouterDetails(Value)`.
   - Optionally, fingerprint tools+system on the assistant message so a replay after an in-transcript tool change treats it as foreign (Rig `Origin.context`). Defer this unless a provider actually rejects such replays.
4. **Keep pi's behavior choices where Rig differs:** drop foreign thinking (don't textify it), skip error/aborted turns, and synthesize `"No result provided"`. design.md item 3 (recording aborted results in the loop) makes the last one mostly a safety net.
5. **Scope:** the five wire families (Chat Completions with its compat flags, Responses, Anthropic Messages, Google, Bedrock) cover all 42 pi providers. Start with Anthropic Messages + Chat Completions, as `core.md` plans; Responses is next because OpenAI, xAI and Copilot need it.
