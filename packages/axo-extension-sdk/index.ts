// The author contract for axo extensions. Types only: nothing here runs, and an extension
// never needs this package at runtime (`import type` / `satisfies` is erased).
//
//   import type { Extension } from "@axo/extension-sdk";
//   export default (async ({ id, dataDir, session }) => ({
//     providers: { lab: { login, refresh, rewrite } },
//     tools: {
//       echo: async ({ call, update }) => {
//         update({ content: [{ type: "text", text: "working" }] });
//         return { content: [{ type: "text", text: JSON.stringify(call.arguments) }] };
//       },
//     },
//     commands: {
//       hello: (args, session) =>
//         session.post({ customType: "hello", text: args, display: true, delivery: "quiet" }),
//     },
//   })) satisfies Extension;
//
// Everything the model or the host sees is declared statically in `extension.json`; the
// default export only binds handlers to those declarations, and activation fails unless they
// match both ways:
// - `providers` is keyed by the provider IDs declared there. A provider declared with
//   `"login": true` must supply both `login` and `refresh`; `"rewrite": true` requires
//   `rewrite`.
// - `tools`, `schemes` and `commands` are keyed by the declared names, one handler each.
// - `toolPolicy` is present exactly when the manifest's `"toolPolicy"` lists hooks, and defines
//   exactly the listed hooks (`"before"`, `"after"`). `spawnHook` and `onEntry` are present
//   exactly when the manifest sets `"spawnHook"` and `"events"`.
// Unknown keys, undeclared names and missing handlers fail activation.

/** Any JSON value. */
export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

// ---- Model (the Rust `axo_ai::Model` serde shape) ----

export type ApiKind = "anthropic-messages" | "openai-completions" | "openai-codex-responses";

export type InputKind = "text" | "image";

export type ThinkingLevel = "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";

/** Provider value per thinking level: a missing key uses the provider default; `null` marks the level unsupported. */
export type ThinkingLevelMap = Partial<Record<ThinkingLevel, string | null>>;

/** Prices in USD per million tokens. */
export interface Cost {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
  /** Request-wide tiers; the highest matching input threshold applies to the whole request. */
  tiers?: CostTier[];
}

export interface CostTier {
  inputTokensAbove: number;
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

export type ChatTemplateValue =
  | { $var: "thinking.enabled" | "thinking.effort" | "thinking.budget"; omitWhenOff?: boolean }
  | JsonValue;

/** Compatibility quirks; an absent field means "auto-detect / API default". */
export interface Compat {
  // Shared.
  supportsDeveloperRole?: boolean;
  supportsMidConvoSystemMessages?: boolean;
  supportsMidConvoEffort?: boolean;
  sessionAffinityFormat?: "openai" | "openai-nosession" | "openrouter";
  sendSessionAffinityHeaders?: boolean;
  supportsStrictMode?: boolean;

  // OpenAI Completions.
  supportsStore?: boolean;
  supportsReasoningEffort?: boolean;
  supportsUsageInStreaming?: boolean;
  supportsFinishReason?: boolean;
  maxTokensField?: "max_completion_tokens" | "max_tokens";
  requiresToolResultName?: boolean;
  requiresAssistantAfterToolResult?: boolean;
  requiresThinkingAsText?: boolean;
  requiresReasoningContentOnAssistantMessages?: boolean;
  thinkingFormat?:
    | "openai"
    | "openrouter"
    | "deepseek"
    | "together"
    | "baseten"
    | "zai"
    | "qwen"
    | "chat-template"
    | "qwen-chat-template"
    | "string-thinking"
    | "ant-ling";
  chatTemplateKwargs?: Record<string, ChatTemplateValue>;
  chatTemplateArgs?: Record<string, ChatTemplateValue>;
  openRouterRouting?: JsonValue;
  vercelGatewayRouting?: JsonValue;
  zaiToolStream?: boolean;
  thinkingTokenBudgetField?: "thinking_token_budget" | "thinking_budget" | "thinking_budget_tokens";
  supportsThinkingTokenBudget?: boolean;
  supportsMidConvoToolAdditions?: boolean;
  cacheControlFormat?: "anthropic";
  vllmPriority?: number;

  // OpenAI Responses.
  supportsAdditionalTools?: boolean;
  supportsToolSearch?: boolean;

  // Anthropic Messages.
  supportsEagerToolInputStreaming?: boolean;
  supportsCacheControlOnTools?: boolean;
  supportsTemperature?: boolean;
  forceAdaptiveThinking?: boolean;
  allowEmptySignature?: boolean;
  supportsMidConvoToolChanges?: boolean;
}

export interface Model {
  id: string;
  name: string;
  api: ApiKind;
  provider: string;
  baseUrl: string;
  reasoning: boolean;
  thinkingLevelMap?: ThinkingLevelMap;
  input: InputKind[];
  cost: Cost;
  contextWindow: number;
  maxTokens: number;
  compat: Compat;
}

// ---- Credentials (the Rust `axo_ai::auth::Credential` serde shape) ----

export interface ApiKeyCredential {
  type: "api_key";
  key: string;
}

export interface OAuthCredential {
  type: "oauth";
  access: string;
  refresh: string;
  /** Expiry in milliseconds since the epoch. */
  expires: number;
  /** Extra request headers this credential authorizes with. */
  headers?: Record<string, string>;
  /** Base URL override for requests made with this credential. */
  baseUrl?: string;
  /** Provider-specific fields, stored flattened beside the standard ones. */
  [field: string]: unknown;
}

export type Credential = ApiKeyCredential | OAuthCredential;

// ---- Request rewrite ----

/** A UTF-8 header value as a string; any other bytes as `{ base64 }`. */
export type HeaderValue = string | { base64: string };

/** A provider request as axo's standard adapter built it, before axo sends it. */
export interface ProviderRequest {
  /** `http:` or `https:` only. */
  url: string;
  /** Ordered `[name, value]` pairs; duplicate names are separate values. */
  headers: [name: string, value: HeaderValue][];
  body: JsonValue;
}

// ---- Login interaction (the host UI) ----

export interface DeviceCode {
  userCode: string;
  verificationUrl: string;
  expiresInSeconds?: number;
}

export interface LoginPrompt {
  message: string;
  placeholder?: string;
  /** The answer is a secret; the host masks it. */
  secret: boolean;
}

export interface LoginOption {
  id: string;
  label: string;
}

export interface LoginSelect {
  message: string;
  options: LoginOption[];
}

/**
 * What a login asks of the host UI. Display methods resolve once the host has shown the
 * request. `prompt` and `select` resolve with the answer (`select` with the chosen option
 * `id`); their optional signal withdraws that one question, e.g. when a browser callback
 * wins. The login's own signal withdraws every outstanding question.
 */
export interface LoginInteraction {
  openUrl(url: string, instructions?: string): Promise<void>;
  deviceCode(code: DeviceCode): Promise<void>;
  prompt(prompt: LoginPrompt, signal?: AbortSignal): Promise<string>;
  select(select: LoginSelect, signal?: AbortSignal): Promise<string>;
  /** A non-secret status line for the login in progress. */
  progress(message: string): Promise<void>;
}

// ---- Provider handlers and the default export ----

type LoginHandlers =
  | {
      /** Runs the login; axo stores the returned credential. */
      login(ui: LoginInteraction, signal: AbortSignal): Promise<Credential>;
      /** Exchanges an expiring OAuth credential for a fresh one, under axo's credential lock. */
      refresh(credential: Credential, signal: AbortSignal): Promise<Credential>;
    }
  | { login?: never; refresh?: never };

interface RewriteHandler {
  /** Returns the complete replacement request; axo still sends it and parses the response. */
  rewrite?(model: Readonly<Model>, request: ProviderRequest, signal: AbortSignal): Promise<ProviderRequest>;
}

export type ProviderHandlers = LoginHandlers & RewriteHandler;

// ---- Sessions ----

/** A user-visible content block (the Rust `UserBlock` serde shape). */
export type UserBlock = { type: "text"; text: string } | { type: "image"; data: string; mimeType: string };

/**
 * Acts on one session while the host has it attached; calls on a session the host no longer
 * tracks reject.
 */
export interface SessionHandle {
  id: string;
  /**
   * Adds a custom message to the session's context. `steer` lands at the next turn boundary,
   * `followUp` when the active run would end (both start a run when idle); `quiet` waits for
   * the next turn boundary or run start and never starts a run. The host stores the type as
   * `<extension id>:<customType>`.
   */
  post(message: {
    customType: string;
    text: string;
    display: boolean;
    delivery: "steer" | "followUp" | "quiet";
  }): Promise<void>;
  /**
   * Appends branch-scoped state without adding a context message; forks and reopens keep it.
   * The host stores the type as `<extension id>:<customType>`.
   */
  appendCustom(customType: string, data?: JsonValue): Promise<void>;
  /**
   * The latest `appendCustom` data of `customType` on the current root-to-leaf path, or `null`.
   * Reads this extension's own `<extension id>:<customType>` entries.
   */
  custom(customType: string): Promise<JsonValue | null>;
}

export interface ToolCall {
  id: string;
  name: string;
  arguments: Record<string, JsonValue>;
}

export interface ToolResult {
  /** Model-facing result. */
  content: UserBlock[];
  /** Host/log data; never reaches the model. */
  details?: JsonValue;
  isError?: boolean;
  /** Hint to stop after this batch; only honored when every result in the batch sets it. */
  terminate?: boolean;
}

export interface ToolRun {
  call: ToolCall;
  session: SessionHandle;
  /** Aborts when the host cancels the call. */
  signal: AbortSignal;
  /**
   * A partial result the host shows while the tool runs. The result is sent only after the host
   * has answered every update; one the host refuses is dropped.
   */
  update(update: { content: UserBlock[]; details?: JsonValue }): void;
}

export type ToolHandler = (run: ToolRun) => Promise<ToolResult>;

/** Where a tool call runs. */
export interface ToolScope {
  sessionId: string;
  depth: number;
  agent: string | null;
  mode: string | null;
  readOnlyBash: boolean;
}

/**
 * Sees every tool call of attached sessions. `before` blocks with an optional reason; `after`
 * overrides fields of the result. `null` allows the call or keeps the result.
 */
export interface ToolPolicy {
  before?(
    scope: ToolScope,
    call: ToolCall,
    signal: AbortSignal,
  ): Promise<null | { block: true; reason?: string; terminate?: boolean }>;
  after?(
    scope: ToolScope,
    call: ToolCall,
    result: { content: UserBlock[]; details?: JsonValue; isError: boolean },
    signal: AbortSignal,
  ): Promise<null | { content?: UserBlock[]; details?: JsonValue; isError?: boolean; terminate?: boolean }>;
}

/** A child agent about to be spawned. */
export interface SpawnRequest {
  /** The agent spec (the Rust `AgentSpec` serde shape). */
  spec: JsonValue;
  agent: string | null;
  mode: string | null;
  /** `provider/id`. */
  model: string;
  thinking: ThinkingLevel;
  depth: number;
  origin: "host" | "tool";
  team: { name: string | null; member: string } | null;
}

/** `null` allows the spawn; `block` refuses it with a reason; `replace` spawns another `AgentSpec`. */
export type SpawnHook = (
  spawn: SpawnRequest,
  session: SessionHandle,
  signal: AbortSignal,
) => Promise<null | { block: string } | { replace: JsonValue }>;

export interface SchemeContext {
  /** The scheme name, without `://`. */
  scheme: string;
  session: { id: string; dir: string; cwd: string };
}

/** Serves `<scheme>://<path>` URLs. */
export interface SchemeHandlers {
  /** `"virtual"` serves reads and writes here; `file` maps the URL to an absolute path. */
  resolve(path: string, cx: SchemeContext, signal: AbortSignal): Promise<"virtual" | { file: string; writable: boolean }>;
  read?(path: string, cx: SchemeContext, signal: AbortSignal): Promise<string>;
  /** Returns a short confirmation for the model. */
  write?(path: string, content: string, cx: SchemeContext, signal: AbortSignal): Promise<string>;
}

/** Runs `/name args`; `args` is the rest of the line, trimmed. */
export type CommandHandler = (args: string, session: SessionHandle, signal: AbortSignal) => Promise<void>;

// ---- The default export ----

export interface ExtensionContext {
  /** The extension ID from `extension.json`. */
  id: string;
  /** A private directory the host provides for this extension's own files. */
  dataDir: string;
  /** Session actions for an attached session by ID. */
  session(id: string): SessionHandle;
}

/** Handlers by kind, bound to the declarations in `extension.json`. */
export interface Handlers {
  providers?: Record<string, ProviderHandlers>;
  tools?: Record<string, ToolHandler>;
  schemes?: Record<string, SchemeHandlers>;
  commands?: Record<string, CommandHandler>;
  toolPolicy?: ToolPolicy;
  spawnHook?: SpawnHook;
  /**
   * New journal entries of attached sessions, in stored JSON shape and append order. Best effort
   * with no replay: entries are skipped when the stream lags, the runner is unavailable or this
   * handler throws.
   */
  onEntry?: (entry: JsonValue, session: SessionHandle) => void | Promise<void>;
}

/** The type of an extension's default export. */
export type Extension = (ctx: ExtensionContext) => Handlers | Promise<Handlers>;
