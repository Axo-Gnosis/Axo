// axo extension runner: the Bun-side peer of the bridge in this crate. The bridge embeds this
// file, writes it into a private per-generation directory and starts `bun run <file>`.
//
// Environment: AXO_EXTENSION_SOCKET (Unix socket path), AXO_EXTENSION_NONCE,
// AXO_EXTENSION_GENERATION (decimal string). They are removed from `process.env` before
// any extension code loads.
//
// Wire: each frame is a 4-byte unsigned big-endian length followed by that many bytes of
// UTF-8 JSON, at most 64 MiB in either direction. The runner connects and sends
//   {type:"hello",protocol:1,nonce,generation}
// Every frame after that, both ways, carries `generation` (the same opaque string).
//   {type:"call",id,ext,method,args,parent?}  id "h:<n>" from Rust, "r:<n>" from the runner
//   {type:"ok",id,value} | {type:"error",id,code,message}  terminal reply to a call
//   {type:"cancel",id}                         withdraw a call the sender made
//   {type:"ping",id} -> {type:"pong",id}       Rust pings; the runner answers from its event loop.
//                                              While the socket is blocked an unsent pong is
//                                              replaced by the newest one; any pong proves liveness.
// Rust -> runner methods: activate {entry,dataDir,providers:[{id,login,rewrite}]} -> null;
// login {provider} -> Credential; refresh {provider,credential} -> Credential;
// rewrite {provider,model,request} -> ProviderRequest.
// Runner -> Rust methods (only with `parent` = a live login call of the same `ext`):
// openUrl {url,instructions?} -> null; deviceCode {userCode,verificationUrl,expiresInSeconds?}
// -> null; prompt {message,placeholder?,secret} -> string; select {message,options:[{id,label}]}
// -> string; progress {message} -> null. A login replies only after every display/progress
// callback it made was acknowledged; an error reply to one fails the login.
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
  Credential,
  Extension,
  LoginInteraction,
  Model,
  ProviderRequest,
} from "../../../packages/axo-extension-sdk/index.ts";

const PROTOCOL = 1;
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

const HOST_METHODS: Record<string, true> = { activate: true, login: true, refresh: true, rewrite: true };
const RUNNER_METHODS: Record<string, true> = {
  openUrl: true,
  deviceCode: true,
  prompt: true,
  select: true,
  progress: true,
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

interface Active {
  login?: (ui: LoginInteraction, signal: AbortSignal) => Promise<Credential>;
  refresh?: (credential: Credential, signal: AbortSignal) => Promise<Credential>;
  rewrite?: (model: Readonly<Model>, request: ProviderRequest, signal: AbortSignal) => Promise<ProviderRequest>;
}

/** Activated extensions: provider ID -> bound handlers. */
const extensions = new Map<string, Map<string, Active>>();
const activating = new Set<string>();

function record(value: unknown, what: string): Frame {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Fail("invalid", `${what} is not an object`);
  }
  return value as Frame;
}

function text(value: unknown, what: string): string {
  if (typeof value !== "string") throw new Fail("invalid", `${what} is not a string`);
  return value;
}

async function run(id: string, call: Call, rawArgs: unknown): Promise<unknown> {
  // `method` is one of HOST_METHODS, so labels stay fixed strings; Rust names the owner.
  const { ext, method, controller } = call;
  const args = record(rawArgs, `${method} args`);
  if (method === "activate") return activate(ext, args, controller.signal);
  if (typeof args.provider !== "string") throw new Fail("invalid", `${method} provider is not a string`);
  const handlers = extensions.get(ext)?.get(args.provider);
  const signal = controller.signal;
  switch (method) {
    case "login": {
      if (!handlers?.login) throw new Fail("unsupported", "no login handler");
      const notices = new Set<Promise<void>>();
      const value = await handlers.login(interaction(ext, id, signal, notices), signal);
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
  if (extensions.has(ext) || activating.has(ext)) throw new Fail("invalid", "already activated");
  // Fixed messages only: author errors and author-chosen keys never reach diagnostics.
  const fail = (message: string): never => {
    throw new Fail("activation", message);
  };
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
      result = await (module.default as Extension)({ id: ext, dataDir });
    } catch {
      return fail("activation threw");
    }
    if (typeof result !== "object" || result === null || Array.isArray(result)) {
      return fail("activation did not return a provider record");
    }
    const returned = result as Record<string, unknown>;
    for (const key of Object.keys(returned)) {
      if (!declared.some((p) => p.id === key)) fail("activation returned an undeclared provider");
    }
    const providers = new Map<string, Active>();
    for (const [i, p] of declared.entries()) {
      const value = returned[p.id];
      if (value === undefined && !p.login && !p.rewrite) continue;
      if (typeof value !== "object" || value === null) fail(`provider ${i}: handlers missing`);
      const h = value as Record<string, unknown>;
      const wants: Record<string, boolean> = { login: p.login, refresh: p.login, rewrite: p.rewrite };
      for (const key of Object.keys(h)) {
        if (!Object.hasOwn(wants, key)) fail(`provider ${i}: undeclared method`);
      }
      const active: Active = {};
      for (const [key, wanted] of Object.entries(wants)) {
        const fn = h[key];
        if (wanted && typeof fn !== "function") fail(`provider ${i}: ${key} is not a function`);
        if (!wanted && fn !== undefined) fail(`provider ${i}: ${key} is not declared`);
        if (wanted) (active as Record<string, unknown>)[key] = (fn as Function).bind(h);
      }
      providers.set(p.id, active);
    }
    if (!signal.aborted) extensions.set(ext, providers);
    return null;
  } finally {
    activating.delete(ext);
  }
}

// ---- Runner -> Rust login callbacks ----

/**
 * The host UI for one login. Display calls (openUrl, deviceCode, progress) are tracked in
 * `notices` until Rust acknowledges them so the login cannot return past an undelivered one.
 */
function interaction(ext: string, parent: string, outer: AbortSignal, notices: Set<Promise<void>>): LoginInteraction {
  const ask = (method: string, args: Frame, own?: AbortSignal) =>
    callHost(ext, parent, method, args, outer, own);
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

function callHost(
  ext: string,
  parent: string,
  method: string,
  args: Frame,
  outer: AbortSignal,
  own?: AbortSignal,
): Promise<unknown> {
  const { promise, resolve, reject } = Promise.withResolvers<unknown>();
  if (outer.aborted || !calls.has(parent)) {
    reject(outer.reason ?? new Error("login finished"));
    return promise;
  }
  if (own?.aborted) {
    reject(own.reason);
    return promise;
  }
  const id = `r:${nextId++}`;
  let sent = false;
  const settle = (finish: () => void) => {
    pending.delete(id);
    outer.removeEventListener("abort", onOuter);
    own?.removeEventListener("abort", onOwn);
    finish();
  };
  // The parent ended: Rust has already dropped its callbacks, so no cancel frame.
  const onOuter = () => settle(() => reject(outer.reason));
  const onOwn = () =>
    settle(() => {
      if (sent) sendControl({ type: "cancel", id });
      reject(own!.reason);
    });
  pending.set(id, {
    resolve: (value) => settle(() => resolve(value)),
    reject: (error) => settle(() => reject(error)),
  });
  outer.addEventListener("abort", onOuter, { once: true });
  own?.addEventListener("abort", onOwn, { once: true });
  void send({ type: "call", id, ext, method, args, parent }, () => (sent = pending.has(id))).then((status) => {
    if (status !== "ok") pending.get(id)?.reject(new RangeError(`axo: ${method} arguments cannot be sent (${status})`));
  });
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
