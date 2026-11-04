import { test } from "bun:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { WebSocketServer } from "ws";
import { RollercoinGateway } from "../src/worker/gateway.js";

function waitFor(predicate, timeoutMs = 3_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const check = () => {
      if (predicate()) return resolve();
      if (Date.now() >= deadline) return reject(new Error("condition not met before timeout"));
      setTimeout(check, 10);
    };
    check();
  });
}

test("gateway uses Bun WebSocket, forwards frames and sends the browser headers", async () => {
  const headers = {};
  const server = createServer((req, res) => res.end("ok"));
  const wss = new WebSocketServer({ noServer: true });

  server.on("upgrade", (req, socket, head) => {
    headers.origin = req.headers.origin;
    headers.cookie = req.headers.cookie;
    headers.agent = req.headers["user-agent"];
    headers.token = new URL(req.url, "http://localhost").searchParams.get("token");
    wss.handleUpgrade(req, socket, head, (ws) => wss.emit("connection", ws));
  });

  wss.on("connection", (ws) => {
    ws.send(JSON.stringify({ cmd: "marketplace_orders_update", value: { item_id: "item-1" } }));
    ws.send(JSON.stringify({ cmd: "marketplace_orders_filled", value: { order_id: "order-1" } }));
  });

  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const gateway = new RollercoinGateway({
    url: `ws://127.0.0.1:${server.address().port}/`,
    getToken: () => "token-123",
    getCookie: () => "cf_clearance=test",
    logger: { info() {}, warn() {}, error() {}, debug() {} }
  });
  const frames = [];
  gateway.on("frame", (frame) => frames.push(frame));

  try {
    gateway.start();
    await waitFor(() => frames.length >= 2);

    assert.deepEqual(
      frames.map((frame) => frame.cmd),
      ["marketplace_orders_update", "marketplace_orders_filled"]
    );
    assert.equal(gateway.stats().status, "online");
    assert.equal(gateway.stats().eventCount, 2);
    assert.equal(headers.origin, "https://rollercoin.com");
    assert.equal(headers.cookie, "cf_clearance=test");
    assert.equal(headers.token, "token-123");
    assert.match(headers.agent, /Mozilla\/5\.0/);
  } finally {
    gateway.stop();
    wss.close();
    await new Promise((resolve) => server.close(resolve));
  }
});
