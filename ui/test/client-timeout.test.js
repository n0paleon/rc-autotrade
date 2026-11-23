import { describe, expect, it } from "bun:test";
import { WorkerClient } from "../src/lib/client.js";

function aborted(signal) {
  return new Promise((_, reject) => {
    signal.addEventListener("abort", () => {
      const error = new Error("aborted");
      error.name = "AbortError";
      reject(error);
    });
  });
}

function clientWithFetch(fetchImpl) {
  const client = new WorkerClient();
  client.baseUrl = "http://127.0.0.1:8787";
  client.token = "test-token";
  const original = globalThis.fetch;
  globalThis.fetch = fetchImpl;
  return { client, restore: () => { globalThis.fetch = original; } };
}

describe("rawRequest deadlines", () => {
  it("reports a silent worker when headers never arrive", async () => {
    const { client, restore } = clientWithFetch((_url, init) => aborted(init.signal));
    try {
      await expect(client.request("/api/state", { timeout: 20 })).rejects.toThrow(
        "The worker did not answer in time"
      );
    } finally {
      restore();
    }
  });

  it("reports a slow body when headers arrive but the stream stalls", async () => {
    const { client, restore } = clientWithFetch((_url, init) =>
      Promise.resolve({
        ok: true,
        text: () => aborted(init.signal)
      })
    );
    try {
      await expect(
        client.request("/api/state", { timeout: 500, bodyTimeout: 20 })
      ).rejects.toThrow("The worker answered, but the response took too long to download.");
    } finally {
      restore();
    }
  });

  it("lets a transfer use the whole body budget after fast headers", async () => {
    let abortedPhase = null;
    const { client, restore } = clientWithFetch((_url, init) => {
      init.signal.addEventListener("abort", () => { abortedPhase = true; });
      return Promise.resolve({
        ok: true,
        text: () => Promise.resolve('{"ok":true}')
      });
    });
    try {
      const result = await client.request("/api/state", { timeout: 500, bodyTimeout: 500 });
      expect(result).toEqual({ ok: true });
      expect(abortedPhase).toBeNull();
    } finally {
      restore();
    }
  });
});
