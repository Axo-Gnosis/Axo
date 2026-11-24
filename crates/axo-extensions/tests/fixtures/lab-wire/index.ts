// Test-only provider extension owning its transport and model list for the `bun_provider`
// acceptance target. `send` posts to the lab server's native completions route (named by
// `LAB_URL`), never to the model's unreachable base URL; `listModels` adds `listed`.

import type { Extension, Model, ProviderRequest } from "../../../../../packages/axo-extension-sdk/index.ts";

function labUrl(): string {
  const url = process.env.LAB_URL;
  if (!url) throw new Error("LAB_URL is not set");
  return url.replace(/\/+$/, "");
}

const activate: Extension = ({ id }) => {
  async function send(_model: Readonly<Model>, request: ProviderRequest, _context: unknown, signal: AbortSignal) {
    const headers = request.headers.filter((pair): pair is [string, string] => typeof pair[1] === "string");
    return fetch(`${labUrl()}/native/chat/completions`, {
      method: "POST",
      headers,
      body: JSON.stringify(request.body),
      signal,
    });
  }

  async function listModels(): Promise<Model[]> {
    return ["m", "listed"].map((model) => ({
      id: model,
      name: model,
      api: "openai-completions",
      provider: id,
      baseUrl: "http://127.0.0.1:9/v1",
      reasoning: false,
      input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      contextWindow: 200000,
      maxTokens: 4096,
    })) as Model[];
  }

  return { providers: { [id]: { send, listModels } } };
};

export default activate;
