// The author contract for axo provider extensions. Types only: nothing here runs, and an
// extension never needs this package at runtime (`import type` / `satisfies` is erased).
//
//   import type { Extension } from "@axo/extension-sdk";
//   export default (async ({ id, dataDir }) => ({
//     lab: { login, refresh, rewrite },
//   })) satisfies Extension;
//
// The returned record is keyed by the provider IDs declared in `extension.json`. A provider
// declared with `"login": true` must supply both `login` and `refresh`; `"rewrite": true`
// requires `rewrite`. Undeclared providers or methods fail activation.

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

export interface ExtensionContext {
  /** The extension ID from `extension.json`. */
  id: string;
  /** A private directory the host provides for this extension's own files. */
  dataDir: string;
}

/** The type of an extension's default export. */
export type Extension = (
  ctx: ExtensionContext,
) => Record<string, ProviderHandlers> | Promise<Record<string, ProviderHandlers>>;
