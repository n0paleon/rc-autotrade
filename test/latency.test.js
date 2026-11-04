import test from "node:test";
import assert from "node:assert/strict";
import { LatencyTracker } from "../src/worker/latency.js";

let tick = 0;
const clock = () => 1_700_000_000_000 + (tick += 10);

function tracker() {
  return new LatencyTracker({ capacity: 5, now: clock });
}

test("record keeps bounded rolling samples and computes stats", () => {
  const t = tracker();
  const values = [120, 80, 200, 95, 60, 400, 110];
  for (const value of values) t.record("http", value);

  const stats = t.stats().http;
  assert.equal(stats.count, 5, "oldest samples beyond capacity are dropped");
  assert.deepEqual(stats.recent, [200, 95, 60, 400, 110]);
  assert.equal(stats.last, 110);
  assert.equal(stats.min, 60);
  assert.equal(stats.max, 400);
  assert.equal(stats.errors, 0);
  assert.ok(stats.avg > 0);
  assert.ok(stats.p95 >= stats.p50);
  assert.ok(stats.jitter >= 0);
  assert.equal(stats.lastAt, clock() - 10);
});

test("unknown channels and non-finite values are rejected", () => {
  const t = tracker();
  assert.equal(t.record("nope", 100), false);
  assert.equal(t.record("http", Number.NaN), false);
  assert.equal(t.record("http", -5), false);
  assert.equal(t.record("http", 42), true);
  assert.equal(t.stats().http.count, 1);
});

test("failures are counted with duration and error text", () => {
  const t = tracker();
  t.record("http", 850, { ok: false, error: "HTTP 500" });
  const stats = t.stats().http;
  assert.equal(stats.count, 1);
  assert.equal(stats.errors, 1);
  assert.equal(stats.lastError, "HTTP 500");
  assert.equal(stats.lastOk, false);
});

test("measure wraps async results and records ok/fail outcomes", async () => {
  const t = tracker();
  const ok = await t.measure("keepalive", async () => ({ ok: true }));
  assert.equal(ok.ok, true);

  const failed = await t.measure("keepalive", async () => ({ ok: false, error: "boom" }));
  assert.equal(failed.ok, false);

  await assert.rejects(() => t.measure("keepalive", async () => {
    throw new Error("crashed");
  }), /crashed/);

  const stats = t.stats().keepalive;
  assert.equal(stats.count, 3, "all three attempts are timed");
  assert.equal(stats.errors, 2);
});

test("measure okFn override treats raw fetches as success regardless of status", async () => {
  const t = tracker();
  await t.measure("http", async () => ({ status: 503 }), { okFn: () => true });
  assert.equal(t.stats().http.errors, 0);
  assert.equal(t.stats().http.lastOk, true);
});

test("reset clears everything including freshest", () => {
  const t = tracker();
  t.record("wsHandshake", 350);
  t.record("wsPing", 24);
  assert.equal(t.freshest().channel, "wsPing");
  t.reset();
  assert.equal(t.freshest(), null);
  assert.equal(t.stats().wsHandshake.count, 0);
  assert.equal(t.stats().wsPing.last, null);
});

test("freshest picks the channel with the newest sample", () => {
  let stamps = [100, 200, 300];
  const t = new LatencyTracker({ now: () => stamps[stamps.length - 1] });
  const original = t.now;
  let index = 0;
  t.now = () => stamps[Math.min(index, stamps.length - 1)];
  t.record("http", 10);
  index = 1;
  t.record("keepalive", 20);
  index = 2;
  t.record("http", 30);
  t.now = original;
  assert.equal(t.freshest().channel, "http");
  assert.equal(t.freshest().last, 30);
});
