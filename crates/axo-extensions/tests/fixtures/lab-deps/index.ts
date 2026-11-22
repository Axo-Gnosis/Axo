// Test-only extension with an npm dependency for the `bun_provider` install cases. The
// dependency is a `file:` package under `vendor/`, so `bun install` stays offline.
//
// - Tool `dep` returns the dependency's `value` (`dep-ok`).

import { value } from "lab-dep";
import type { Extension } from "../../../../../packages/axo-extension-sdk/index.ts";

const activate: Extension = () => ({
  tools: {
    async dep() {
      return { content: [{ type: "text", text: value }] };
    },
  },
});

export default activate;
