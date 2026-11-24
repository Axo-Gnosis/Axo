import { expect, test } from "bun:test";
import { once } from "node:events";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Exercise the embedded runner protocol directly: control when each registration's ID arrives.
async function* frames(socket: Socket) {
  let bytes = Buffer.alloc(0);
  for await (const chunk of socket) {
    bytes = Buffer.concat([bytes, chunk]);
    while (bytes.length >= 4) {
      const end = 4 + bytes.readUInt32BE();
      if (bytes.length < end) break;
      yield JSON.parse(bytes.toString("utf8", 4, end));
      bytes = bytes.subarray(end);
    }
  }
}

for (const cancel of [false, true]) {
  test(`activation ${cancel ? "cancellation" : "failure"} retires only its own subscriptions`, async () => {
    const dir = mkdtempSync(join(tmpdir(), "axo-runner-"));
    const server = createServer();
    let child: Bun.Subprocess | undefined;
    let socket: Socket | undefined;
    try {
      const path = join(dir, "sock");
      server.listen(path);
      await once(server, "listening");
      const connected = once(server, "connection", { signal: AbortSignal.timeout(5000) });
      child = Bun.spawn([process.execPath, join(import.meta.dir, "../src/runner.ts")], {
        env: { ...process.env, AXO_EXTENSION_SOCKET: path, AXO_EXTENSION_NONCE: "nonce" },
        stderr: "inherit",
      });
      const [connection] = await connected;
      const peer: Socket = socket = connection;
      peer.setTimeout(5000, () => peer.destroy(new Error("runner timed out")));
      const messages = frames(peer);
      const next = async () => {
        const { done, value } = await messages.next();
        if (done) throw new Error("runner disconnected");
        return value;
      };
      const send = (frame: object) => {
        const body = Buffer.from(JSON.stringify(frame));
        const prefix = Buffer.alloc(4);
        prefix.writeUInt32BE(body.length);
        peer.write(Buffer.concat([prefix, body]));
      };
      let serial = 0;
      const call = (ext: string, method: string, args: object) => {
        const id = `h:${++serial}`;
        send({ type: "call", id, ext, method, args });
        return id;
      };
      const ok = (id: string, value: unknown = null) => send({ type: "ok", id, value });
      const activate = (ext: string, module: string, commands: string[]) => call(ext, "activate", {
        entry: join(dir, module), dataDir: dir,
        providers: [{ id: "constructor", login: false, rewrite: false }],
        session: { commands: commands.map(name => ({ name })), tools: [], schemes: [], events: false, spawnHook: false, toolPolicy: [] },
        methods: { host: ["ready", "wait"] },
      });
      const command = (ext: string, command: string) => call(ext, "command", { session: "s", command, args: "" });
      expect(await next()).toEqual({ type: "hello", nonce: "nonce" });
      writeFileSync(join(dir, "good.ts"), `export default cx => ({ commands: {
        ping() {},
        async probe() { try { await globalThis.old.subscribe(() => {}); } catch { return; } throw Error('stale handle accepted'); },
        async fill() { await Promise.all(Array.from({length:127}, () => cx.host.wait())); }
      } });`);
      const commands = ["ping", "probe", "fill"];
      const good = activate("good", "good.ts", commands);
      expect(await next()).toMatchObject({ type: "ok", id: good });
      writeFileSync(join(dir, "bad.ts"), `export default async cx => {
        globalThis.old = cx.session('s');
        await globalThis.old.subscribe(() => { throw Error('retired listener'); });
        void cx.models.stream({}, () => {});
        await cx.host.ready();
        ${cancel ? "return new Promise(() => {});" : "return { invalid: true };"}
      };`);
      const bad = activate("bad", "bad.ts", []);
      const first = await next();
      expect(first).toMatchObject({ method: "subscribe", args: { session: "s" } });
      ok(first.id, { sub: "first" });
      const second = await next();
      expect(second).toMatchObject({ method: "subscribe", args: { completion: {} } });
      const barrier = await next();
      expect(barrier).toMatchObject({ method: "invoke", args: { method: "ready" } });
      ok(barrier.id);
      const held: string[] = [];
      let fill: string | undefined;
      if (cancel) {
        // One sent subscribe + 127 unanswered invokes: rollback must have its own slot.
        fill = command("good", "fill");
        for (let i = 0; i < 127; i++) {
          const frame = await next();
          expect(frame).toMatchObject({ ext: "good", args: { method: "wait" } });
          held.push(frame.id);
        }
        send({ type: "cancel", id: bad });
      }
      let unsubscribe = await next();
      if (!cancel) {
        const other = await next();
        const failure = unsubscribe.type === "error" ? unsubscribe : other;
        expect(failure).toMatchObject({ id: bad, type: "error", code: "activation" });
        if (failure === unsubscribe) unsubscribe = other;
      }
      expect(unsubscribe).toMatchObject({ ext: "bad", method: "unsubscribe", args: { sub: "first" } });
      ok(second.id, { sub: "late" });
      // Delivery must not wait for unsubscribe acknowledgement or enter either listener.
      for (const sub of ["first", "late"]) {
        const delivery = call("bad", "stream", { sub, event: { type: "test" } });
        expect(await next()).toMatchObject({ type: "error", id: delivery, code: "unsupported" });
      }
      ok(unsubscribe.id);
      const late = await next();
      expect(late).toMatchObject({ ext: "bad", method: "unsubscribe", args: { sub: "late" } });
      ok(late.id);
      for (const id of held) ok(id);
      if (fill) expect(await next()).toMatchObject({ type: "ok", id: fill });
      const resumed = activate("bad", "good.ts", commands);
      expect(await next()).toMatchObject({ type: "ok", id: resumed });
      for (const [ext, name] of [["bad", "probe"], ["good", "ping"]] as const) {
        const id = command(ext, name);
        expect(await next()).toMatchObject({ type: "ok", id });
      }
    } finally {
      socket?.destroy();
      if (child) { child.kill(); await child.exited; }
      server.close();
      rmSync(dir, { recursive: true, force: true });
    }
  }, 15000);
}
