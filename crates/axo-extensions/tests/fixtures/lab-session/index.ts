// Test-only session extension for the `bun_provider` acceptance target, mirroring the scripted
// fake runner of `tests/bridge/sessions.rs`. Dependency-free: the SDK import is type-only.
//
// - Tool `read` sends one update (`partial`), then returns `ext:read:<arguments JSON>`.
// - Tool `probe {mode:"state"}` appends custom state `lab` = `{n:1}` and returns the session's
//   `custom("lab")` as JSON text.
// - Tool `probe {mode:"hang"}` sends one update, writes `probe-started` in `dataDir` and waits
//   for its signal; on abort it writes `canceled` and fails.
// - The tool policy blocks every call with reason `no` while `dataDir/block` exists.
// - The spawn hook records its task and originating call ID in `dataDir/spawn.json`, then blocks.
// - Tool `burst` sends 300 updates in a plain loop, then returns `burst`.
// - `/hello <args>` posts a quiet, displayed `lab` message with text `<args>`.
// - `/flood` holds every runner host-call slot with `/hold-0`..`/hold-127` (one command may run
//   once per session at a time), withdraws a full queue, then posts the next queued call's stall code.
// - Each journal entry appends its `type` and a newline to `dataDir/entries`.
// - Scheme `lab://` is virtual; reading `lab://<path>` returns `read:<path>`.
//
// Barrier files are written atomically (temp file + rename).

import { appendFileSync, existsSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { CommandHandler, Extension } from "../../../../../packages/axo-extension-sdk/index.ts";

function writeAtomic(dir: string, name: string, contents: string): void {
  const tmp = join(dir, `.${name}.${process.pid}.tmp`);
  writeFileSync(tmp, contents);
  renameSync(tmp, join(dir, name));
}

async function waitForAbort(signal: AbortSignal): Promise<void> {
  if (signal.aborted) return;
  const { promise, resolve } = Promise.withResolvers<void>();
  signal.addEventListener("abort", () => resolve(), { once: true });
  await promise;
}

// The flood must free its own slots before it can post the stall result.
const floods = new Map<string, AbortSignal>();

/** Runner host-call slots (`MAX_HOST_CALLS` in runner.ts); `/flood` fills each with a distinct `/hold-<i>`. */
const SLOTS = 128;

const hold: CommandHandler = async (_args, session, signal) => {
  const flood = floods.get(session.id);
  const stop = flood ? AbortSignal.any([signal, flood]) : signal;
  await waitForAbort(stop);
};

const activate: Extension = ({ dataDir, host, models, session: getSession }) => ({
  tools: {
    async read({ call, update }) {
      update({ content: [{ type: "text", text: "partial" }] });
      return { content: [{ type: "text", text: `ext:read:${JSON.stringify(call.arguments)}` }] };
    },
    async probe({ call, sessionId, signal, update }) {
      if (call.arguments.mode === "state") {
        const session = getSession(sessionId);
        await session.appendCustom("lab", { n: 1 });
        return { content: [{ type: "text", text: JSON.stringify(await session.custom("lab")) }] };
      }
      update({ content: [{ type: "text", text: "partial" }] });
      writeAtomic(dataDir, "probe-started", "");
      await waitForAbort(signal);
      writeAtomic(dataDir, "canceled", "");
      throw signal.reason;
    },
    async burst({ update }) {
      for (let i = 0; i < 300; i++) update({ content: [{ type: "text", text: `${i}` }] });
      return { content: [{ type: "text", text: "burst" }] };
    },
    async "touch-two"(run) {
      await run.tools.writeFile("a.txt", "first");
      await run.tools.writeFile("b.txt", "second");
      return { content: [] };
    },
    async stale(run) {
      await run.tools.writeFile("seen.txt", "new");
      return { content: [] };
    },
    async sh(run) {
      return run.call.arguments.background
        ? await run.tools.runBash({ command: "sleep 0.2; echo bg", background: true })
        : await run.tools.runBash({ command: "echo hi" });
    },
  },
  toolPolicy: {
    async before(_scope, _cx, _signal) {
      return existsSync(join(dataDir, "block")) ? { block: true, reason: "no" } : null;
    },
  },
  async spawnHook(spawn) {
    const callId = spawn.origin.type === "tool" ? spawn.origin.callId : null;
    writeAtomic(dataDir, "spawn.json", JSON.stringify({ task: spawn.task, callId }));
    return { block: "spawn recorded" };
  },
  commands: {
    hello: (args, session) => session.post({ customType: "lab", text: args, display: true, delivery: "quiet" }),
    async "ask-model"(args, session) {
      const count = (await models.all()).length;
      let reply;
      try {
        reply = await models.complete({
          model: "test/m",
          messages: [{ role: "user", content: args, timestamp: Date.now() }],
        });
      } catch (error) {
        if (args !== "report-error" || !(error instanceof Error && "code" in error && "details" in error)) throw error;
        const { code, message, details } = error;
        await session.post({ customType: "lab", text: JSON.stringify({ code, message, details }), display: true, delivery: "quiet" });
        return;
      }
      const text = Array.isArray(reply.content)
        ? reply.content.flatMap(block => block.type === "text" ? [block.text] : []).join("")
        : reply.content;
      await session.post({ customType: "lab", text: `${count}: ${text}`, display: true, delivery: "quiet" });
    },
    async "fork-here"(_args, session) {
      let page = await session.entries();
      while (page.more) page = await session.entries(page.entries.at(-1)!.seq);
      const id = page.entries.at(-1)?.entry.id;
      if (typeof id !== "string") throw new Error("no entry ID to fork at");
      const receipt = await host.forkSession(session.id, id);
      await getSession(receipt.session).setName("forked");
      await session.post({ customType: "lab", text: receipt.session, display: true, delivery: "quiet" });
    },
    async "plan-task"(args, session) {
      await session.runCommand({ type: "mode", name: "plan", prompt: args });
    },
    async "skill-input"(args, session) {
      await session.followUp(await session.renderSkill("review", args));
    },
    async "skill-focus"(args, session) {
      await session.compact(await session.renderSkill("review", args));
    },
    async again(_args, session) {
      try {
        await session.runCommand({ type: "registered", name: "again", args: "" });
      } catch (error) {
        if (!(error instanceof Error)) throw error;
        await session.post({ customType: "lab", text: error.message, display: true, delivery: "quiet" });
      }
    },
    hold,
    ...Object.fromEntries(Array.from({ length: SLOTS }, (_, i) => [`hold-${i}`, hold])),
    async flood(_args, session, signal) {
      const release = new AbortController();
      floods.set(session.id, AbortSignal.any([signal, release.signal]));
      const run = (name: string, ...stop: [] | [AbortSignal]) => session.runCommand({ type: "registered", name, args: "" }, ...stop);
      const occupied = Promise.allSettled(Array.from({ length: SLOTS }, (_, i) => run(`hold-${i}`)));
      let text = "not stalled";
      try {
        const controllers = Array.from({ length: SLOTS }, () => new AbortController());
        const waiting = Promise.allSettled(controllers.map(({ signal }) => run("hold", signal)));
        try {
          await run("hold");
          throw new Error("full queue accepted another call");
        } catch (error) {
          if (!(error instanceof Error && "code" in error && error.code === "busy" && error.message === "host call queue is full")) throw error;
        }
        controllers.forEach((controller, i) => controller.abort(new Error(`withdraw ${i}`)));
        for (const [i, result] of (await waiting).entries()) {
          if (result.status !== "rejected" || result.reason !== controllers[i]!.signal.reason) throw new Error("queued withdrawal lost its reason");
        }
        await run("hold");
      } catch (error) {
        if (!(error instanceof Error && "code" in error)) throw error;
        text = String(error.code);
      } finally {
        release.abort();
        await occupied;
        floods.delete(session.id);
      }
      await session.post({ customType: "lab", text, display: true, delivery: "quiet" });
    },
    async watch(_args, session) {
      if (JSON.stringify(session) !== JSON.stringify({ id: session.id })) {
        throw new Error("session handle serialized through the host");
      }
      await session.subscribe((event) => {
        if (event.type === "entry") appendFileSync(join(dataDir, "watch.log"), `${event.value.seq}\n`);
      }, ["entry"]);
    },
    async "compact-light"(_args, session) {
      try {
        await session.compact("keep the plan and open questions");
      } catch (error) {
        if (!(error instanceof Error && "code" in error && error.code === "busy")) throw error;
        await session.post({ customType: "lab", text: "busy", display: true, delivery: "quiet" });
      }
    },
  },
  schemes: {
    lab: {
      resolve: async () => "virtual",
      read: async (path) => `read:${path}`,
    },
  },
  onEntry({ entry }) {
    appendFileSync(join(dataDir, "entries"), `${entry.type}\n`);
  },
});

export default activate;
