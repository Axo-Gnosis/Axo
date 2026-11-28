// The author contract for axo extensions. Types only: nothing here runs, and an extension
// never needs this package at runtime (`import type` / `satisfies` is erased).
// Host object methods are generated in bindings.ts and accept an optional trailing AbortSignal.
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
//   `rewrite`, `"transport": true` requires `send`, `"listModels": true` requires
//   `listModels`. A provider with none of these flags may omit its handler entry.
// - `tools`, `schemes` and `commands` are keyed by the declared names, one handler each.
// - `toolPolicy` is present exactly when the manifest's `"toolPolicy"` lists hooks, and defines
//   exactly the listed hooks (`"before"`, `"after"`). `spawnHook` and `onEntry` are present
//   exactly when the manifest sets `"spawnHook"` and `"events"`.
// Unknown keys, undeclared names and missing handlers fail activation.

import type {
  AfterToolCall, AssistantEvent, BeforeToolCall, CompletionDetails, CompletionRequest, CredentialInput, CredentialOutput,
  DeviceCodeInput, Host, HostErrorCode, HostLifecycle, LoginPromptInput, LoginSelectInput, Model, ModelAuth, Models,
  PolicyBlockInput, PolicyOverrideInput, SchemeResolutionInput, SeqEntry, Session, SessionEvent,
  SessionEventKind, SpawnContext, SpawnReplyInput, ToolCall, ToolResultInput, ToolScope, Tools,
  UserBlock, Value,
} from "./bindings.ts";
export type * from "./bindings.ts";

/** A rejected host call. Completion details are present only when the stream reported a terminal error. */
export type HostCallError = Error & (
  | { readonly code: "completion"; readonly details?: CompletionDetails }
  | { readonly code: Exclude<HostErrorCode, "completion">; readonly details?: never }
);

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

/**
 * What a login asks of the host UI. Display methods resolve once the host has shown the
 * request. `prompt` and `select` resolve with the answer (`select` with the chosen option
 * `id`); their optional signal withdraws that one question, e.g. when a browser callback
 * wins. The login's own signal withdraws every outstanding question.
 */
export interface LoginInteraction {
  openUrl(url: string, instructions?: string): Promise<void>;
  deviceCode(code: DeviceCodeInput): Promise<void>;
  prompt(prompt: LoginPromptInput, signal?: AbortSignal): Promise<string>;
  select(select: LoginSelectInput, signal?: AbortSignal): Promise<string>;
  /** A non-secret status line for the login in progress. */
  progress(message: string): Promise<void>;
}

// ---- Provider handlers and the default export ----

type LoginHandlers =
  | {
      /** Runs the login; axo stores the returned credential. */
      login(ui: LoginInteraction, signal: AbortSignal): Promise<CredentialInput>;
      /** Exchanges an expiring OAuth credential for a fresh one, under axo's credential lock. */
      refresh(credential: CredentialOutput, signal: AbortSignal): Promise<CredentialInput>;
    }
  | { login?: never; refresh?: never };

interface RewriteHandler {
  /** Returns the complete replacement request; axo still sends it and parses the response. */
  rewrite?(model: Readonly<Model>, request: ProviderRequest, signal: AbortSignal): Promise<ProviderRequest>;
}

interface TransportHandler {
  /**
   * Sends one attempt of the request axo built (after `rewrite`) and returns the HTTP
   * response; axo retries retryable statuses as new attempts and decodes the body as the
   * model's wire. The body must be text (SSE or a JSON error); binary bodies are not
   * supported. `signal` aborts when axo drops the response.
   */
  send?(model: Readonly<Model>, request: ProviderRequest, context: RequestContext, signal: AbortSignal): Promise<Response>;
}

interface ListModelsHandler {
  /** The provider's current models, each naming this provider; a rejection keeps the list axo has. */
  listModels?(request: ModelsRequest, signal: AbortSignal): Promise<Model[]>;
}

/** What axo knows about a request beyond the request itself. */
export interface RequestContext {
  sessionId?: string;
  /** 0 for the first attempt; axo's retry increments it. */
  attempt: number;
}

/** What a model listing receives. */
export interface ModelsRequest {
  /** The provider's resolved credential; absent without one. */
  auth?: ModelAuth;
  /** The models.dev catalog rows axo maps for `provider`, before `models.jsonc`. */
  catalog(provider: string): Promise<Model[]>;
}

export type ProviderHandlers = LoginHandlers & RewriteHandler & TransportHandler & ListModelsHandler;

// ---- Sessions ----

export type SessionStreamEvent =
  | SessionEvent
  | { type: "lagged"; value: { skipped: number } }
  | { type: "closed"; value: null };

/**
 * Actions address an attached session or a known child, reviving a parked child if needed.
 * Subscriptions never revive a child: they observe attached sessions or currently resident
 * children, end on detach or loss of residency respectively, and reject with `no_session`
 * for an unattached nonresident or unknown ID.
 */
export type SessionHandle = Session & {
  readonly id: string;
  subscribe(
    listener: (event: SessionStreamEvent) => void | Promise<void>,
    kinds?: SessionEventKind[],
  ): Promise<{ close(): Promise<void> }>;
};

export interface ToolRun {
  call: ToolCall;
  sessionId: string;
  invocation: number;
  /** The model-issued call id for a nested call; null for a direct call. */
  parent: string | null;
  /** Guarded file operations, supervised commands, and helpers for this call. */
  tools: Tools;
  /** Aborts when the host cancels the call. */
  signal: AbortSignal;
  /**
   * Shows a partial result. Await updates for sustained output: a full host-call queue drops
   * the update. Host refusals are nonfatal; the tool result waits for all admitted updates.
   */
  update(update: { content: UserBlock[]; details?: Value }): Promise<void>;
}

export type ToolHandler = (run: ToolRun) => Promise<ToolResultInput>;

/**
 * Sees the assistant message and tool call. `before` blocks with an optional reason; `after`
 * also sees the full result and may override its fields. `null` keeps the default behavior.
 */
export interface ToolPolicy {
  before?(
    scope: ToolScope,
    cx: BeforeToolCall,
    signal: AbortSignal,
  ): Promise<PolicyBlockInput | null>;
  after?(
    scope: ToolScope,
    cx: AfterToolCall,
    signal: AbortSignal,
  ): Promise<PolicyOverrideInput | null>;
}

/** `null` allows the spawn; `block` refuses it with a reason; `replace` spawns another `AgentSpec`. */
export type SpawnHook = (
  spawn: SpawnContext,
  signal: AbortSignal,
) => Promise<SpawnReplyInput | null>;

export interface SchemeHandlerContext {
  /** The scheme name, without `://`. */
  scheme: string;
  session: { id: string; dir: string; cwd: string };
}

/** Serves `<scheme>://<path>` URLs. */
export interface SchemeHandlers {
  /** `"virtual"` serves reads and writes here; `file` maps the URL to an absolute path. */
  resolve(path: string, cx: SchemeHandlerContext, signal: AbortSignal): Promise<SchemeResolutionInput>;
  read?(path: string, cx: SchemeHandlerContext, signal: AbortSignal): Promise<string>;
  /** Returns a short confirmation for the model. */
  write?(path: string, content: string, cx: SchemeHandlerContext, signal: AbortSignal): Promise<string>;
}

/** Runs `/name args`; `args` is the rest of the line, trimmed. */
export type CommandHandler = (args: string, session: SessionHandle, signal: AbortSignal) => Promise<void>;

// ---- The default export ----

export type HostHandle = Host & HostLifecycle;

export type CompletionStreamEvent = AssistantEvent | { type: "closed"; value: null };

export type ModelsHandle = Models & {
  stream(
    request: CompletionRequest,
    listener: (event: CompletionStreamEvent) => void | Promise<void>,
  ): Promise<{ close(): Promise<void> }>;
};

export interface ExtensionContext {
  /** The extension ID from `extension.json`. */
  id: string;
  /** A private directory the host provides for this extension's own files. */
  dataDir: string;
  host: HostHandle;
  models: ModelsHandle;
  /** Address an attached session or known child. Actions can revive a child; subscriptions cannot. */
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
