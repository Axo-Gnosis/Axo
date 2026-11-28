// axo extension runner: the Bun-side peer of the bridge in this crate. The bridge embeds this
// file, writes it into a private per-generation directory and starts `bun run <file>`.
//
// Environment: AXO_EXTENSION_SOCKET (Unix socket path) and AXO_EXTENSION_NONCE.
// They are removed from `process.env` before any extension code loads.
//
// Wire: each frame is a 4-byte unsigned big-endian length followed by that many bytes of
// UTF-8 JSON, at most 64 MiB in either direction. The runner connects and sends
//   {type:"hello",nonce}
// The private socket and nonce bind both peers to this runner instance.
//   {type:"call",id,ext,method,args,parent?}  id "h:<n>" from Rust, "r:<n>" from the runner
//   {type:"ok",id,value} | {type:"error",id,code}  runner terminal reply
//   Rust error replies also carry message and optional details.
//   {type:"cancel",id}                         withdraw a call the sender made
//   {type:"ping",id} -> {type:"pong",id}       Rust pings; the runner answers from its event loop.
//                                              While the socket is blocked an unsent pong is
//                                              replaced by the newest one; any pong proves liveness.
// Rust -> runner methods (arguments supplied by the same-build parent):
//   activate {entry,dataDir,providers:[{id,login,rewrite,transport,listModels}],
//             session:{tools:[{name,..}],schemes:[{name,..}],commands:[{name,..}],
//                      toolPolicy:["before"|"after"],spawnHook,events},
//             methods:{<object>:[<invokable method>]}} -> null
//   login {provider} -> Credential; refresh {provider,credential} -> Credential;
//   rewrite {provider,model,request} -> ProviderRequest
//   send {provider,model,request,context:{sessionId,attempt}} -> null once the body is sent
//   listModels {provider,auth} -> Model[]
//   tool {tool,session,invocation,call:{id,name,arguments}} -> {content,details?,isError?,terminate?}
//   policyBefore {scope,cx:{assistantMessage,toolCall}} -> null | {block:true,reason?,terminate?}
//   policyAfter {scope,cx:{assistantMessage,toolCall,result}}
//     -> null | {content?,details?,isError?,terminate?}
//   spawn {spawn} -> null | {block} | {replace}
//   schemeResolve {scheme,path,session:{id,dir,cwd}} -> "virtual" | {file,writable}
//   schemeRead {scheme,path,session} -> string; schemeWrite {scheme,path,session,content} -> string
//   command {command,session,args} -> null; entry {session,entry} -> null
//   stream {sub,event} | {sub,lagged} | {sub,closed:true} -> null
// Runner -> Rust methods with `parent` = a live call of the same `ext`:
//   under login: openUrl {url,instructions?} -> null; deviceCode {userCode,verificationUrl,
//   expiresInSeconds?} -> null; prompt {message,placeholder?,secret} -> string;
//   select {message,options:[{id,label}]} -> string; progress {message} -> null.
//   under tool: update {content,details?} -> null; invoke {object:"tools",method,args} -> method result.
//   under send: head {status,headers:[[name,value]]} -> null (once, first); body {text} -> null.
//   under listModels: catalog {provider} -> Model[].
// A login replies only after every display/progress callback it made was acknowledged; an
// error reply to one fails the login. A tool replies only after every update it sent was
// answered; an error reply to an update is ignored.
// Runner -> Rust actions without `parent`:
//   invoke {object:"host"|"models",method,args:[...]} -> method result.
//   invoke {object:"session",target:<session id>,method,args:[...]} -> method result.
//   subscribe {session,kinds} | {completion} -> {sub}; unsubscribe {sub} -> null.
// Error codes the runner sends: "activation" (import/activation of that member failed; the
// bridge disables only that member), "failed" (the author's method threw), "invalid" (bad
// arguments, an invalid author return value, or a reply that is not JSON-serializable),
// "unsupported" (no such handler or method), "too_large" (the reply would exceed the frame
// bound). Rust owns the message and names the owner from the call. A canceled call gets no reply.
// Protocol violations (bad length or JSON, unknown frame type,
// duplicate live ID, wrong-direction method) end the process with status 1; Rust sees EOF.
// The runner exits with status 0 when Rust closes the socket.

import { connect } from "node:net";
import { pathToFileURL } from "node:url";
import type {
  AfterToolCall,
  BeforeToolCall,
  CommandHandler,
  CompletionStreamEvent,
  CredentialOutput,
  Extension,
  Handlers,
  HostHandle,
  LoginInteraction,
  Model,
  ModelAuth,
  ModelsHandle,
  ProviderHandlers,
  ProviderRequest,
  RequestContext,
  SchemeHandlerContext,
  SchemeHandlers,
  SeqEntry,
  SessionHandle,
  SessionStreamEvent,
  SpawnContext,
  SpawnHook,
  ToolHandler,
  ToolPolicy,
  ToolResultInput,
  ToolRun,
  ToolRunArgs,
  ToolScope,
} from "../../../packages/axo-extension-sdk/index.ts";

const MAX_FRAME = 64 * 1024 * 1024;
// Bytes held ahead of the socket before replies and callbacks wait for their turn.
const QUEUE_LIMIT = MAX_FRAME;

const socketPath = process.env.AXO_EXTENSION_SOCKET;
const nonce = process.env.AXO_EXTENSION_NONCE;
delete process.env.AXO_EXTENSION_SOCKET;
delete process.env.AXO_EXTENSION_NONCE;
if (!socketPath || !nonce) {
  console.error("axo runner: missing or invalid AXO_EXTENSION_* environment");
  process.exit(2);
}

type Frame = { [key: string]: unknown };

/** An error whose code goes to Rust as-is. */
class Fail extends Error {
  constructor(readonly code: string) { super(); }
}

/** A Rust error reply to a runner callback. */
class HostError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly details?: unknown,
  ) {
    super(message);
    this.name = "HostError";
  }
}

const socket = connect(socketPath);

function violation(reason: string): never {
  console.error(`axo runner: protocol violation: ${reason}`);
  socket.destroy();
  process.exit(1);
}

// ---- Writer: one ordered queue honoring socket backpressure; a pong jumps ahead ----
//
// Bytes held here stay under QUEUE_LIMIT + one frame: senders queue FIFO for a turn and
// encode only once granted, and the socket is written only while it is not blocked.
// Control frames bypass the turn: at most one pong (a newer ping replaces an unsent pong)
// and one cancel per live callback.

let pong: Buffer | undefined;
const frames: Buffer[] = [];
const turns: Array<() => void> = [];
let granted = false;
let queued = 0;
let blocked = false;

/** Encodes a frame, or names why it cannot be sent. */
function encode(frame: Frame): Buffer | "too_large" | "invalid" {
  let json: string;
  try {
    json = JSON.stringify(frame);
  } catch {
    return "invalid";
  }
  const length = Buffer.byteLength(json);
  if (length > MAX_FRAME) return "too_large";
  const out = Buffer.allocUnsafe(4 + length);
  out.writeUInt32BE(length, 0);
  out.write(json, 4);
  return out;
}

function pump(): void {
  while (!blocked) {
    const buf = pong ?? frames.shift();
    if (!buf) break;
    if (buf === pong) pong = undefined;
    queued -= buf.length;
    blocked = !socket.write(buf);
  }
  grant();
}

function grant(): void {
  if (granted || queued >= QUEUE_LIMIT) return;
  const next = turns.shift();
  if (!next) return;
  granted = true;
  next();
}

socket.on("drain", () => {
  blocked = false;
  pump();
});

function enqueue(buf: Buffer): void {
  frames.push(buf);
  queued += buf.length;
  pump();
}

/** Small control frames that do not wait for a turn (hello, cancel). */
function sendControl(frame: Frame): void {
  enqueue(encode(frame) as Buffer);
}

function sendPong(id: string): void {
  if (pong) queued -= pong.length;
  pong = encode({ type: "pong", id }) as Buffer;
  queued += pong.length;
  pump();
}

/**
 * Queues a frame on its turn. A withdrawn waiter releases the payload without
 * waiting for socket progress.
 */
async function send(frame: Frame, withdrawal?: AbortSignal, enqueued?: () => void): Promise<"ok" | "too_large" | "invalid"> {
  const { promise, resolve } = Promise.withResolvers<boolean>();
  const turn = () => resolve(true);
  const withdraw = () => {
    const index = turns.indexOf(turn);
    if (index !== -1) {
      turns.splice(index, 1);
      resolve(false);
    }
  };
  turns.push(turn);
  withdrawal?.addEventListener("abort", withdraw, { once: true });
  grant();
  const acquired = await promise;
  withdrawal?.removeEventListener("abort", withdraw);
  if (!acquired) return "ok";
  try {
    if (withdrawal?.aborted) return "ok";
    const buf = encode(frame);
    if (withdrawal?.aborted) return "ok"; // JSON serialization can invoke author code.
    if (typeof buf === "string") return buf;
    frames.push(buf);
    queued += buf.length;
    enqueued?.();
    return "ok";
  } finally {
    granted = false;
    pump();
  }
}

// ---- Reader: length-prefixed frames, bounded before allocation ----

const utf8 = new TextDecoder("utf-8", { fatal: true });
const chunks: Buffer[] = [];
let have = 0;
let need = -1;

function take(n: number): Buffer {
  have -= n;
  if (chunks[0]!.length === n) return chunks.shift()!;
  if (chunks[0]!.length > n) {
    const out = chunks[0]!.subarray(0, n);
    chunks[0] = chunks[0]!.subarray(n);
    return out;
  }
  const out = Buffer.allocUnsafe(n);
  for (let off = 0; off < n; ) {
    const chunk = chunks[0]!;
    const k = Math.min(chunk.length, n - off);
    chunk.copy(out, off, 0, k);
    off += k;
    if (k === chunk.length) chunks.shift();
    else chunks[0] = chunk.subarray(k);
  }
  return out;
}

socket.on("data", (chunk: Buffer) => {
  chunks.push(chunk);
  have += chunk.length;
  for (;;) {
    if (need < 0) {
      if (have < 4) return;
      need = take(4).readUInt32BE(0);
      if (need > MAX_FRAME) violation("frame exceeds 64 MiB");
    }
    if (have < need) return;
    const body = take(need);
    need = -1;
    let frame: unknown;
    try {
      frame = JSON.parse(utf8.decode(body));
    } catch {
      violation("frame is not UTF-8 JSON");
    }
    if (typeof frame !== "object" || frame === null || Array.isArray(frame)) {
      violation("frame is not an object");
    }
    onFrame(frame as Frame);
  }
});

socket.on("connect", () => sendControl({ type: "hello", nonce }));
socket.on("close", () => process.exit(0));
socket.on("error", () => {
  console.error("axo runner: socket error");
  process.exit(1);
});

// ---- Dispatch ----

const RUNNER_METHODS: Record<string, true> = {
  openUrl: true,
  deviceCode: true,
  prompt: true,
  select: true,
  progress: true,
  update: true,
  invoke: true,
  subscribe: true,
  unsubscribe: true,
};

interface Call {
  ext: string;
  method: string;
  controller: AbortController;
}

/** Live Rust calls by ID; a canceled or finished call is absent. */
const calls = new Map<string, Call>();
/** Live runner callbacks by ID. */
const pending = new Map<string, { resolve(value: unknown): void; reject(error: Error): void }>();
let nextId = 0n;
/** Live runner calls Rust has not answered: half of peer.rs `MAX_CALLBACKS`, whose overflow ends the generation. */
const MAX_HOST_CALLS = 128;
let activeHostCalls = 0; // excludes the reserved subscription rollback slot
const acknowledged = Promise.resolve();
/** Calls waiting for a slot under `MAX_HOST_CALLS`, started in order. */
const queuedHostCalls: { start(): void; reject(error: Error): void }[] = [];
// ponytail: a stall is detected, not prevented; exempt re-entrant calls from the slot cap if real workloads hit it
const STALL_MS = 30_000;
let stallTimer: NodeJS.Timeout | undefined;

function resetStallTimer(): void {
  clearTimeout(stallTimer);
  stallTimer = queuedHostCalls.length ? setTimeout(() => {
    stallTimer = undefined;
    for (const call of queuedHostCalls.splice(0)) call.reject(new HostError("stalled", "too many nested host calls"));
  }, STALL_MS) : undefined;
}

function onFrame(frame: Frame): void {
  const id = frame.id;
  if (typeof id !== "string") violation("frame without string id");
  switch (frame.type) {
    case "ping":
      return sendPong(id);
    case "call":
      return onCall(id, frame);
    case "cancel": {
      if (!id.startsWith("h:")) violation("cancel of a runner call");
      const call = calls.get(id);
      calls.delete(id);
      call?.controller.abort(new Error("canceled by host"));
      return;
    }
    case "ok":
    case "error": {
      if (!id.startsWith("r:")) violation("reply to a host call");
      const waiter = pending.get(id);
      if (!waiter) return; // late reply to a withdrawn callback
      if (frame.type === "ok") return waiter.resolve(frame.value);
      const code = typeof frame.code === "string" ? frame.code : "error";
      const message = typeof frame.message === "string" ? frame.message : "host error";
      return waiter.reject(new HostError(code, message, frame.details));
    }
    default:
      violation("unknown frame type");
  }
}

function onCall(id: string, frame: Frame): void {
  if (!/^h:\d+$/.test(id)) violation("call id is not a host id");
  if (calls.has(id)) violation("duplicate live call id");
  const { method, ext } = frame;
  if (typeof method !== "string" || typeof ext !== "string") violation("malformed call");
  if (Object.hasOwn(RUNNER_METHODS, method)) violation("host called a runner method");
  if (frame.parent !== undefined) violation("host call with a parent");
  const handler = Object.hasOwn(handlers, method) ? handlers[method] : undefined;
  if (!handler) {
    void send({ type: "error", id, code: "unsupported" });
    return;
  }
  const controller = new AbortController();
  const call: Call = { ext, method, controller };
  calls.set(id, call);
  const finish = async (reply: Frame) => {
    if (calls.get(id) !== call) return; // canceled
    calls.delete(id);
    controller.abort(new Error("call finished"));
    const status = await send(reply);
    if (status !== "ok") {
      await send({ type: "error", id, code: status });
    }
  };
  handler(id, call, frame.args).then(
    (value) => finish({ type: "ok", id, value: value ?? null }),
    (error) => finish(errorReply(id, error)),
  );
}

/** Author exceptions are never logged or forwarded. */
function errorReply(id: string, error: unknown): Frame {
  return { type: "error", id, code: error instanceof Fail ? error.code : "failed" };
}

// ---- Rust -> runner methods ----

/** A tool policy hook the manifest declares. */
type PolicyHook = "before" | "after";

/** One activated extension's bound handlers, exactly as its activate call declared them. */
interface Active {
  providers: Map<string, ProviderHandlers>;
  tools: Map<string, ToolHandler>;
  schemes: Map<string, SchemeHandlers>;
  commands: Map<string, CommandHandler>;
  toolPolicy?: ToolPolicy;
  spawnHook?: SpawnHook;
  onEntry?: NonNullable<Handlers["onEntry"]>;
}

/** Activated extensions by ID. */
const extensions = new Map<string, Active>();
const activating = new Set<string>();

// The authenticated same-build parent supplies each handler's argument shape.
type Handler = (id: string, call: Call, args: unknown) => Promise<unknown>;

const handlers: Record<string, Handler> = {
  activate: async (_id, call, args) => activate(call.ext, args as ActivateArgs, call.controller.signal),
  login: providerCall,
  refresh: providerCall,
  rewrite: providerCall,
  send: providerCall,
  listModels: providerCall,
  async tool(id, call, raw) {
    const args = raw as ToolRunArgs;
    const handler = extensions.get(call.ext)?.tools.get(args.tool);
    if (!handler) throw new Fail("unsupported");
    return tool(id, call, handler, args);
  },
  async policyBefore(_id, call, args) {
    const before = extensions.get(call.ext)?.toolPolicy?.before;
    if (!before) throw new Fail("unsupported");
    const { scope, cx } = args as { scope: ToolScope; cx: BeforeToolCall };
    const value = await before(freeze(scope), cx, call.controller.signal);
    return decision(value, { block: "true", reason: "string?", terminate: "boolean?" });
  },
  async policyAfter(_id, call, args) {
    const after = extensions.get(call.ext)?.toolPolicy?.after;
    if (!after) throw new Fail("unsupported");
    const { scope, cx } = args as { scope: ToolScope; cx: AfterToolCall };
    const value = await after(freeze(scope), cx, call.controller.signal);
    return decision(value, {
      content: "array?",
      details: "any?",
      isError: "boolean?",
      terminate: "boolean?",
    });
  },
  async spawn(_id, call, args) {
    const spawn = extensions.get(call.ext)?.spawnHook;
    if (!spawn) throw new Fail("unsupported");
    const { spawn: context } = args as { spawn: SpawnContext };
    const value = await spawn(context, call.controller.signal);
    if (isRecord(value) && Object.hasOwn(value, "replace")) return decision(value, { replace: "object" });
    return decision(value, { block: "string" });
  },
  schemeResolve: schemeCall,
  schemeRead: schemeCall,
  schemeWrite: schemeCall,
  async command(_id, call, raw) {
    const args = raw as { command: string; session: string; args: string };
    const handler = extensions.get(call.ext)?.commands.get(args.command);
    if (!handler) throw new Fail("unsupported");
    await handler(args.args, handle(call.ext, args.session), call.controller.signal);
    return null;
  },
  async entry(_id, call, args) {
    const onEntry = extensions.get(call.ext)?.onEntry;
    if (!onEntry) throw new Fail("unsupported");
    const { session, entry } = args as { session: string; entry: SeqEntry };
    await onEntry(freeze(entry), handle(call.ext, session));
    return null;
  },
  async stream(_id, { ext }, raw) {
    const args = raw as { sub: string; event?: StreamEvent; lagged?: number; closed?: true };
    const { sub } = args;
    // A stream can emit before Rust's subscribe reply reaches the socket.
    while (!subscriptions.has(sub)) {
      const replies = [];
      for (const [reply, owner] of registering) if (owner === ext) replies.push(reply);
      if (!replies.length) break;
      await Promise.race(replies).catch(() => {});
    }
    const subscription = subscriptions.get(sub);
    if (subscription?.ext !== ext) throw new Fail("unsupported");
    let event: StreamEvent;
    if (args.closed === true) {
      subscription.remove();
      event = { type: "closed", value: null };
    } else if (args.lagged !== undefined) {
      event = { type: "lagged", value: { skipped: args.lagged } };
    } else {
      event = args.event!;
    }
    const { listener } = subscription;
    try {
      await listener(event);
    } catch {
      // One listener error must not end the subscription.
    }
    return null;
  },
};

function isRecord(value: unknown): value is Frame {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function record(value: unknown): Frame {
  if (!isRecord(value)) throw new Fail("invalid");
  return value;
}

function text(value: unknown): string {
  if (typeof value !== "string") throw new Fail("invalid");
  return value;
}

async function schemeCall(_id: string, { ext, method, controller }: Call, raw: unknown): Promise<unknown> {
  const signal = controller.signal;
  const args = raw as { scheme: string; path: string; session: SchemeHandlerContext["session"]; content: string };
  const scheme = extensions.get(ext)?.schemes.get(args.scheme);
  if (!scheme) throw new Fail("unsupported");
  const cx: SchemeHandlerContext = freeze({ scheme: args.scheme, session: args.session });
  const { path } = args;
  if (method === "schemeResolve") {
    const target = await scheme.resolve(path, cx, signal);
    if (target === "virtual") return target;
    return decision(target, { file: "string", writable: "boolean" }) ?? invalidReply();
  }
  if (method === "schemeRead") {
    if (!scheme.read) throw new Fail("unsupported");
    return text(await scheme.read(path, cx, signal));
  }
  if (!scheme.write) throw new Fail("unsupported");
  return text(await scheme.write(path, args.content, cx, signal));
}

async function providerCall(id: string, call: Call, raw: unknown): Promise<unknown> {
  const { ext, method, controller } = call;
  const signal = controller.signal;
  const args = raw as {
    provider: string;
    credential: CredentialOutput;
    model: Model;
    request: ProviderRequest;
    context: RequestContext;
    auth: ModelAuth | null;
  };
  const handlers = extensions.get(ext)?.providers.get(args.provider);
  switch (method) {
    case "login": {
      if (!handlers?.login) throw new Fail("unsupported");
      const notices = { pending: new Set<Promise<void>>(), failed: false };
      const value = await handlers.login(interaction(ext, { id, signal }, notices), signal);
      while (notices.pending.size) await Promise.allSettled(notices.pending);
      if (notices.failed) throw new Fail("failed");
      return credential(value);
    }
    case "refresh": {
      if (!handlers?.refresh) throw new Fail("unsupported");
      return credential(await handlers.refresh(args.credential, signal));
    }
    case "rewrite": {
      if (!handlers?.rewrite) throw new Fail("unsupported");
      return request(await handlers.rewrite(freeze(args.model), args.request, signal));
    }
    case "listModels": {
      if (!handlers?.listModels) throw new Fail("unsupported");
      const parent: Parent = { id, signal };
      const models = await handlers.listModels(
        {
          ...(args.auth ? { auth: freeze(args.auth) } : {}),
          catalog: async (provider: string) => (await callHost(ext, "catalog", { provider }, parent)) as Model[],
        },
        signal,
      );
      if (!Array.isArray(models)) invalidReply();
      return models;
    }
    default: {
      if (!handlers?.send) throw new Fail("unsupported");
      const response = await handlers.send(freeze(args.model), args.request, freeze(args.context), signal);
      if (!(response instanceof Response)) invalidReply();
      return respond(ext, { id, signal }, response);
    }
  }
}

/** Sends a transport response as parented `head` and `body` calls, each awaited (backpressure). */
async function respond(ext: string, parent: Parent, response: Response): Promise<null> {
  const headers: [string, string][] = [...response.headers];
  await callHost(ext, "head", { status: response.status, headers }, parent);
  if (!response.body) return null;
  const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
  const abort = () => void reader.cancel(parent.signal.reason).catch(() => {});
  parent.signal.addEventListener("abort", abort, { once: true });
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return null;
      if (value) await callHost(ext, "body", { text: value }, parent);
    }
  } finally {
    parent.signal.removeEventListener("abort", abort);
  }
}

async function tool(id: string, call: Call, handler: ToolHandler, args: ToolRunArgs): Promise<ToolResultInput> {
  const { ext, controller } = call;
  const signal = controller.signal;
  const parent: Parent = { id, signal };
  // Every update is answered before the result goes out, so none trails it; a refused
  // update is the host's business, not the tool's failure.
  const updates = new Set<Promise<unknown>>();
  const run: ToolRun = {
    call: args.call,
    sessionId: args.session,
    invocation: args.invocation,
    parent: args.parent ?? null,
    tools: object(ext, "tools", {}, undefined, parent) as ToolRun["tools"],
    signal,
    update(update) {
      const u = local(isRecord(update), "update", update);
      const content = local(Array.isArray(u.content), "update content", u.content);
      if (signal.aborted || queuedHostCalls.length >= MAX_HOST_CALLS) return acknowledged;
      const sent = callHost(ext, "update", { content, details: u.details }, parent).then(done, done);
      function done() { updates.delete(sent); }
      updates.add(sent);
      return sent;
    },
  };
  let value: ToolResultInput;
  try {
    value = await handler(run);
  } catch (error) {
    // An unhandled host tool refusal is a model-facing tool error, not an author exception.
    if (!(error instanceof HostError && error.code === "tool")) throw error;
    value = { content: [{ type: "text", text: error.message }], isError: true };
  } finally {
    while (updates.size) await Promise.allSettled(updates);
  }
  const result = decision(value, {
    content: "array",
    details: "any?",
    isError: "boolean?",
    terminate: "boolean?",
  });
  return (result ?? invalidReply()) as unknown as ToolResultInput;
}

type Check = "true" | "string" | "boolean" | "array" | "object" | "any";

function invalidReply(): never {
  throw new Fail("invalid");
}

/**
 * An author's reply: `null` or `undefined` as `null`, otherwise a record with only the
 * `shape` keys (`?` marks optional); anything else fails with an invalid-result error.
 */
function decision(value: unknown, shape: Record<string, Check | `${Check}?`>): Frame | null {
  if (value === null || value === undefined) return null;
  if (!isRecord(value) || Object.keys(value).some((key) => !Object.hasOwn(shape, key))) return invalidReply();
  const out: Frame = {};
  for (const [key, spec] of Object.entries(shape)) {
    const v = value[key];
    const optional = spec.endsWith("?");
    const check = (optional ? spec.slice(0, -1) : spec) as Check;
    const ok =
      v === undefined || (v === null && optional)
        ? optional
        : check === "any" ||
          (check === "true" && v === true) ||
          (check === "array" && Array.isArray(v)) ||
          (check === "object" && isRecord(v)) ||
          ((check === "string" || check === "boolean") && typeof v === check);
    if (!ok) invalidReply();
    if (v !== undefined) out[key] = v;
  }
  return out;
}

/** Author errors and keys never reach diagnostics. */
function fail(): never {
  throw new Fail("activation");
}

const HANDLER_KINDS = ["providers", "tools", "schemes", "commands", "toolPolicy", "spawnHook", "onEntry"];

type ActivateArgs = {
  entry: string;
  dataDir: string;
  providers: DeclaredProvider[];
  methods: Record<string, string[]>;
  session: {
    tools: { name: string }[];
    schemes: { name: string }[];
    commands: { name: string }[];
    toolPolicy: PolicyHook[];
    spawnHook: boolean;
    events: boolean;
  };
};

async function activate(ext: string, args: ActivateArgs, signal: AbortSignal): Promise<null> {
  const { entry, dataDir, session: decl } = args;
  methods.clear();
  for (const [name, list] of Object.entries(args.methods)) methods.set(name, new Set(list));
  if (extensions.has(ext) || activating.has(ext)) throw new Fail("invalid");
  activating.add(ext);
  const lifetime = new AbortController();
  const cancel = () => {
    activating.delete(ext);
    lifetime.abort(signal.reason);
  };
  signal.addEventListener("abort", cancel, { once: true });
  let committed = false;
  try {
    let module: { default?: unknown };
    try {
      // The entry is a runtime-selected, host-admitted extension file.
      module = await import(pathToFileURL(entry).href);
    } catch {
      return fail();
    }
    signal.throwIfAborted();
    if (typeof module.default !== "function") return fail();
    let result: unknown;
    try {
      result = await (module.default as Extension)({
        id: ext,
        dataDir,
        host: object(ext, "host", {}) as HostHandle,
        models: object(ext, "models", {
          stream: (request, listener) => openSubscription(ext, { completion: request }, listener, lifetime.signal),
        } satisfies Pick<ModelsHandle, "stream">) as ModelsHandle,
        session: (id) => handle(ext, local(typeof id === "string", "session id", id), lifetime.signal),
      });
    } catch {
      return fail();
    }
    const handlers: Frame = isRecord(result) ? result : fail();
    for (const key of Object.keys(handlers)) {
      if (!HANDLER_KINDS.includes(key)) fail();
    }
    const active: Active = {
      providers: providers(handlers.providers, args.providers),
      tools: named<ToolHandler>(handlers.tools, decl.tools.map(({ name }) => name), method),
      schemes: named(handlers.schemes, decl.schemes.map(({ name }) => name), schemeHandlers),
      commands: named<CommandHandler>(handlers.commands, decl.commands.map(({ name }) => name), method),
      toolPolicy: single(handlers.toolPolicy, decl.toolPolicy.length > 0, (h): ToolPolicy | undefined => {
        if (!isRecord(h)) return undefined;
        if (Object.keys(h).some((key) => key !== "before" && key !== "after")) fail();
        const bind = <T>(hook: PolicyHook): T | undefined => {
          if (!decl.toolPolicy.includes(hook)) {
            return h[hook] === undefined ? undefined : fail();
          }
          return method<T>(h[hook], h) ?? fail();
        };
        return { before: bind<ToolPolicy["before"]>("before"), after: bind<ToolPolicy["after"]>("after") };
      }),
      spawnHook: single(handlers.spawnHook, decl.spawnHook, (h) => method<SpawnHook>(h, handlers)),
      onEntry: single(handlers.onEntry, decl.events, (h) => method<Active["onEntry"]>(h, handlers)),
    };
    signal.throwIfAborted();
    extensions.set(ext, active);
    committed = true;
    return null;
  } finally {
    signal.removeEventListener("abort", cancel);
    if (!signal.aborted) activating.delete(ext);
    if (!committed) lifetime.abort();
  }
}

function method<T>(value: unknown, owner: unknown): T | undefined {
  return typeof value === "function" ? (value.bind(owner) as T) : undefined;
}

function optionalMethod<T>(owner: Frame, key: string): T | undefined {
  const value = owner[key];
  if (value === undefined) return undefined;
  return method<T>(value, owner) ?? fail();
}

function schemeHandlers(value: unknown): SchemeHandlers | undefined {
  if (!isRecord(value)) return undefined;
  if (Object.keys(value).some((key) => key !== "resolve" && key !== "read" && key !== "write")) {
    fail();
  }
  const resolve = method<SchemeHandlers["resolve"]>(value.resolve, value);
  if (!resolve) return undefined;
  return {
    resolve,
    read: optionalMethod(value, "read"),
    write: optionalMethod(value, "write"),
  };
}

/** Binds one handler per declared name; every returned name must be declared and vice versa. */
function named<T>(
  value: unknown,
  names: string[],
  bind: (handler: unknown, owner: Frame) => T | undefined,
): Map<string, T> {
  const owner: Frame = value === undefined ? {} : isRecord(value) ? value : fail();
  for (const key of Object.keys(owner)) if (!names.includes(key)) fail();
  const out = new Map<string, T>();
  for (const name of names) {
    const handler = Object.hasOwn(owner, name) ? bind(owner[name], owner) : undefined;
    out.set(name, handler ?? fail());
  }
  return out;
}

/** Binds a singular handler the manifest declared; an undeclared one fails activation. */
function single<T>(value: unknown, declared: boolean, bind: (handler: unknown) => T | undefined): T | undefined {
  if (!declared) return value === undefined ? undefined : fail();
  return (value === undefined ? undefined : bind(value)) ?? fail();
}

type DeclaredProvider = { id: string; login: boolean; rewrite: boolean; transport: boolean; listModels: boolean };

function providers(value: unknown, declared: DeclaredProvider[]): Map<string, ProviderHandlers> {
  const returned: Frame =
    value === undefined ? {} : isRecord(value) ? value : fail();
  for (const key of Object.keys(returned)) {
    if (!declared.some((p) => p.id === key)) fail();
  }
  const out = new Map<string, ProviderHandlers>();
  for (const p of declared) {
    const value = Object.hasOwn(returned, p.id) ? returned[p.id] : undefined;
    if (value === undefined && !p.login && !p.rewrite && !p.transport && !p.listModels) continue;
    if (typeof value !== "object" || value === null) fail();
    const h = value as Record<string, unknown>;
    const wants: Record<string, boolean> = {
      login: p.login,
      refresh: p.login,
      rewrite: p.rewrite,
      send: p.transport,
      listModels: p.listModels,
    };
    for (const key of Object.keys(h)) {
      if (!Object.hasOwn(wants, key)) fail();
    }
    const bound: ProviderHandlers = {};
    for (const [key, wanted] of Object.entries(wants)) {
      const fn = h[key];
      if (wanted && typeof fn !== "function") fail();
      if (!wanted && fn !== undefined) fail();
      if (wanted) (bound as Record<string, unknown>)[key] = (fn as Function).bind(h);
    }
    out.set(p.id, bound);
  }
  return out;
}

// ---- Runner -> Rust session actions ----

type StreamEvent = SessionStreamEvent | CompletionStreamEvent;
type StreamListener = (event: StreamEvent) => void | Promise<void>;
const subscriptions = new Map<string, { ext: string; listener: StreamListener; remove(): void }>();
const registering = new Map<Promise<unknown>, string>();
/** The methods each object forwards; sent with every activation. */
const methods = new Map<string, Set<string>>();

function object<T extends object>(ext: string, name: string, base: T, target?: string, parent?: Parent): T {
  return new Proxy(base, {
    get(base, key) {
      if (Object.hasOwn(base, key)) return Reflect.get(base, key);
      if (typeof key !== "string" || key === "then" || !methods.get(name)?.has(key)) return undefined;
      return (...args: unknown[]) => {
        const last = args.at(-1);
        const own = last instanceof AbortSignal ? last : undefined;
        if (own) args.pop();
        return callHost(ext, "invoke", { object: name, target, method: key, args }, parent, own);
      };
    },
  });
}

/** Session actions for one session Rust can resolve by ID. */
function handle(ext: string, id: string, lifetime?: AbortSignal): SessionHandle {
  const subscribe: SessionHandle["subscribe"] = (listener, kinds) =>
    openSubscription(ext, { session: id, kinds }, listener, lifetime);
  return object(ext, "session", { id, subscribe }, id) as SessionHandle;
}

// Only IDs and owners outlive a failed activation; no listener or author context is retained.
const rollbacks = new Map<string, string>();
function rollbackSubscription(sub: string, ext: string): void {
  const running = rollbacks.size !== 0;
  rollbacks.set(sub, ext);
  if (running) return;
  void (async () => {
    for (const [id, owner] of rollbacks) {
      try {
        await startHostCall(owner, "unsubscribe", { sub: id }, undefined, undefined, true);
      } catch (error) {
        if (!(error instanceof HostError && error.code === "no_subscription")) violation("subscription rollback failed");
      }
      rollbacks.delete(id);
    }
  })();
}

async function openSubscription<E extends StreamEvent>(ext: string, args: Frame, listener: (event: E) => void | Promise<void>, lifetime?: AbortSignal) {
  lifetime?.throwIfAborted();
  const reply = callHost(ext, "subscribe", args, undefined, lifetime);
  registering.set(reply, ext);
  try {
    const sub = text(record(await reply).sub);
    let closing: Promise<void> | undefined;
    const remove = () => {
      subscriptions.delete(sub);
      lifetime?.removeEventListener("abort", rollback);
    };
    const rollback = () => {
      remove();
      if (!closing) rollbackSubscription(sub, ext);
    };
    if (lifetime?.aborted) {
      rollbackSubscription(sub, ext);
      return { close: () => acknowledged };
    }
    subscriptions.set(sub, { ext, listener: listener as StreamListener, remove });
    lifetime?.addEventListener("abort", rollback, { once: true });
    return {
      close() {
        closing ??= (async () => {
          if (!subscriptions.has(sub)) return;
          try {
            await callHost(ext, "unsubscribe", { sub }, undefined, lifetime);
          } catch (error) {
            // The host already ended the stream: closed either way.
            if (!(error instanceof HostError && error.code === "no_subscription")) {
              closing = undefined;
              if (lifetime?.aborted) rollbackSubscription(sub, ext);
              throw error;
            }
          }
          remove();
        })();
        return closing;
      },
    };
  } finally {
    registering.delete(reply);
  }
}

// ---- Runner -> Rust login callbacks ----

/**
 * The host UI for one login. Display calls (openUrl, deviceCode, progress) are tracked in
 * `notices` until Rust acknowledges them so the login cannot return past an undelivered one.
 */
function interaction(ext: string, parent: Parent, notices: { pending: Set<Promise<void>>; failed: boolean }): LoginInteraction {
  const ask = (method: string, args: Frame, own?: AbortSignal) => callHost(ext, method, args, parent, own);
  const notify = (method: string, build: () => Frame): Promise<void> => {
    let delivered: Promise<void>;
    try {
      delivered = ask(method, build()).then(() => undefined);
    } catch (error) {
      return Promise.reject(error);
    }
    notices.pending.add(delivered);
    // Keep failure, not every rejected promise; callers can still await the original rejection.
    delivered.then(() => notices.pending.delete(delivered), () => {
      notices.failed = true;
      notices.pending.delete(delivered);
    });
    return delivered;
  };
  const optional = (value: unknown, what: string): string | undefined =>
    value === undefined ? undefined : local(typeof value === "string", what, value as string);
  return {
    openUrl: (url, instructions) =>
      notify("openUrl", () => ({
        url: local(typeof url === "string", "openUrl url", url),
        instructions: optional(instructions, "openUrl instructions"),
      })),
    deviceCode: (code) =>
      notify("deviceCode", () => {
        const c = local(typeof code === "object" && code !== null, "deviceCode code", code);
        const expires = c.expiresInSeconds;
        return {
          userCode: local(typeof c.userCode === "string", "deviceCode userCode", c.userCode),
          verificationUrl: local(typeof c.verificationUrl === "string", "deviceCode verificationUrl", c.verificationUrl),
          expiresInSeconds:
            expires == null
              ? expires
              : local(Number.isSafeInteger(expires) && expires >= 0, "deviceCode expiresInSeconds", expires),
        };
      }),
    async prompt(prompt, signal) {
      const p = local(typeof prompt === "object" && prompt !== null, "prompt", prompt);
      return answer(
        await ask(
          "prompt",
          {
            message: local(typeof p.message === "string", "prompt message", p.message),
            placeholder: optional(p.placeholder ?? undefined, "prompt placeholder"),
            secret: local(typeof p.secret === "boolean", "prompt secret", p.secret),
          },
          signal,
        ),
      );
    },
    async select(select, signal) {
      const s = local(typeof select === "object" && select !== null, "select", select);
      local(Array.isArray(s.options), "select options", s.options);
      const options = s.options.map((o) => {
        local(typeof o === "object" && o !== null, "select option", o);
        return {
          id: local(typeof o.id === "string", "select option id", o.id),
          label: local(typeof o.label === "string", "select option label", o.label),
        };
      });
      return answer(
        await ask(
          "select",
          { message: local(typeof s.message === "string", "select message", s.message), options },
          signal,
        ),
      );
    },
    progress: (message) =>
      notify("progress", () => ({ message: local(typeof message === "string", "progress message", message) })),
  };
}

/** Rejects malformed author arguments, including session IDs, before sending to Rust. */
function local<T>(ok: boolean, what: string, value: T): T {
  if (!ok) throw new TypeError(`axo: invalid ${what}`);
  return value;
}

function answer(value: unknown): string {
  if (typeof value !== "string") throw new TypeError("axo: host answered with a non-string");
  return value;
}

/** The live Rust call a callback runs under; its signal aborts when that call ends. */
interface Parent {
  id: string;
  signal: AbortSignal;
}

/**
 * Calls Rust. With `parent` the callback lives no longer than that call; without one it is a
 * session action, independent of any call.
 */
function callHost(ext: string, method: string, args: Frame, parent?: Parent, own?: AbortSignal): Promise<unknown> {
  const outer = parent?.signal;
  if (parent && (outer!.aborted || !calls.has(parent.id))) {
    return Promise.reject(outer!.reason ?? new Error("call finished"));
  }
  if (own?.aborted) return Promise.reject(own.reason);
  if (activeHostCalls < MAX_HOST_CALLS) return startHostCall(ext, method, args, parent, own);
  if (queuedHostCalls.length >= MAX_HOST_CALLS) return Promise.reject(new HostError("busy", "host call queue is full"));
  const { promise, resolve, reject } = Promise.withResolvers<unknown>();
  const detach = () => {
    outer?.removeEventListener("abort", withdraw);
    own?.removeEventListener("abort", withdraw);
  };
  const withdraw = () => {
    const index = queuedHostCalls.indexOf(entry);
    if (index === -1) return;
    queuedHostCalls.splice(index, 1);
    entry.reject(outer?.aborted ? outer.reason : own!.reason);
    if (!queuedHostCalls.length) resetStallTimer();
  };
  const entry = {
    start() {
      detach();
      resolve(startHostCall(ext, method, args, parent, own));
    },
    reject(error: Error) {
      detach();
      reject(error);
    },
  };
  queuedHostCalls.push(entry);
  outer?.addEventListener("abort", withdraw, { once: true });
  own?.addEventListener("abort", withdraw, { once: true });
  if (queuedHostCalls.length === 1) resetStallTimer();
  return promise;
}

/** Rollback unsubscribes use one separate serial slot even when ordinary calls are stalled. */
function startHostCall(ext: string, method: string, args: Frame, parent?: Parent, own?: AbortSignal, rollback = false): Promise<unknown> {
  const outer = parent?.signal;
  if (outer?.aborted) return Promise.reject(outer.reason);
  if (own?.aborted) return Promise.reject(own.reason);
  if (!rollback) activeHostCalls++;
  const { promise, resolve, reject } = Promise.withResolvers<unknown>();
  const id = `r:${nextId++}`;
  const writer = new AbortController();
  let sent = false;
  const settle = (finish: () => void) => {
    if (!pending.delete(id)) return;
    if (!sent) writer.abort();
    outer?.removeEventListener("abort", onOuter);
    own?.removeEventListener("abort", onOwn);
    finish();
    if (!rollback) {
      activeHostCalls--;
      while (activeHostCalls < MAX_HOST_CALLS && queuedHostCalls.length) queuedHostCalls.shift()!.start();
      resetStallTimer();
    }
  };
  const onOuter = () => settle(() => reject(outer!.reason));
  const onOwn = () => {
    // Registration/close must consume the host's answer once sent to roll back exactly once.
    if (sent && (method === "subscribe" || method === "unsubscribe")) return;
    settle(() => {
      if (sent && !outer?.aborted) sendControl({ type: "cancel", id });
      reject(own!.reason);
    });
  };
  pending.set(id, {
    resolve: (value) => settle(() => resolve(value)),
    reject: (error) => settle(() => reject(error)),
  });
  outer?.addEventListener("abort", onOuter, { once: true });
  if (own !== outer) own?.addEventListener("abort", onOwn, { once: true });
  const frame = { type: "call", id, ext, method, args, parent: parent?.id };
  void send(frame, writer.signal, () => { sent = true; }).then((status) => {
    if (status !== "ok") pending.get(id)?.reject(new RangeError(`axo: ${method} arguments cannot be sent (${status})`));
  });
  return promise;
}

// ---- Value checks (Rust validates again before use) ----

function credential(value: unknown): Frame {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return invalidReply();
  const c = value as Record<string, unknown>;
  if (c.type === "api_key") {
    if (typeof c.key !== "string" || c.key === "") invalidReply();
  } else if (c.type === "oauth") {
    if (typeof c.access !== "string" || c.access === "") invalidReply();
    if (typeof c.refresh !== "string") invalidReply();
    if (!Number.isSafeInteger(c.expires)) invalidReply();
    if (c.baseUrl !== undefined && typeof c.baseUrl !== "string") invalidReply();
    if (c.headers !== undefined) {
      if (typeof c.headers !== "object" || c.headers === null || Array.isArray(c.headers)) invalidReply();
      if (Object.values(c.headers as object).some((v) => typeof v !== "string")) invalidReply();
    }
  } else {
    invalidReply();
  }
  return c;
}

function request(value: unknown): ProviderRequest {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return invalidReply();
  const r = value as Record<string, unknown>;
  if (typeof r.url !== "string" || !URL.canParse(r.url)) return invalidReply();
  const scheme = new URL(r.url).protocol;
  if (scheme !== "http:" && scheme !== "https:") invalidReply();
  if (!Array.isArray(r.headers)) return invalidReply();
  for (const pair of r.headers) {
    if (!Array.isArray(pair) || pair.length !== 2 || typeof pair[0] !== "string" || pair[0] === "") {
      invalidReply();
    }
    const v = pair[1];
    const binary =
      typeof v === "object" && v !== null && typeof v.base64 === "string" && Object.keys(v).length === 1;
    if (typeof v !== "string" && !binary) invalidReply();
  }
  if (r.body === undefined) invalidReply();
  return r as unknown as ProviderRequest;
}

function freeze<T>(value: T): T {
  if (typeof value === "object" && value !== null && !Object.isFrozen(value)) {
    Object.freeze(value);
    for (const child of Object.values(value)) freeze(child);
  }
  return value;
}
