import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  jitterBand,
  latencyDomain,
  orderAxisTick,
  orderBucket,
  orderHistoryPath,
  orderPeriod,
  orderRangeLabel,
  orderStage,
  orderTimeTicks
} from "../src/lib/order-chart.js";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;

test("falls back to the 24h/1h/end-to-end view", () => {
  assert.equal(orderPeriod("nope").key, "24h");
  assert.equal(orderBucket("nope").key, "1h");
  assert.equal(orderStage("nope").key, "total");
  assert.equal(orderPeriod("7d").bucket, "6h");
});

test("builds the history query with every control", () => {
  const path = orderHistoryPath({
    hours: 24,
    bucket: "1h",
    stage: "ttfb",
    tzOffsetMs: 25_200_000
  });
  assert.equal(
    path,
    "/api/benchmark/history?hours=24&bucket=1h&stage=ttfb&tzOffsetMs=25200000"
  );
});

test("latency domain pads around the data and never goes negative", () => {
  assert.deepEqual(latencyDomain([]), [0, 1000]);
  assert.deepEqual(latencyDomain([100, 200]), [88, 212]);
  assert.deepEqual(latencyDomain([1000]), [988, 1012]);
  assert.deepEqual(latencyDomain([0, 10]), [0, 11.2]);
});

test("the jitter band clamps its lower edge at zero", () => {
  assert.deepEqual(jitterBand({ avg: 100, jitter: 20 }), [80, 120]);
  assert.deepEqual(jitterBand({ avg: 10, jitter: 20 }), [0, 30]);
  assert.equal(jitterBand({ avg: null, jitter: 20 }), null);
});

test("bucket labels read as local ranges", () => {
  const start = new Date(2026, 10, 7, 10, 0, 0).getTime();
  assert.equal(orderRangeLabel(start, 30 * MINUTE), "10:00–10:30");

  const late = new Date(2026, 10, 7, 23, 45, 0).getTime();
  assert.equal(orderRangeLabel(late, 30 * MINUTE), "07 Nov 23:45–08 Nov 00:15");
});

test("axis labels switch from clock to date by span", () => {
  const value = new Date(2026, 10, 7, 10, 30, 0).getTime();
  assert.equal(orderAxisTick(value, 24 * HOUR, 5 * MINUTE), "10:30");
  assert.equal(orderAxisTick(value, 3 * 24 * HOUR, HOUR), "7/11 10:30");
  assert.equal(orderAxisTick(value, 7 * 24 * HOUR, 24 * HOUR), "7/11");
});

test("time ticks stay between three and six labels", () => {
  const start = new Date(2026, 10, 7, 0, 0, 0).getTime();
  const end = start + 24 * HOUR;
  const offset = -new Date(start).getTimezoneOffset() * MINUTE;
  const ticks = orderTimeTicks(start, end, offset);
  assert.ok(Array.isArray(ticks));
  assert.ok(ticks.length >= 2 && ticks.length <= 6, `got ${ticks.length} ticks`);
  assert.ok(ticks[0] >= start && ticks[ticks.length - 1] <= end);
});
