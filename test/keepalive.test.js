import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { loadConfig } from "../src/worker/config.js";
import { RollercoinApi } from "../src/worker/api.js";
import { createHttpDispatcher, httpFetch } from "../src/worker/http.js";
import { KeepaliveService } from "../src/worker/keepalive.js";

function jsonResponse(payload, status = 200) {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { "content-type": "application/json" }
  });
}

function waitFor(predicate, timeoutMs = 1_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const check = () => {
      if (predicate()) return resolve();
      if (Date.now() >= deadline) return reject(new Error("condition not met before timeout"));
      setTimeout(check, 5);
    };
    check();
  });
}

test("createHttpDispatcher reuses one socket across sequential requests", async () => {
  let connections = 0;
  const server = createServer((req, res) => res.end("ok"));
  server.on("connection", () => {
    connections += 1;
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));

  const url = `http://127.0.0.1:${server.address().port}/`;
  const dispatcher = createHttpDispatcher({ keepAliveMs: 60_000, connections: 2 });
  try {
    for (let i = 0; i < 3; i += 1) {
      const response = await httpFetch(url, { dispatcher });
      await response.arrayBuffer();
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    assert.equal(connections, 1);
  } finally {
    await dispatcher.close();
    await new Promise((resolve) => server.close(resolve));
  }
});

test("config keep-alive defaults survive a missing or empty env", () => {
  const missing = loadConfig({ cwd: "/nonexistent-turu-rc", env: {} });
  assert.equal(missing.apiBase, "https://rollercoin.com");
  assert.equal(missing.httpKeepAliveMs, 60_000);
  assert.equal(missing.httpConnections, 8);
  assert.equal(missing.keepAliveMs, 30_000);
  assert.equal(missing.keepAliveUrl, "https://rollercoin.com/api/events/config");

  const empty = loadConfig({
    cwd: "/nonexistent-turu-rc",
    env: {
      RC_API_BASE: "",
      RC_HTTP_KEEPALIVE_MS: "",
      RC_HTTP_CONNECTIONS: "",
      RC_KEEPALIVE_MS: "",
      RC_KEEPALIVE_URL: ""
    }
  });
  assert.equal(empty.httpKeepAliveMs, 60_000);
  assert.equal(empty.httpConnections, 8);
  assert.equal(empty.keepAliveMs, 30_000);
  assert.equal(empty.keepAliveUrl, "https://rollercoin.com/api/events/config");
});

test("config keep-alive values are clamped and RC_KEEPALIVE_MS=0 disables probing", () => {
  const clamped = loadConfig({
    cwd: "/nonexistent-turu-rc",
    env: {
      RC_API_BASE: "https://example.test/",
      RC_HTTP_KEEPALIVE_MS: "1",
      RC_HTTP_CONNECTIONS: "999",
      RC_KEEPALIVE_MS: "1000",
      RC_KEEPALIVE_URL: "/api/events/config"
    }
  });
  assert.equal(clamped.httpKeepAliveMs, 1_000);
  assert.equal(clamped.httpConnections, 32);
  assert.equal(clamped.keepAliveMs, 5_000);
  assert.equal(clamped.keepAliveUrl, "https://example.test/api/events/config");

  const disabled = loadConfig({
    cwd: "/nonexistent-turu-rc",
    env: { RC_KEEPALIVE_MS: "0" }
  });
  assert.equal(disabled.keepAliveMs, 0);

  assert.throws(
    () => loadConfig({ cwd: "/nonexistent-turu-rc", env: { RC_KEEPALIVE_URL: "http://[bad" } }),
    /RC_KEEPALIVE_URL must be a valid URL/
  );
});

test("RollercoinApi sends every upstream call through the shared dispatcher", async () => {
  const dispatcher = { name: "test-pool" };
  const seen = [];
  const session = {
    getAccessToken: () => "access-token",
    getCookie: () => "cf_clearance=test",
    refresh: async () => false
  };
  const api = new RollercoinApi({
    baseUrl: "https://rollercoin.test",
    session,
    logger: null,
    dispatcher,
    fetchImpl: async (url, init) => {
      seen.push({ url, init });
      if (url.includes("/purchase-item")) {
        return jsonResponse({ success: true, data: { usersPurchaseOrder: "order-1" } });
      }
      if (url.includes("/item-info")) {
        return jsonResponse({ success: true, data: { name: "Rare Neon Shard" } });
      }
      return jsonResponse({ data: { access_token: "next-access", refresh_token: "next-refresh" } });
    }
  });

  const purchase = await api.purchase({
    itemId: "item-1",
    qty: 2,
    unitPriceRaw: 10
  });
  assert.equal(purchase.ok, true);
  assert.equal(purchase.orderId, "order-1");

  const info = await api.fetchItemInfo({ itemId: "item-1" });
  assert.equal(info.ok, true);

  const refreshed = await api.refreshSession("refresh-token");
  assert.equal(refreshed.ok, true);
  assert.equal(refreshed.accessToken, "next-access");

  assert.equal(seen.length, 3);
  for (const call of seen) {
    assert.equal(call.init.dispatcher, dispatcher);
  }
  assert.ok(api.getLastRequestAt() > 0);
});

test("KeepaliveService probes with GET, drains the body, and reports warm stats", async () => {
  const calls = [];
  const dispatcher = { name: "test-pool" };
  const service = new KeepaliveService({
    url: "https://rollercoin.com/api/events/config",
    intervalMs: 30_000,
    dispatcher,
    fetchImpl: async (url, init) => {
      calls.push({ url, init });
      return jsonResponse({ ok: true });
    },
    now: () => 1_000
  });

  const result = await service.probeOnce();
  assert.equal(result.ok, true);
  assert.equal(result.status, 200);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, "https://rollercoin.com/api/events/config");
  assert.equal(calls[0].init.method, "GET");
  assert.equal(calls[0].init.dispatcher, dispatcher);
  assert.equal(calls[0].init.headers.origin, "https://rollercoin.com");

  const stats = service.stats();
  assert.equal(stats.probes, 1);
  assert.equal(stats.failures, 0);
  assert.equal(stats.lastStatus, 200);
  assert.equal(stats.lastSuccessAt, 1_000);
});

test("KeepaliveService backs off on errors and suspends on 403/429", async () => {
  let status = 500;
  const service = new KeepaliveService({
    url: "https://rollercoin.com/api/events/config",
    intervalMs: 30_000,
    suspendMs: 60_000,
    fetchImpl: async () => jsonResponse({ error: "nope" }, status),
    now: () => 5_000
  });

  const failed = await service.probeOnce();
  assert.equal(failed.ok, false);
  assert.equal(failed.status, 500);
  assert.equal(service.stats().consecutiveFailures, 1);
  assert.equal(service.stats().lastError, "HTTP 500");
  assert.equal(service.stats().suspendedUntil, null);

  status = 429;
  const suspended = await service.probeOnce();
  assert.equal(suspended.ok, false);
  assert.equal(suspended.suspendedUntil, 65_000);
  assert.equal(service.stats().suspendedUntil, 65_000);
});

test("KeepaliveService skips the probe when a real request just happened", async () => {
  let probes = 0;
  const service = new KeepaliveService({
    url: "https://rollercoin.com/api/events/config",
    intervalMs: 30_000,
    api: { getLastRequestAt: () => Date.now() },
    fetchImpl: async () => {
      probes += 1;
      return jsonResponse({ ok: true });
    }
  });

  service.start();
  service.wake("test");
  await waitFor(() => service.stats().lastSkipAt !== null);
  service.stop();

  assert.equal(probes, 0);
  assert.ok(service.stats().lastSkipAt > 0);
});

test("KeepaliveService aborts an inflight probe on stop without counting it as a failure", async () => {
  const service = new KeepaliveService({
    url: "https://rollercoin.com/api/events/config",
    intervalMs: 30_000,
    fetchImpl: (url, { signal }) =>
      new Promise((resolve, reject) => {
        signal.addEventListener("abort", () => {
          reject(Object.assign(new Error("aborted"), { name: "AbortError" }));
        });
      })
  });

  service.start();
  const inflight = service.probeOnce();
  await new Promise((resolve) => setTimeout(resolve, 10));
  service.stop();
  const result = await inflight;

  assert.equal(result.aborted, true);
  assert.equal(service.stats().failures, 0);
});

test("KeepaliveService probes on its own schedule and stops cleanly", async () => {
  let probes = 0;
  const service = new KeepaliveService({
    url: "https://rollercoin.com/api/events/config",
    intervalMs: 10,
    fetchImpl: async () => {
      probes += 1;
      return jsonResponse({ ok: true });
    }
  });

  service.start();
  await waitFor(() => probes >= 2);
  service.stop();
  const stoppedAt = probes;
  await new Promise((resolve) => setTimeout(resolve, 30));

  assert.equal(probes, stoppedAt);
});
