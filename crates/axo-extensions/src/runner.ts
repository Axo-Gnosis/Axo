// axo extension runner: the Bun-side peer of the bridge in this crate. The bridge embeds this
// file, writes it into a private per-generation directory and starts `bun run <file>`.
//
// Environment: AXO_EXTENSION_SOCKET (Unix socket path), AXO_EXTENSION_NONCE,
// AXO_EXTENSION_GENERATION (decimal string). They are removed from `process.env` before
// any extension code loads.
//
// Wire: each frame is a 4-byte unsigned big-endian length followed by that many bytes of
// UTF-8 JSON, at most 64 MiB in either direction. The runner connects and sends
//   {type:"hello",protocol:2,nonce,generation}
// Every frame after that, both ways, carries `generation` (the same opaque string).
//   {type:"call",id,ext,method,args,parent?}  id "h:<n>" from Rust, "r:<n>" from the runner
//   {type:"ok",id,value} | {type:"error",id,code,message}  terminal reply to a call
//   {type:"cancel",id}                         withdraw a call the sender made
//   {type:"ping",id} -> {type:"pong",id}       Rust pings; the runner answers from its event loop.
//                                              While the socket is blocked an unsent pong is
//                                              replaced by the newest one; any pong proves liveness.
// Rust -> runner methods:
//   activate {entry,dataDir,providers:[{id,login,rewrite}],
//             session:{tools:[{name,..}],schemes:[{name,..}],commands:[{name,..}],
//                      toolPolicy:["before"|"after"],spawnHook,events},
//             sessionMethods:[<invokable session method>]} -> null
//   login {provider} -> Credential; refresh {provider,credential} -> Credential;
//   rewrite {provider,model,request} -> ProviderRequest
//   tool {tool,session,call:{id,name,arguments}} -> {content,details?,isError?,terminate?}
//   policyBefore {scope,call} -> null | {block:true,reason?,terminate?}
//   policyAfter {scope,call,result:{content,details?,isError}}
//     -> null | {content?,details?,isError?,terminate?}
//   spawn {session,spawn} -> null | {block} | {replace}
//   schemeResolve {scheme,path,session:{id,dir,cwd}} -> "virtual" | {file,writable}
//   schemeRead {scheme,path,session} -> string; schemeWrite {scheme,path,session,content} -> string
//   command {command,session,args} -> null; event {session,entry} -> null
//   stream {sub,event} | {sub,lagged} | {sub,closed:true} -> null
// Runner -> Rust methods with `parent` = a live call of the same `ext`:
//   under login: openUrl {url,instructions?} -> null; deviceCode {userCode,verificationUrl,
//   expiresInSeconds?} -> null; prompt {message,placeholder?,secret} -> string;
//   select {message,options:[{id,label}]} -> string; progress {message} -> null.
//   under tool: update {content,details?} -> null.
// A login replies only after every display/progress callback it made was acknowledged; an
// error reply to one fails the login. A tool replies only after every update it sent was
// answered; an error reply to an update is ignored.
// Runner -> Rust session actions, without `parent`, valid while the session is attached:
//   invoke {object:"session",target:<session id>,method,args:[...]} -> method result; a
//   session handle forwards only `sessionMethods` names.
//   subscribe {session,kinds} -> {sub}; unsubscribe {sub} -> null.
// Error codes the runner sends: "activation" (import/activation of that member failed; the
// bridge disables only that member), "failed" (the author's method threw), "invalid" (bad
// arguments, an invalid author return value, or a reply that is not JSON-serializable),
// "unsupported" (no such handler or method), "too_large" (the reply would exceed the frame
// bound). Messages are fixed runner labels: never author exceptions, payloads or IDs; Rust
// names the owner from the call. A canceled call gets no reply.
// Protocol violations (bad length or JSON, wrong generation, unknown frame type,
// duplicate live ID, wrong-direction method) end the process with status 1; Rust sees EOF.
// The runner exits with status 0 when Rust closes the socket.

import { connect } from "node:net";
import { pathToFileURL } from "node:url";
import type {
  CommandHandler,
  Credential,
  Extension,
  Handlers,
  LoginInteraction,
  Model,
  ProviderRequest,
  SchemeContext,
  SchemeHandlers,
  SeqEntry,
  SessionEvent,
  SessionHandle,
  SessionStreamEvent,
  SpawnContext,
  SpawnHook,
  ToolCall,
  ToolHandler,
  ToolPolicy,
  ToolResult,
  ToolRun,
  ToolScope,
} from "../../../packages/axo-extension-sdk/index.ts";

const PROTOCOL = 2;
const MAX_FRAME = 64 * 1024 * 1024;
// Bytes held ahead of the socket before replies and callbacks wait for their turn.
const QUEUE_LIMIT = MAX_FRAME;

const socketPath = process.env.AXO_EXTENSION_SOCKET;
const nonce = process.env.AXO_EXTENSION_NONCE;
const generation = process.env.AXO_EXTENSION_GENERATION;
delete process.env.AXO_EXTENSION_SOCKET;
delete process.env.AXO_EXTENSION_NONCE;
delete process.env.AXO_EXTENSION_GENERATION;
if (!socketPath || !nonce || !generation || !/^\d+$/.test(generation)) {
  console.error("axo runner: missing or invalid AXO_EXTENSION_* environment");
  process.exit(2);
}

type Frame = { [key: string]: unknown };

/** An error whose code and message go to Rust as-is. */
class Fail extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

/** A Rust error reply to a runner callback. */
class HostError extends Error {
  constructor(
    readonly code: string,
    message: string,
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
    json = JSON.stringify({ ...frame, generation });
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
 * Queues a frame on its turn. Skips it when `live()` turned false while waiting;
 * otherwise resolves "ok" or why the frame could not be encoded.
 */
async function send(frame: Frame, live: () => boolean = () => true): Promise<"ok" | "too_large" | "invalid"> {
  const { promise, resolve } = Promise.withResolvers<void>();
  turns.push(resolve);
  grant();
  await promise;
  try {
    if (!live()) return "ok";
    const buf = encode(frame);
    if (typeof buf === "string") return buf;
    frames.push(buf);
    queued += buf.length;
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

socket.on("connect", () => sendControl({ type: "hello", protocol: PROTOCOL, nonce }));
socket.on("close", () => process.exit(0));
socket.on("error", () => {
  console.error("axo runner: socket error");
  process.exit(1);
});

// ---- Dispatch ----

const HOST_METHODS: Record<string, true> = {
  activate: true,
  login: true,
  refresh: true,
  rewrite: true,
  tool: true,
  policyBefore: true,
  policyAfter: true,
  spawn: true,
  schemeResolve: true,
  schemeRead: true,
  schemeWrite: true,
  command: true,
  event: true,
  stream: true,
};
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
/** Calls waiting for a slot under `MAX_HOST_CALLS`, started in order. */
const queuedHostCalls: { start(): void; reject(error: Error): void }[] = [];
// ponytail: a stall is detected, not prevented; exempt re-entrant calls from the slot cap if real workloads hit it
const STALL_MS = 30_000;
let stallTimer: NodeJS.Timeout | undefined;

function resetStallTimer(): void {
  clearTimeout(stallTimer);
  stallTimer = queuedHostCalls.length ? setTimeout(() => {
    stallTimer = undefined;
    for (const call of queuedHostCalls) call.reject(new HostError("stalled", "too many nested host calls"));
    queuedHostCalls.length = 0;
  }, STALL_MS) : undefined;
}

function onFrame(frame: Frame): void {
  if (frame.generation !== generation) violation("wrong generation");
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
      return waiter.reject(new HostError(code, message));
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
  if (!Object.hasOwn(HOST_METHODS, method)) {
    void send({ type: "error", id, code: "unsupported", message: "unknown method" });
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
      const message = status === "too_large" ? `${method} reply exceeds 64 MiB` : `${method} reply is not JSON`;
      await send({ type: "error", id, code: status, message });
    }
  };
  run(id, call, frame.args).then(
    (value) => finish({ type: "ok", id, value: value ?? null }),
    (error) => finish(errorReply(id, error, method)),
  );
}

/** Author exceptions are never logged or forwarded; Rust gets the fixed method label. */
function errorReply(id: string, error: unknown, method: string): Frame {
  if (error instanceof Fail) return { type: "error", id, code: error.code, message: error.message };
  return { type: "error", id, code: "failed", message: `${method} failed` };
}

// ---- Rust -> runner methods ----

interface Provider {
  login?: (ui: LoginInteraction, signal: AbortSignal) => Promise<Credential>;
  refresh?: (credential: Credential, signal: AbortSignal) => Promise<Credential>;
  rewrite?: (model: Readonly<Model>, request: ProviderRequest, signal: AbortSignal) => Promise<ProviderRequest>;
}

/** A tool policy hook the manifest declares. */
type PolicyHook = "before" | "after";

/** One activated extension's bound handlers, exactly as its activate call declared them. */
interface Active {
  providers: Map<string, Provider>;
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

function isRecord(value: unknown): value is Frame {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function record(value: unknown, what: string): Frame {
  if (!isRecord(value)) throw new Fail("invalid", `${what} is not an object`);
  return value;
}

function text(value: unknown, what: string): string {
  if (typeof value !== "string") throw new Fail("invalid", `${what} is not a string`);
  return value;
}

async function run(id: string, call: Call, rawArgs: unknown): Promise<unknown> {
  // `method` is one of HOST_METHODS, so labels stay fixed strings; Rust names the owner.
  const { ext, method, controller } = call;
  const signal = controller.signal;
  const args = record(rawArgs, `${method} args`);
  if (method === "activate") return activate(ext, args, signal);
  const active = extensions.get(ext);
  switch (method) {
    case "login":
    case "refresh":
    case "rewrite":
      return provider(id, call, active, args);
    case "tool": {
      const handler = active?.tools.get(text(args.tool, "tool name"));
      if (!handler) throw new Fail("unsupported", "no tool handler");
      return tool(id, call, handler, args);
    }
    case "policyBefore": {
      const before = active?.toolPolicy?.before;
      if (!before) throw new Fail("unsupported", "no tool policy handler");
      const scope = freeze(record(args.scope, "policyBefore scope")) as unknown as ToolScope;
      const value = await before(scope, toolCall(args.call, "policyBefore call"), signal);
      return decision(value, method, { block: "true", reason: "string?", terminate: "boolean?" });
    }
    case "policyAfter": {
      const after = active?.toolPolicy?.after;
      if (!after) throw new Fail("unsupported", "no tool policy handler");
      const scope = freeze(record(args.scope, "policyAfter scope")) as unknown as ToolScope;
      const result = record(args.result, "policyAfter result") as unknown as Parameters<typeof after>[2];
      const value = await after(scope, toolCall(args.call, "policyAfter call"), result, signal);
      return decision(value, method, {
        content: "array?",
        details: "any?",
        isError: "boolean?",
        terminate: "boolean?",
      });
    }
    case "spawn": {
      if (!active?.spawnHook) throw new Fail("unsupported", "no spawn hook");
      const request = record(args.spawn, "spawn request") as unknown as SpawnContext;
      const value = await active.spawnHook(request, text(args.session, "spawn session"), signal);
      if (isRecord(value) && Object.hasOwn(value, "replace")) return decision(value, method, { replace: "object" });
      return decision(value, method, { block: "string" });
    }
    case "schemeResolve":
    case "schemeRead":
    case "schemeWrite": {
      const name = text(args.scheme, `${method} scheme`);
      const scheme = active?.schemes.get(name);
      if (!scheme) throw new Fail("unsupported", "no scheme handler");
      const path = text(args.path, `${method} path`);
      const session = record(args.session, `${method} session`);
      const cx: SchemeContext = freeze({
        scheme: name,
        session: {
          id: text(session.id, `${method} session id`),
          dir: text(session.dir, `${method} session dir`),
          cwd: text(session.cwd, `${method} session cwd`),
        },
      });
      if (method === "schemeResolve") {
        const target = await scheme.resolve(path, cx, signal);
        if (target === "virtual") return target;
        return decision(target, method, { file: "string", writable: "boolean" }) ?? invalidReply(method);
      }
      if (method === "schemeRead") {
        if (!scheme.read) throw new Fail("unsupported", "no scheme read handler");
        return text(await scheme.read(path, cx, signal), "schemeRead result");
      }
      if (!scheme.write) throw new Fail("unsupported", "no scheme write handler");
      const content = text(args.content, "schemeWrite content");
      return text(await scheme.write(path, content, cx, signal), "schemeWrite result");
    }
    case "command": {
      const handler = active?.commands.get(text(args.command, "command name"));
      if (!handler) throw new Fail("unsupported", "no command handler");
      await handler(text(args.args, "command args"), handle(ext, text(args.session, "command session")), signal);
      return null;
    }
    case "stream": {
      const sub = text(args.sub, "stream subscription");
      // A live session can emit before Rust's subscribe reply reaches the socket.
      while (!subscriptions.has(sub)) {
        const replies = [];
        for (const [reply, owner] of registering) if (owner === ext) replies.push(reply);
        if (!replies.length) break;
        await Promise.race(replies).catch(() => {});
      }
      const subscription = subscriptions.get(sub);
      if (subscription?.ext !== ext) throw new Fail("unsupported", "no stream listener");
      let event: SessionStreamEvent;
      if (args.closed === true) {
        subscriptions.delete(sub);
        event = { type: "closed", value: null };
      } else if (typeof args.lagged === "number") {
        event = { type: "lagged", value: { skipped: args.lagged } };
      } else {
        event = record(args.event, "stream event") as unknown as SessionEvent;
      }
      const { listener } = subscription;
      try {
        await listener(event);
      } catch {
        // One listener error must not end the subscription.
      }
      return null;
    }
    default: {
      // event
      if (!active?.onEntry) throw new Fail("unsupported", "no entry handler");
      const session = handle(ext, text(args.session, "event session"));
      await active.onEntry(freeze(record(args.entry, "event entry")) as unknown as SeqEntry, session);
      return null;
    }
  }
}

async function provider(id: string, call: Call, active: Active | undefined, args: Frame): Promise<unknown> {
  const { ext, method, controller } = call;
  const signal = controller.signal;
  if (typeof args.provider !== "string") throw new Fail("invalid", `${method} provider is not a string`);
  const handlers = active?.providers.get(args.provider);
  switch (method) {
    case "login": {
      if (!handlers?.login) throw new Fail("unsupported", "no login handler");
      const notices = new Set<Promise<void>>();
      const value = await handlers.login(interaction(ext, { id, signal }, notices), signal);
      // A credential never outruns a display or progress message the host has not acknowledged.
      try {
        while (notices.size) await Promise.all(notices);
      } catch {
        throw new Fail("failed", "login display not acknowledged");
      }
      return credential(value, method);
    }
    case "refresh": {
      if (!handlers?.refresh) throw new Fail("unsupported", "no refresh handler");
      const input = credential(args.credential, "refresh argument");
      return credential(await handlers.refresh(input, signal), method);
    }
    default: {
      if (!handlers?.rewrite) throw new Fail("unsupported", "no rewrite handler");
      const model = freeze(record(args.model, "rewrite model")) as unknown as Model;
      const input = request(args.request, "rewrite argument");
      return request(await handlers.rewrite(model, input, signal), method);
    }
  }
}

async function tool(id: string, call: Call, handler: ToolHandler, args: Frame): Promise<ToolResult> {
  const { ext, controller } = call;
  const signal = controller.signal;
  const parent: Parent = { id, signal };
  // Every update is answered before the result goes out, so none trails it; a refused
  // update is the host's business, not the tool's failure.
  const updates = new Set<Promise<unknown>>();
  const run: ToolRun = {
    call: toolCall(args.call, "tool call"),
    sessionId: text(args.session, "tool session"),
    signal,
    update(update) {
      const u = local(isRecord(update), "update", update);
      const content = local(Array.isArray(u.content), "update content", u.content);
      const sent = callHost(ext, "update", { content, details: u.details }, parent);
      updates.add(sent);
      const done = () => updates.delete(sent);
      sent.then(done, done);
    },
  };
  const value = await handler(run);
  while (updates.size) await Promise.allSettled(updates);
  const result = decision(value, "tool", {
    content: "array",
    details: "any?",
    isError: "boolean?",
    terminate: "boolean?",
  });
  return (result ?? invalidReply("tool")) as unknown as ToolResult;
}

function toolCall(value: unknown, what: string): ToolCall {
  const call = record(value, what);
  text(call.id, `${what} id`);
  text(call.name, `${what} name`);
  record(call.arguments, `${what} arguments`);
  return call as unknown as ToolCall;
}

type Check = "true" | "string" | "boolean" | "array" | "object" | "any";

function invalidReply(label: string): never {
  throw new Fail("invalid", `${label} returned an invalid result`);
}

/**
 * An author's reply: `null` or `undefined` as `null`, otherwise a record with only the
 * `shape` keys (`?` marks optional); anything else fails "{label} returned an invalid result".
 */
function decision(value: unknown, label: string, shape: Record<string, Check | `${Check}?`>): Frame | null {
  if (value === null || value === undefined) return null;
  if (!isRecord(value) || Object.keys(value).some((key) => !Object.hasOwn(shape, key))) return invalidReply(label);
  const out: Frame = {};
  for (const [key, spec] of Object.entries(shape)) {
    const v = value[key];
    const optional = spec.endsWith("?");
    const check = (optional ? spec.slice(0, -1) : spec) as Check;
    const ok =
      v === undefined
        ? optional
        : check === "any" ||
          (check === "true" && v === true) ||
          (check === "array" && Array.isArray(v)) ||
          (check === "object" && isRecord(v)) ||
          ((check === "string" || check === "boolean") && typeof v === check);
    if (!ok) invalidReply(label);
    if (v !== undefined) out[key] = v;
  }
  return out;
}

/** Activation failures carry fixed messages only: author errors and keys never reach diagnostics. */
function fail(message: string): never {
  throw new Fail("activation", message);
}

const HANDLER_KINDS = ["providers", "tools", "schemes", "commands", "toolPolicy", "spawnHook", "onEntry"];

async function activate(ext: string, args: Frame, signal: AbortSignal): Promise<null> {
  const entry = text(args.entry, "activate entry");
  const dataDir = text(args.dataDir, "activate dataDir");
  if (!Array.isArray(args.providers)) throw new Fail("invalid", "activate providers is not an array");
  const declared = args.providers.map((raw, i) => {
    const p = record(raw, `activate provider ${i}`);
    if (typeof p.login !== "boolean" || typeof p.rewrite !== "boolean") {
      throw new Fail("invalid", `activate provider ${i} flags are not booleans`);
    }
    return { id: text(p.id, `activate provider ${i} id`), login: p.login, rewrite: p.rewrite };
  });
  const session = record(args.session, "activate session");
  const methods = args.sessionMethods;
  if (!Array.isArray(methods) || !methods.every((m) => typeof m === "string")) {
    throw new Fail("invalid", "activate sessionMethods is not a string list");
  }
  sessionMethods = new Set(methods);
  const names = (key: string): string[] => {
    const list = session[key];
    if (!Array.isArray(list)) throw new Fail("invalid", `activate session ${key} is not an array`);
    return list.map((raw, i) => text(record(raw, `activate ${key} ${i}`).name, `activate ${key} ${i} name`));
  };
  const flag = (key: string): boolean => {
    const value = session[key];
    if (typeof value !== "boolean") throw new Fail("invalid", `activate session ${key} is not a boolean`);
    return value;
  };
  const hooks = (): PolicyHook[] => {
    const list = session.toolPolicy;
    const valid =
      Array.isArray(list) &&
      list.every((hook, i) => (hook === "before" || hook === "after") && list.indexOf(hook) === i);
    if (!valid) throw new Fail("invalid", "activate session toolPolicy is not a hook list");
    return list as PolicyHook[];
  };
  const decl = {
    tools: names("tools"),
    schemes: names("schemes"),
    commands: names("commands"),
    toolPolicy: hooks(),
    spawnHook: flag("spawnHook"),
    events: flag("events"),
  };
  if (extensions.has(ext) || activating.has(ext)) throw new Fail("invalid", "already activated");
  activating.add(ext);
  try {
    let module: { default?: unknown };
    try {
      // The entry is a runtime-selected, host-admitted extension file.
      module = await import(pathToFileURL(entry).href);
    } catch {
      return fail("import failed");
    }
    if (typeof module.default !== "function") return fail("default export is not a function");
    let result: unknown;
    try {
      result = await (module.default as Extension)({
        id: ext,
        dataDir,
        session: (id) => handle(ext, local(typeof id === "string", "session id", id)),
      });
    } catch {
      return fail("activation threw");
    }
    const handlers: Frame = isRecord(result) ? result : fail("activation did not return a handler record");
    for (const key of Object.keys(handlers)) {
      if (!HANDLER_KINDS.includes(key)) fail("activation returned an unknown handler kind");
    }
    const active: Active = {
      providers: providers(handlers.providers, declared),
      tools: named<ToolHandler>(handlers.tools, decl.tools, "tool", method),
      schemes: named(handlers.schemes, decl.schemes, "scheme", schemeHandlers),
      commands: named<CommandHandler>(handlers.commands, decl.commands, "command", method),
      toolPolicy: single(handlers.toolPolicy, decl.toolPolicy.length > 0, "tool policy", (h): ToolPolicy | undefined => {
        if (!isRecord(h)) return undefined;
        if (Object.keys(h).some((key) => key !== "before" && key !== "after")) fail("tool policy handler malformed");
        const bind = <T>(hook: PolicyHook): T | undefined => {
          if (!decl.toolPolicy.includes(hook)) {
            return h[hook] === undefined ? undefined : fail("undeclared tool policy handler");
          }
          return method<T>(h[hook], h) ?? fail("tool policy handler missing");
        };
        return { before: bind<ToolPolicy["before"]>("before"), after: bind<ToolPolicy["after"]>("after") };
      }),
      spawnHook: single(handlers.spawnHook, decl.spawnHook, "spawn hook", (h) => method<SpawnHook>(h, handlers)),
      onEntry: single(handlers.onEntry, decl.events, "entry", (h) => method<Active["onEntry"]>(h, handlers)),
    };
    if (!signal.aborted) extensions.set(ext, active);
    return null;
  } finally {
    activating.delete(ext);
  }
}

function method<T>(value: unknown, owner: unknown): T | undefined {
  return typeof value === "function" ? (value.bind(owner) as T) : undefined;
}

function optionalMethod<T>(owner: Frame, key: string, kind: string): T | undefined {
  const value = owner[key];
  if (value === undefined) return undefined;
  return method<T>(value, owner) ?? fail(`${kind} handler malformed`);
}

function schemeHandlers(value: unknown): SchemeHandlers | undefined {
  if (!isRecord(value)) return undefined;
  if (Object.keys(value).some((key) => key !== "resolve" && key !== "read" && key !== "write")) {
    fail("scheme handler malformed");
  }
  const resolve = method<SchemeHandlers["resolve"]>(value.resolve, value);
  if (!resolve) return undefined;
  return {
    resolve,
    read: optionalMethod(value, "read", "scheme"),
    write: optionalMethod(value, "write", "scheme"),
  };
}

/** Binds one handler per declared name; every returned name must be declared and vice versa. */
function named<T>(
  value: unknown,
  names: string[],
  kind: string,
  bind: (handler: unknown, owner: Frame) => T | undefined,
): Map<string, T> {
  const owner: Frame = value === undefined ? {} : isRecord(value) ? value : fail(`${kind} handlers are not a record`);
  for (const key of Object.keys(owner)) if (!names.includes(key)) fail(`undeclared ${kind} handler`);
  const out = new Map<string, T>();
  for (const name of names) {
    const handler = Object.hasOwn(owner, name) ? bind(owner[name], owner) : undefined;
    out.set(name, handler ?? fail(`${kind} handler missing`));
  }
  return out;
}

/** Binds a singular handler the manifest declared; an undeclared one fails activation. */
function single<T>(value: unknown, declared: boolean, kind: string, bind: (handler: unknown) => T | undefined): T | undefined {
  if (!declared) return value === undefined ? undefined : fail(`undeclared ${kind} handler`);
  return (value === undefined ? undefined : bind(value)) ?? fail(`${kind} handler missing`);
}

function providers(value: unknown, declared: { id: string; login: boolean; rewrite: boolean }[]): Map<string, Provider> {
  const returned: Frame =
    value === undefined ? {} : isRecord(value) ? value : fail("activation did not return a provider record");
  for (const key of Object.keys(returned)) {
    if (!declared.some((p) => p.id === key)) fail("activation returned an undeclared provider");
  }
  const out = new Map<string, Provider>();
  for (const [i, p] of declared.entries()) {
    const value = returned[p.id];
    if (value === undefined && !p.login && !p.rewrite) continue;
    if (typeof value !== "object" || value === null) fail(`provider ${i}: handlers missing`);
    const h = value as Record<string, unknown>;
    const wants: Record<string, boolean> = { login: p.login, refresh: p.login, rewrite: p.rewrite };
    for (const key of Object.keys(h)) {
      if (!Object.hasOwn(wants, key)) fail(`provider ${i}: undeclared method`);
    }
    const bound: Provider = {};
    for (const [key, wanted] of Object.entries(wants)) {
      const fn = h[key];
      if (wanted && typeof fn !== "function") fail(`provider ${i}: ${key} is not a function`);
      if (!wanted && fn !== undefined) fail(`provider ${i}: ${key} is not declared`);
      if (wanted) (bound as Record<string, unknown>)[key] = (fn as Function).bind(h);
    }
    out.set(p.id, bound);
  }
  return out;
}

// ---- Runner -> Rust session actions ----

type StreamListener = Parameters<SessionHandle["subscribe"]>[0];
const subscriptions = new Map<string, { ext: string; listener: StreamListener }>();
const registering = new Map<Promise<unknown>, string>();
/** The session methods a handle forwards; sent with every activation. */
let sessionMethods = new Set<string>();

/** Session actions for one session; unparented, so they work whenever Rust has it attached. */
function handle(ext: string, id: string): SessionHandle {
  const subscribe: SessionHandle["subscribe"] = async (listener, kinds) => {
    const reply = callHost(ext, "subscribe", { session: id, kinds: kinds ?? null });
    registering.set(reply, ext);
    try {
      const sub = text(record(await reply, "subscribe reply").sub, "subscription id");
      subscriptions.set(sub, { ext, listener });
      // Memoized; the listener stays registered until Rust confirms, so a failed close retries.
      let closing: Promise<void> | undefined;
      return {
        close() {
          closing ??= (async () => {
            if (!subscriptions.has(sub)) return;
            try {
              await callHost(ext, "unsubscribe", { sub });
            } catch (error) {
              // The host already ended the stream: closed either way.
              if (!(error instanceof HostError && error.code === "no_subscription")) {
                closing = undefined;
                throw error;
              }
            }
            subscriptions.delete(sub);
          })();
          return closing;
        },
      };
    } finally {
      registering.delete(reply);
    }
  };
  return new Proxy({ id, subscribe }, {
    get(target, key) {
      if (key === "id" || key === "subscribe") return target[key];
      if (typeof key !== "string" || !sessionMethods.has(key)) return undefined;
      return (...args: unknown[]) => callHost(ext, "invoke", { object: "session", target: id, method: key, args });
    },
  }) as SessionHandle;
}

// ---- Runner -> Rust login callbacks ----

/**
 * The host UI for one login. Display calls (openUrl, deviceCode, progress) are tracked in
 * `notices` until Rust acknowledges them so the login cannot return past an undelivered one.
 */
function interaction(ext: string, parent: Parent, notices: Set<Promise<void>>): LoginInteraction {
  const ask = (method: string, args: Frame, own?: AbortSignal) => callHost(ext, method, args, parent, own);
  const notify = (method: string, build: () => Frame): Promise<void> => {
    let delivered: Promise<void>;
    try {
      delivered = ask(method, build()).then(() => undefined);
    } catch (error) {
      return Promise.reject(error);
    }
    notices.add(delivered);
    // Fulfilled notices are forgotten; a rejected one stays so the login fails on it. The
    // no-op handler marks the rejection handled, so an unawaited call cannot crash the runner.
    delivered.then(() => notices.delete(delivered), () => {});
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
            expires === undefined
              ? undefined
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
            placeholder: optional(p.placeholder, "prompt placeholder"),
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

/** Rejects an author's malformed UI argument locally; nothing reaches Rust. */
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
  const { promise, resolve, reject } = Promise.withResolvers<unknown>();
  const outer = parent?.signal;
  const start = () => {
    if (parent && (parent.signal.aborted || !calls.has(parent.id))) {
      reject(parent.signal.reason ?? new Error("call finished"));
      return;
    }
    if (own?.aborted) {
      reject(own.reason);
      return;
    }
    const id = `r:${nextId++}`;
    let sent = false;
    const settle = (finish: () => void) => {
      pending.delete(id);
      outer?.removeEventListener("abort", onOuter);
      own?.removeEventListener("abort", onOwn);
      finish();
      // A queued start that rejects early takes no slot, so the loop moves on.
      while (pending.size < MAX_HOST_CALLS && queuedHostCalls.length) queuedHostCalls.shift()!.start();
      resetStallTimer();
    };
    // The parent ended: Rust has already dropped its callbacks, so no cancel frame.
    const onOuter = () => settle(() => reject(outer!.reason));
    const onOwn = () =>
      settle(() => {
        if (sent) sendControl({ type: "cancel", id });
        reject(own!.reason);
      });
    pending.set(id, {
      resolve: (value) => settle(() => resolve(value)),
      reject: (error) => settle(() => reject(error)),
    });
    outer?.addEventListener("abort", onOuter, { once: true });
    own?.addEventListener("abort", onOwn, { once: true });
    const frame = { type: "call", id, ext, method, args, parent: parent?.id };
    void send(frame, () => (sent = pending.has(id))).then((status) => {
      if (status !== "ok") pending.get(id)?.reject(new RangeError(`axo: ${method} arguments cannot be sent (${status})`));
    });
  };
  if (pending.size < MAX_HOST_CALLS) start();
  else {
    queuedHostCalls.push({ start, reject });
    if (queuedHostCalls.length === 1) resetStallTimer();
  }
  return promise;
}

// ---- Value checks (Rust validates again before use) ----

function credential(value: unknown, label: string): Credential {
  const bad = (what: string): never => {
    throw new Fail("invalid", `${label}: invalid credential (${what})`);
  };
  if (typeof value !== "object" || value === null || Array.isArray(value)) return bad("not an object");
  const c = value as Record<string, unknown>;
  if (c.type === "api_key") {
    if (typeof c.key !== "string" || c.key === "") bad("key");
  } else if (c.type === "oauth") {
    if (typeof c.access !== "string" || c.access === "") bad("access");
    if (typeof c.refresh !== "string") bad("refresh");
    if (!Number.isSafeInteger(c.expires)) bad("expires");
    if (c.baseUrl !== undefined && typeof c.baseUrl !== "string") bad("baseUrl");
    if (c.headers !== undefined) {
      if (typeof c.headers !== "object" || c.headers === null || Array.isArray(c.headers)) bad("headers");
      if (Object.values(c.headers as object).some((v) => typeof v !== "string")) bad("headers");
    }
  } else {
    bad("type");
  }
  return c as unknown as Credential;
}

function request(value: unknown, label: string): ProviderRequest {
  const bad = (what: string): never => {
    throw new Fail("invalid", `${label}: invalid request (${what})`);
  };
  if (typeof value !== "object" || value === null || Array.isArray(value)) return bad("not an object");
  const r = value as Record<string, unknown>;
  if (typeof r.url !== "string" || !URL.canParse(r.url)) return bad("url");
  const scheme = new URL(r.url).protocol;
  if (scheme !== "http:" && scheme !== "https:") bad("url scheme");
  if (!Array.isArray(r.headers)) return bad("headers");
  r.headers.forEach((pair, i) => {
    if (!Array.isArray(pair) || pair.length !== 2 || typeof pair[0] !== "string" || pair[0] === "") {
      bad(`header ${i}`);
    }
    const v = pair[1];
    const binary =
      typeof v === "object" && v !== null && typeof v.base64 === "string" && Object.keys(v).length === 1;
    if (typeof v !== "string" && !binary) bad(`header ${i} value`);
  });
  if (r.body === undefined) bad("body");
  return r as unknown as ProviderRequest;
}

function freeze<T>(value: T): T {
  if (typeof value === "object" && value !== null && !Object.isFrozen(value)) {
    Object.freeze(value);
    for (const child of Object.values(value)) freeze(child);
  }
  return value;
}
