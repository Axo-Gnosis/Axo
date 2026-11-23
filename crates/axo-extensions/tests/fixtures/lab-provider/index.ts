// Test-only provider extension for the `bun_provider` acceptance target. Dependency-free: the
// SDK import is type-only and erased by Bun. Everything it talks to is the loopback lab
// server named by the explicit `LAB_URL` environment variable; nothing here is a vendor flow.
//
// Provider key: `lab` for the `lab-provider` package, otherwise the extension ID, so a host
// can install renamed copies (with their manifest's provider ID set to the extension ID) as
// distinct members of one group.
//
// Control files in `dataDir`, all written atomically (temp file + rename):
// - Each activation spawns one long-lived child and writes `runner.pid` and `child.pid`
//   (decimal PIDs). The child is ordinary, or detached via `setsid` when `LAB_DETACH=1`.
// - `hang`: the next rewrite removes it, writes `hang-entered` and blocks the JS thread
//   forever (the heartbeat watchdog must kill the runner). Removing the trigger first lets
//   the replacement generation serve requests.
// - `hold-rewrite`: the next rewrite removes it, writes `rewrite-entered`, then waits until
//   `release-rewrite` exists, removes that and completes.

import { spawn } from "node:child_process";
import { existsSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import type {
  CredentialInput,
  CredentialOutput,
  Extension,
  LoginInteraction,
  ProviderRequest,
} from "../../../../../packages/axo-extension-sdk/index.ts";

const POLL_MS = 10;

function labUrl(): string {
  const url = process.env.LAB_URL;
  if (!url) throw new Error("LAB_URL is not set");
  return url.replace(/\/+$/, "");
}

function writeAtomic(dir: string, name: string, contents: string): void {
  const tmp = join(dir, `.${name}.${process.pid}.tmp`);
  writeFileSync(tmp, contents);
  renameSync(tmp, join(dir, name));
}

/** Removes `path` and reports whether it existed. */
function take(path: string): boolean {
  if (!existsSync(path)) return false;
  rmSync(path, { force: true });
  return true;
}

async function post(path: string, body: unknown, signal: AbortSignal): Promise<Record<string, unknown>> {
  const response = await fetch(`${labUrl()}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
    signal,
  });
  if (!response.ok) throw new Error(`lab ${path}: HTTP ${response.status}`);
  return (await response.json()) as Record<string, unknown>;
}

function tokens(reply: Record<string, unknown>, path: string): { access: string; refresh: string } {
  const { access, refresh } = reply;
  if (typeof access !== "string" || typeof refresh !== "string") {
    throw new Error(`lab ${path}: malformed token reply`);
  }
  return { access, refresh };
}

function spawnChild(dataDir: string): void {
  const detached = process.env.LAB_DETACH === "1";
  const child = spawn(process.execPath, ["-e", "setInterval(() => {}, 1 << 30)"], {
    stdio: "ignore",
    detached,
  });
  if (typeof child.pid !== "number") throw new Error("lab child did not spawn");
  // The child is the supervisor's to reap; it must not keep this runner's loop alive.
  child.unref();
  writeAtomic(dataDir, "child.pid", `${child.pid}\n`);
  writeAtomic(dataDir, "runner.pid", `${process.pid}\n`);
}

const activate: Extension = ({ id, dataDir }) => {
  spawnChild(dataDir);
  const key = id === "lab-provider" ? "lab" : id;

  async function login(ui: LoginInteraction, signal: AbortSignal): Promise<CredentialInput> {
    const device = await post("/device", {}, signal);
    const { userCode, verificationUrl } = device;
    if (typeof userCode !== "string" || typeof verificationUrl !== "string") {
      throw new Error("lab /device: malformed reply");
    }
    await ui.deviceCode({ userCode, verificationUrl });
    const answer = await ui.prompt({ message: "Enter the lab approval", secret: false }, signal);
    await ui.progress("Exchanging lab approval");
    const { access, refresh } = tokens(await post("/token", { answer }, signal), "/token");
    return { type: "oauth", access, refresh, expires: Date.now() + 60_000 };
  }

  async function refresh(credential: CredentialOutput, signal: AbortSignal): Promise<CredentialInput> {
    if (credential.type !== "oauth") throw new Error("lab refresh needs an OAuth credential");
    const reply = await post("/refresh", { refresh: credential.refresh }, signal);
    const next = tokens(reply, "/refresh");
    return { type: "oauth", ...next, expires: Date.now() + 3_600_000 };
  }

  async function rewrite(
    _model: unknown,
    request: ProviderRequest,
    signal: AbortSignal,
  ): Promise<ProviderRequest> {
    if (take(join(dataDir, "hang"))) {
      writeAtomic(dataDir, "hang-entered", "");
      // Deliberately starve the event loop: only the heartbeat watchdog can end this.
      for (;;);
    }
    if (take(join(dataDir, "hold-rewrite"))) {
      writeAtomic(dataDir, "rewrite-entered", "");
      while (!take(join(dataDir, "release-rewrite"))) await sleep(POLL_MS, undefined, { signal });
    }
    const body = request.body;
    if (typeof body !== "object" || body === null || Array.isArray(body)) {
      throw new Error("lab rewrite expects a JSON object body");
    }
    // Rust's own authorization header passes through unchanged.
    return {
      url: `${labUrl()}/inference`,
      headers: [...request.headers, ["x-lab", "yes"]],
      body: { ...body, lab: true },
    };
  }

  return { providers: { [key]: { login, refresh, rewrite } } };
};

export default activate;
