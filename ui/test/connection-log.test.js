import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  CONNECTION_LOG_PAGE_SIZE,
  connectionEventMeta,
  connectionLogPage,
  connectionLogRows,
  formatClock,
  formatSessionDuration
} from "../src/lib/connection-log.js";

const LOG = [
  { at: 3_000, kind: "reconnect", sessionMs: null, reason: "handshake 51ms" },
  { at: 2_000, kind: "disconnect", sessionMs: 65_200, reason: "socket closed (1006)" },
  { at: 1_000, kind: "connect", sessionMs: null, reason: "handshake 42ms" }
];

test("every disconnect row keeps its session duration", () => {
  const rows = connectionLogRows(LOG);
  assert.equal(rows.length, 3);
  assert.deepEqual(
    rows.map((row) => row.kind),
    ["reconnect", "disconnect", "connect"],
    "newest transition stays on top"
  );

  const disconnect = rows.find((row) => row.kind === "disconnect");
  assert.equal(disconnect.sessionMs, 65_200);
  assert.equal(disconnect.sessionLabel, "00:01:05");
  assert.equal(disconnect.detail, "socket closed (1006)");

  assert.equal(rows[0].sessionLabel, "—");
  assert.equal(rows[2].sessionLabel, "—");
});

test("negative and missing session durations never render as negative time", () => {
  const rows = connectionLogRows([
    { at: 5, kind: "disconnect", sessionMs: -50, reason: null },
    { at: 4, kind: "disconnect", sessionMs: undefined, reason: "" }
  ]);
  assert.equal(rows[0].sessionMs, 0);
  assert.equal(rows[0].sessionLabel, "00:00:00");
  assert.equal(rows[0].detail, "—", "an empty reason falls back to a dash");
  assert.equal(rows[1].sessionMs, null);
  assert.equal(rows[1].sessionLabel, "—");
});

test("malformed and out-of-order rows are normalized, not dropped", () => {
  const rows = connectionLogRows([
    { at: 1, kind: "disconnect", sessionMs: 1_000 },
    { at: 9, kind: "mystery" },
    null,
    { at: 5, kind: "connect" }
  ]);
  assert.deepEqual(
    rows.map((row) => row.at),
    [9, 5, 1, null]
  );
  assert.equal(rows[0].label, "Unknown");
  assert.equal(rows[0].detail, "—");
  assert.equal(rows[2].sessionMs, 1_000);
});

test("non-array input yields an empty table", () => {
  assert.deepEqual(connectionLogRows(null), []);
  assert.deepEqual(connectionLogRows(undefined), []);
  assert.deepEqual(connectionLogRows({}), []);
});

test("pagination keeps the table at a fixed size and reports its range", () => {
  const rows = Array.from({ length: 25 }, (_, index) => ({ id: index }));
  const first = connectionLogPage(rows, 0);
  assert.equal(first.rows.length, CONNECTION_LOG_PAGE_SIZE);
  assert.equal(first.total, 25);
  assert.equal(first.pageCount, 3);
  assert.deepEqual([first.page, first.from, first.to], [0, 1, 10]);

  const second = connectionLogPage(rows, 1);
  assert.deepEqual([second.page, second.from, second.to], [1, 11, 20]);
  assert.equal(second.rows[0].id, 10);

  const last = connectionLogPage(rows, 2);
  assert.deepEqual([last.page, last.from, last.to], [2, 21, 25]);
  assert.equal(last.rows.length, 5);
});

test("pagination clamps stale pages and empty input", () => {
  const rows = Array.from({ length: 12 }, (_, index) => ({ id: index }));
  assert.equal(connectionLogPage(rows, 99).page, 1, "page past the end falls back");
  assert.equal(connectionLogPage(rows, -3).page, 0);
  assert.equal(connectionLogPage(rows, NaN).page, 0);

  const empty = connectionLogPage([], 5);
  assert.deepEqual(empty.rows, []);
  assert.deepEqual([empty.page, empty.pageCount, empty.total, empty.from, empty.to], [0, 1, 0, 0, 0]);

  const custom = connectionLogPage(rows, 1, 5);
  assert.deepEqual([custom.page, custom.from, custom.to, custom.rows.length], [1, 6, 10, 5]);
});

test("event metadata and formatting helpers", () => {
  assert.equal(connectionEventMeta("connect").chip, "chip-online");
  assert.equal(connectionEventMeta("reconnect").chip, "chip-dry");
  assert.equal(connectionEventMeta("disconnect").tone, "text-danger");
  assert.equal(connectionEventMeta("nope").label, "Unknown");

  assert.equal(formatSessionDuration(0), "00:00:00");
  assert.equal(formatSessionDuration("1500"), "00:00:01");
  assert.equal(formatSessionDuration(NaN), "—");
  assert.equal(formatClock(null), "—");
});
