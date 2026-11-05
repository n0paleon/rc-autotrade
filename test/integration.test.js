import { test, beforeAll, afterAll, setDefaultTimeout } from "bun:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { connect, createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import WebSocket from "ws";

const ROOT = resolve(import.meta.dirname, "..");
const TOKEN = "integration-test-access-key";
const TIMEOUT = 30_000;
setDefaultTimeout(TIMEOUT);

let child;
let baseUrl;
let dataDir;
let output = "";

async function freePort() {
  const server = createServer();
  await new Promise((resolveListen, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolveListen);
  });
  const port = server.address().port;
  await new Promise((resolveClose) => server.close(resolveClose));
  return port;
}

/**
 * Raw WebSocket handshake. Bun's ws client does not implement
 * `unexpected-response` and its `protocol` getter is unreliable, so the
 * negotiation and the rejected-key status are asserted from the HTTP reply.
 */
function websocketHandshake(protocols) {
  const port = new URL(baseUrl).port;
  return new Promise((resolve, reject) => {
    const socket = connect(Number(port), "127.0.0.1", () => {
      socket.write(
        `GET /ws HTTP/1.1\r\n` +
          `Host: 127.0.0.1:${port}\r\n` +
          `Upgrade: websocket\r\n` +
          `Connection: Upgrade\r\n` +
          `Sec-WebSocket-Key: ${Buffer.from("0123456789abcdef").toString("base64")}\r\n` +
          `Sec-WebSocket-Version: 13\r\n` +
          `Sec-WebSocket-Protocol: ${protocols.join(", ")}\r\n\r\n`
      );
    });
    let data = "";
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error("websocket handshake timed out"));
    }, 3000);
    socket.on("data", (chunk) => {
      data += chunk.toString("latin1");
      if (!data.includes("\r\n\r\n")) return;
      clearTimeout(timer);
      socket.destroy();
      const status = Number(/^HTTP\/1\.1 (\d{3})/.exec(data)?.[1] || 0);
      const protocol = /sec-websocket-protocol:\s*([^\r\n]+)/i.exec(data)?.[1]?.trim() || null;
      resolve({ status, protocol });
    });
    socket.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

async function waitForHealth(url, deadline) {
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${url}/api/health`);
      if (response.ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((resolveSleep) => setTimeout(resolveSleep, 120));
  }
  throw new Error(`worker did not start\n${output}`);
}

function authHeaders(extra = {}) {
  return { authorization: `Bearer ${TOKEN}`, ...extra };
}

beforeAll(async () => {
  dataDir = mkdtempSync(join(tmpdir(), "turuc-int-"));
  const port = await freePort();
  const env = {
    ...process.env,
    HOST: "127.0.0.1",
    PORT: String(port),
    DATA_DIR: dataDir,
    MOCK: "1",
    MOCK_INTERVAL_MS: "500",
    WORKER_TOKEN: TOKEN,
    LOG_LEVEL: "warn"
  };
  child = spawn(process.execPath, ["src/worker/index.js"], { cwd: ROOT, env, stdio: ["ignore", "pipe", "pipe"] });
  child.stdout.on("data", (chunk) => {
    output += chunk.toString();
  });
  child.stderr.on("data", (chunk) => {
    output += chunk.toString();
  });
  baseUrl = `http://127.0.0.1:${port}`;
  await waitForHealth(baseUrl, Date.now() + 12_000);
});

afterAll(async () => {
  if (child) {
    child.kill("SIGTERM");
    await Promise.race([
      new Promise((resolveExit) => child.once("exit", resolveExit)),
      new Promise((resolveTimer) => setTimeout(resolveTimer, 3000))
    ]);
    if (child.exitCode === null) child.kill("SIGKILL");
  }
  if (dataDir) rmSync(dataDir, { recursive: true, force: true });
});

test("health and dashboard are available without a key", async () => {
  const health = await fetch(`${baseUrl}/api/health`);
  assert.equal(health.status, 200);
  const body = await health.json();
  assert.equal(body.ok, true);
  assert.equal(body.mock, true);

  const page = await fetch(`${baseUrl}/`);
  assert.equal(page.status, 200);
  const html = await page.text();
  assert.match(html, /TURU\.RC/);
  assert.match(html, /id="app"/);
  assert.ok(page.headers.get("content-security-policy"));
});

test("API requires the access key", async () => {
  const response = await fetch(`${baseUrl}/api/state`);
  assert.equal(response.status, 401);
  const withKey = await fetch(`${baseUrl}/api/state`, { headers: authHeaders() });
  assert.equal(withKey.status, 200);
});

test("item-info endpoint resolves an automatic label", async () => {
  const response = await fetch(
    `${baseUrl}/api/item-info?itemId=6571aa39ca8bfef34adcd9e9&itemType=mutation_component`,
    { headers: authHeaders() }
  );
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.equal(body.ok, true);
  assert.equal(body.item.label, "Rare Neon Shard");
  assert.equal(body.item.rarity, "Rare");
  assert.equal(body.item.rarityColor, "b8ff3c");
});

test("mock engine fills a seeded target", async () => {
  const deadline = Date.now() + 20_000;
  let snapshot;
  while (Date.now() < deadline) {
    const response = await fetch(`${baseUrl}/api/state`, { headers: authHeaders() });
    snapshot = await response.json();
    if (snapshot.stats?.filled > 0) break;
    await new Promise((resolveSleep) => setTimeout(resolveSleep, 300));
  }
  assert.equal(snapshot.worker?.mock, true, "worker must run in mock mode");
  assert.equal(snapshot.settings?.paused, false, "trading must not be paused");
  assert.equal(snapshot.settings?.dryRun, false, "dry run must be off");
  assert.ok(snapshot.stats.filled > 0, `expected at least one simulated fill\n${output}`);
  assert.ok(snapshot.history.length > 0);
  const ranged = snapshot.configs.find(
    (config) =>
      Number.isFinite(config.runtime?.allowedPriceMin) &&
      Number.isFinite(config.runtime?.allowedPriceMax)
  );
  assert.ok(ranged, `expected the market frame to cache an allowed price band\n${output}`);
  assert.ok(ranged.runtime.allowedPriceMax > ranged.runtime.allowedPriceMin);
  assert.equal(snapshot.startedAt > 0, true);

  assert.ok(snapshot.report?.items?.length > 0, "snapshot must include the per-product report");
  const reportResponse = await fetch(`${baseUrl}/api/report`, { headers: authHeaders() });
  assert.equal(reportResponse.status, 200);
  const reportBody = await reportResponse.json();
  assert.equal(reportBody.ok, true);
  assert.ok(reportBody.report.items.length > 0);
  assert.ok(reportBody.report.totals.fills > 0);
  assert.ok(reportBody.report.items.some((item) => item.fills > 0 && item.avgBuy > 0));

  const historyResponse = await fetch(`${baseUrl}/api/history?limit=5`, { headers: authHeaders() });
  assert.equal(historyResponse.status, 200);
  const historyBody = await historyResponse.json();
  assert.equal(historyBody.ok, true);
  assert.ok(historyBody.total >= 1, "the permanent log counts stored orders");
  assert.ok(historyBody.entries.length > 0, "the first page has entries");
  assert.ok(historyBody.entries.length <= 5, "the page honours the limit");
  assert.ok(historyBody.total >= snapshot.historyTotal);

  const label = historyBody.entries[0].label;
  const searchResponse = await fetch(
    `${baseUrl}/api/history?q=${encodeURIComponent(label.toLowerCase())}`,
    { headers: authHeaders() }
  );
  assert.equal(searchResponse.status, 200);
  const searchBody = await searchResponse.json();
  assert.ok(searchBody.total >= 1, "searching a stored product finds it");
  assert.ok(
    searchBody.entries.every((entry) => String(entry.label).toLowerCase().includes(label.toLowerCase())),
    "search only returns matching products"
  );
});

test("target CRUD validates and broadcasts snapshots", async () => {
  const itemId = "66ccddeeff001122334455";
  const created = await fetch(`${baseUrl}/api/configs`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({
      itemId: `https://rollercoin.com/marketplace/mutation-components/${itemId}?utm=x`,
      label: "INTEGRATION DRONE",
      targetPrice: 0.075,
      desiredQty: 12,
      highStockThreshold: 30,
      minAcceptableQty: 3,
      autoTarget: true,
      autoTargetMarginPct: 3.5
    })
  });
  assert.equal(created.status, 201);
  const createdBody = await created.json();
  const createdTarget = createdBody.configs.find((config) => config.itemId === itemId);
  assert.equal(createdTarget.autoTarget, true);
  assert.equal(createdTarget.autoTargetMarginPct, 3.5);

  const patched = await fetch(`${baseUrl}/api/configs/${itemId}`, {
    method: "PATCH",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ active: false })
  });
  assert.equal(patched.status, 200);
  const patchedBody = await patched.json();
  const patchedTarget = patchedBody.configs.find((config) => config.itemId === itemId);
  assert.equal(patchedTarget.active, false);
  assert.equal(patchedTarget.autoTarget, true, "a partial PATCH keeps the auto target switch");
  assert.equal(patchedTarget.autoTargetMarginPct, 3.5);
  assert.equal(patchedTarget.targetPrice, 0.075, "the API never returns a runtime-adjusted base");

  const invalid = await fetch(`${baseUrl}/api/configs`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ itemId, targetPrice: 0, desiredQty: 0, highStockThreshold: 0, minAcceptableQty: 0 })
  });
  assert.equal(invalid.status, 422);

  const removed = await fetch(`${baseUrl}/api/configs/${itemId}`, {
    method: "DELETE",
    headers: authHeaders()
  });
  assert.equal(removed.status, 200);
  const removedBody = await removed.json();
  assert.equal(removedBody.configs.some((config) => config.itemId === itemId), false);
});

test("settings, pause and history reset work", async () => {
  const settings = await fetch(`${baseUrl}/api/settings`, {
    method: "PATCH",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ delayPerOrder: 420, maxOrdersPerMinute: 20, dryRun: true })
  });
  assert.equal(settings.status, 200);
  assert.equal((await settings.json()).settings.delayPerOrder, 420);

  const paused = await fetch(`${baseUrl}/api/control`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ action: "pause" })
  });
  assert.equal(paused.status, 200);
  assert.equal((await paused.json()).settings.paused, true);

  const resumed = await fetch(`${baseUrl}/api/control`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ action: "resume" })
  });
  assert.equal((await resumed.json()).settings.paused, false);

  const toggled = await fetch(`${baseUrl}/api/control`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ action: "toggle" })
  });
  assert.equal((await toggled.json()).settings.paused, true);

  const toggledBack = await fetch(`${baseUrl}/api/control`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json" }),
    body: JSON.stringify({ action: "toggle" })
  });
  assert.equal((await toggledBack.json()).settings.paused, false);

  const reset = await fetch(`${baseUrl}/api/history`, { method: "DELETE", headers: authHeaders() });
  assert.equal(reset.status, 200);
  assert.equal((await reset.json()).stats.filled, 0);

  const clearedReport = await fetch(`${baseUrl}/api/report`, { headers: authHeaders() });
  assert.equal(clearedReport.status, 200);
  const clearedBody = await clearedReport.json();
  assert.deepEqual(clearedBody.report.items, []);
  assert.equal(clearedBody.report.totals.fills, 0);
  assert.equal(clearedBody.report.totals.profitRlt, 0);
});

test("bridge CORS headers survive preflight and auth errors", async () => {
  const preflight = await fetch(`${baseUrl}/api/session/push`, {
    method: "OPTIONS",
    headers: {
      origin: "https://rollercoin.com",
      "access-control-request-method": "POST",
      "access-control-request-headers": "authorization,content-type,cache-control",
      "access-control-request-private-network": "true"
    }
  });
  assert.equal(preflight.status, 204);
  assert.match(preflight.headers.get("access-control-allow-headers"), /cache-control/);
  assert.equal(preflight.headers.get("access-control-allow-private-network"), "true");

  const unauthorized = await fetch(`${baseUrl}/api/session/push`, {
    method: "POST",
    headers: {
      origin: "https://rollercoin.com",
      "content-type": "application/json",
      authorization: "Bearer definitely-wrong-key"
    },
    body: JSON.stringify({ accessToken: "opaque-token-123456" })
  });
  assert.equal(unauthorized.status, 401);
  assert.equal(unauthorized.headers.get("access-control-allow-origin"), "*");

  const invalid = await fetch(`${baseUrl}/api/session/push`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json", origin: "https://rollercoin.com" }),
    body: JSON.stringify({ accessToken: "x" })
  });
  assert.equal(invalid.status, 422);
  assert.equal(invalid.headers.get("access-control-allow-origin"), "*");
});

test("session push accepts a newer JWT and CORS is enabled", async () => {
  const header = Buffer.from(JSON.stringify({ alg: "none", typ: "JWT" })).toString("base64url");
  const payload = Buffer.from(
    JSON.stringify({ sub: "bridge-test", exp: Math.floor(Date.now() / 1000) + 7200 })
  ).toString("base64url");
  const token = `${header}.${payload}.signature`;

  const preflight = await fetch(`${baseUrl}/api/session/push`, {
    method: "OPTIONS",
    headers: {
      origin: "https://rollercoin.com",
      "access-control-request-method": "POST",
      "access-control-request-headers": "authorization,content-type"
    }
  });
  assert.equal(preflight.status, 204);
  assert.equal(preflight.headers.get("access-control-allow-origin"), "*");

  const response = await fetch(`${baseUrl}/api/session/push`, {
    method: "POST",
    headers: authHeaders({ "content-type": "application/json", origin: "https://rollercoin.com" }),
    body: JSON.stringify({ accessToken: token, refreshToken: "bridge-refresh-token-123" })
  });
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.equal(body.accepted, true);
  assert.equal(response.headers.get("access-control-allow-origin"), "*");

  const state = await (await fetch(`${baseUrl}/api/state`, { headers: authHeaders() })).json();
  assert.equal(state.session.source, "bridge");
});

test("session can be cleared from the control deck", async () => {
  const cleared = await fetch(`${baseUrl}/api/session`, {
    method: "DELETE",
    headers: authHeaders()
  });
  assert.equal(cleared.status, 200);
  const body = await cleared.json();
  assert.equal(body.ok, true);
  assert.equal(body.session.hasAccessToken, false);
  assert.equal(body.session.hasRefreshToken, false);
  assert.equal(body.session.hasCookie, false);

  const state = await (await fetch(`${baseUrl}/api/state`, { headers: authHeaders() })).json();
  assert.equal(state.session.hasAccessToken, false);
  assert.equal(state.session.hasRefreshToken, false);
});

test("WebSocket accepts the subprotocol key and answers ping", async () => {
  const socket = new WebSocket(`ws://127.0.0.1:${new URL(baseUrl).port}/ws`, ["rcw.v1", TOKEN]);
  const seen = new Set();

  await new Promise((resolveReady, reject) => {
    const timer = setTimeout(() => reject(new Error("websocket timeout")), 5000);
    socket.on("message", (data) => {
      const message = JSON.parse(data.toString());
      seen.add(message.type);
      if (message.type === "snapshot") {
        socket.send(JSON.stringify({ type: "ping", t: Date.now() }));
      }
      if (message.type === "pong") {
        clearTimeout(timer);
        resolveReady();
      }
    });
    socket.on("error", reject);
  });

  // Reaching hello/snapshot proves the server accepted the rcw.v1 subprotocol
  // and the key. Bun's ws client corrupts `socket.protocol` after the
  // handshake, so the negotiation is asserted through the connection itself.
  assert.equal(seen.has("hello"), true);
  assert.equal(seen.has("snapshot"), true);
  socket.close();

  const accepted = await websocketHandshake(["rcw.v1", TOKEN]);
  assert.equal(accepted.status, 101);
  assert.equal(accepted.protocol, "rcw.v1");

  // Bun cannot send an HTTP 403 on an upgrade socket, so a rejected key is
  // accepted at the HTTP layer and closed with 4001 by the server.
  const bad = new WebSocket(`ws://127.0.0.1:${new URL(baseUrl).port}/ws`, ["rcw.v1", "wrong-key"]);
  const closeCode = await new Promise((resolveClose) => {
    bad.on("close", (code) => resolveClose(code));
    bad.on("error", () => resolveClose(0));
    setTimeout(() => resolveClose(-1), 2500);
  });
  assert.equal(closeCode, 4001);
});

test("system monitor exposes live worker and host metrics", async () => {
  // CPU percent needs two samples, and the monitor samples every 2s, so give
  // it a window before asking for the current view.
  await new Promise((resolveSleep) => setTimeout(resolveSleep, 2500));

  const response = await fetch(`${baseUrl}/api/system`, { headers: authHeaders() });
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.equal(body.ok, true);

  const system = body.system;
  assert.ok(system, "system payload must be present");
  assert.ok(system.sampledAt > 0);
  assert.ok(system.intervalMs >= 500);
  assert.equal(system.process.pid > 0, true);
  assert.ok(system.process.uptimeMs > 0);
  assert.ok(system.memory.rss > 0, "the worker must report RSS");
  assert.ok(system.memory.heapLimit > 0, "the worker must report the V8 heap ceiling");
  assert.ok(system.memory.heapPercent > 0 && system.memory.heapPercent <= 100);
  assert.ok(system.system.total > 0, "host memory must be reported");
  assert.ok(system.system.available >= 0);
  assert.ok(Array.isArray(system.history) && system.history.length >= 1);
  assert.ok(["ok", "warn", "critical"].includes(system.health.level));
  assert.deepEqual(Object.keys(system.eventLoop).sort(), ["lag", "utilization"]);

  assert.ok(system.db, "DB view must be present");
  assert.equal(system.db.instrumented, true);
  assert.equal(system.db.sqlite.journalMode, "wal");
  assert.ok(system.db.storage.pageSize > 0);
  assert.ok(system.db.storage.dataBytes > 0);
  assert.ok(system.db.files.db > 0, "the SQLite file must be measured");
  assert.equal(system.db.queries.statements >= 0, true);
  assert.equal(typeof system.db.window.p95Ms, "number");
  assert.ok(Array.isArray(system.db.recentSlow));
  assert.ok(Array.isArray(system.db.history));

  const state = await (await fetch(`${baseUrl}/api/state`, { headers: authHeaders() })).json();
  assert.ok(state.system, "snapshot must embed the system view");
  assert.ok(state.system.memory.rss > 0);
});

test("key rotation is refused when WORKER_TOKEN pins the key", async () => {
  const response = await fetch(`${baseUrl}/api/auth/rotate`, { method: "POST", headers: authHeaders() });
  assert.equal(response.status, 409);
});
