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

import type { AgentSpec, ErrorCode, Model, SeqEntry, Session, SessionEvent, SessionEventKind, ThinkingLevel, ToolCall, UserBlock, Value } from "./bindings.ts";
export type * from "./bindings.ts";

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
  body: Value;
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

export type SessionStreamEvent =
  | SessionEvent
  | { type: "lagged"; value: { skipped: number } }
  | { type: "closed"; value: null };

/**
 * Acts on one session while the host has it attached; calls on a session the host no longer
 * tracks reject.
 */
export type SessionHandle = Session & {
  readonly id: string;
  subscribe(
    listener: (event: SessionStreamEvent) => void | Promise<void>,
    kinds?: SessionEventKind[],
  ): Promise<{ close(): Promise<void> }>;
};

export type HostErrorCode =
  | ErrorCode
  | "unknown_method"
  | "invalid_arguments"
  | "encode"
  | "no_session"
  | "no_subscription"
  | "unsupported"
  | "stalled"
  /** The bridge could not decode the action's arguments. */
  | "invalid"
  /** The host's reply would exceed the 64 MiB frame bound. */
  | "too_large";

export interface ToolResult {
  /** Model-facing result. */
  content: UserBlock[];
  /** Host/log data; never reaches the model. */
  details?: Value;
  isError?: boolean;
  /** Hint to stop after this batch; only honored when every result in the batch sets it. */
  terminate?: boolean;
}

export interface ToolRun {
  call: ToolCall;
  sessionId: string;
  /** Aborts when the host cancels the call. */
  signal: AbortSignal;
  /**
   * A partial result the host shows while the tool runs. The result is sent only after the host
   * has answered every update; one the host refuses is dropped.
   */
  update(update: { content: UserBlock[]; details?: Value }): void;
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
    result: { content: UserBlock[]; details?: Value; isError: boolean },
    signal: AbortSignal,
  ): Promise<null | { content?: UserBlock[]; details?: Value; isError?: boolean; terminate?: boolean }>;
}

/** A child agent about to be spawned: the bridge's summary of the Rust `SpawnContext` (see `RemoteSpawnHook`), not its full shape. */
export interface SpawnContext {
  spec: AgentSpec;
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
  spawn: SpawnContext,
  sessionId: string,
  signal: AbortSignal,
) => Promise<null | { block: string } | { replace: AgentSpec }>;

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
   * New journal entries of attached sessions with their sequence numbers, in stored JSON shape
   * and append order. Best effort with no replay: entries are skipped when the stream lags, the
   * runner is unavailable or this handler throws. An entry these bindings do not know arrives
   * with a `RawEntry` payload.
   */
  onEntry?: (entry: SeqEntry, session: SessionHandle) => void | Promise<void>;
}

/** The type of an extension's default export. */
export type Extension = (ctx: ExtensionContext) => Handlers | Promise<Handlers>;
