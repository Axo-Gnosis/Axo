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
// - Each journal entry appends its `type` and a newline to `dataDir/entries`.
// - Scheme `lab://` is virtual; reading `lab://<path>` returns `read:<path>`.
//
// Barrier files are written atomically (temp file + rename).

import { appendFileSync, existsSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { Extension } from "../../../../../packages/axo-extension-sdk/index.ts";

function writeAtomic(dir: string, name: string, contents: string): void {
  const tmp = join(dir, `.${name}.${process.pid}.tmp`);
  writeFileSync(tmp, contents);
  renameSync(tmp, join(dir, name));
}

const activate: Extension = ({ dataDir }) => ({
  tools: {
    async read({ call, update }) {
      update({ content: [{ type: "text", text: "partial" }] });
      return { content: [{ type: "text", text: `ext:read:${JSON.stringify(call.arguments)}` }] };
    },
    async probe({ call, session, signal, update }) {
      if (call.arguments.mode === "state") {
        await session.appendCustom("lab", { n: 1 });
        return { content: [{ type: "text", text: JSON.stringify(await session.custom("lab")) }] };
      }
      update({ content: [{ type: "text", text: "partial" }] });
      writeAtomic(dataDir, "probe-started", "");
      const { promise: aborted, resolve } = Promise.withResolvers<void>();
      signal.addEventListener("abort", () => resolve(), { once: true });
      if (signal.aborted) resolve();
      await aborted;
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
  },
  schemes: {
    lab: {
      resolve: async () => "virtual",
      read: async (path) => `read:${path}`,
    },
  },
  onEntry(seq) {
    const { entry } = seq as { entry: { type: string } };
    appendFileSync(join(dataDir, "entries"), `${entry.type}\n`);
  },
});

export default activate;
