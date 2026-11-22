// Test-only session extension for the `bun_provider` acceptance target, mirroring the scripted
// fake runner of `tests/bridge/sessions.rs`. Dependency-free: the SDK import is type-only.
//
// - Tool `read` sends one update (`partial`), then returns `ext:read:<arguments JSON>`.
// - Tool `probe {mode:"state"}` appends custom state `lab` = `{n:1}` and returns the session's
//   `custom("lab")` as JSON text.
// - Tool `probe {mode:"hang"}` sends one update, writes `probe-started` in `dataDir` and waits
//   for its signal; on abort it writes `canceled` and fails.
// - The tool policy blocks every call with reason `no` while `dataDir/block` exists.
// - Tool `burst` sends 300 updates in a plain loop, then returns `burst`.
// - `/hello <args>` posts a quiet, displayed `lab` message with text `<args>`.
// - `/flood` holds every runner host-call slot with `/hold-0`..`/hold-127` (one command may run
//   once per session at a time), then posts the error code of a queued 129th `/hold`.
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

const activate: Extension = ({ dataDir, session: getSession }) => ({
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
  },
  toolPolicy: {
    async before() {
      return existsSync(join(dataDir, "block")) ? { block: true, reason: "no" } : null;
    },
  },
  commands: {
    hello: (args, session) => session.post({ customType: "lab", text: args, display: true, delivery: "quiet" }),
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
      const run = (name: string) => session.runCommand({ type: "registered", name, args: "" });
      const occupied = Promise.allSettled(Array.from({ length: SLOTS }, (_, i) => run(`hold-${i}`)));
      let text = "not stalled";
      try {
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
