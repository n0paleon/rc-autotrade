import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  bucketEvents,
  EVENT_PERIODS,
  EVENT_TIMEFRAMES,
  summarizeEvents,
  timezoneOffsetMs
} from "../src/lib/events.js";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const PLUS_SEVEN = 7 * HOUR;

test("buckets 5-minute points into local-clock 15-minute bars", () => {
  const now = Date.UTC(2026, 10, 7, 0, 0, 0); // 07:00 in +07:00
  const first = now - 30 * MINUTE; // 06:30 local
  const series = bucketEvents(
    [
      { t: first, n: 2 },
      { t: first + 5 * MINUTE, n: 3 },
      { t: first + 10 * MINUTE, n: 4 }
    ],
    { days: 1, bucketMs: 15 * MINUTE, now, tzOffsetMs: PLUS_SEVEN }
  );

  assert.equal(series.length, 97); // one day of bars, both ends inclusive
  const bar = series[series.length - 3]; // 06:30 local, two bars before the live 07:00 one
  assert.equal(bar.t, first);
  assert.equal(bar.n, 9);
  assert.equal(series[series.length - 1].t, now);
  assert.equal(
    series.reduce((sum, point) => sum + point.n, 0),
    9
  );
});

test("fills quiet buckets with zero instead of dropping them", () => {
  const now = Date.UTC(2026, 10, 7, 5, 0, 0);
  const series = bucketEvents([{ t: now - 10 * MINUTE, n: 2 }], {
    days: 1,
    bucketMs: HOUR,
    now,
    tzOffsetMs: 0
  });

  assert.equal(series.length, 25); // both ends inclusive
  assert.equal(series[0].n, 0);
  // 04:50 lands in the 04:00 bar; the live 05:00 bar is still quiet.
  assert.equal(series[series.length - 2].n, 2);
  assert.equal(series[series.length - 1].n, 0);
  assert.equal(
    series.filter((point) => point.n === 0).length,
    24
  );
});

test("drops points outside the requested window and invalid rows", () => {
  const now = Date.UTC(2026, 10, 7, 5, 0, 0);
  const series = bucketEvents(
    [
      { t: now - 9 * DAY, n: 500 },
      { t: now - 30 * MINUTE, n: 3 },
      { t: "nope", n: 3 },
      { t: now, n: -1 }
    ],
    { days: 1, bucketMs: 5 * MINUTE, now, tzOffsetMs: 0 }
  );

  assert.equal(
    series.reduce((sum, point) => sum + point.n, 0),
    3
  );
});

test("returns an all-zero series when no history exists yet", () => {
  const now = Date.UTC(2026, 10, 7, 5, 0, 0);
  const series = bucketEvents(null, { days: 3, bucketMs: 30 * MINUTE, now, tzOffsetMs: 0 });
  assert.equal(series.length, 145);
  assert.ok(series.every((point) => point.n === 0));
});

test("summarizeEvents reports the total, the busiest bucket and the average", () => {
  const series = [
    { t: 1, n: 0 },
    { t: 2, n: 4 },
    { t: 3, n: 9 },
    { t: 4, n: 2 }
  ];
  const summary = summarizeEvents(series);
  assert.equal(summary.total, 15);
  assert.deepEqual(summary.peak, { t: 3, n: 9 });
  assert.equal(summary.average, 15 / 4);

  assert.deepEqual(summarizeEvents([]), { total: 0, peak: null, average: 0 });
});

test("exposes the requested period and timeframe options", () => {
  assert.deepEqual(
    EVENT_PERIODS.map((period) => period.label),
    ["1D", "3D", "7D"]
  );
  assert.deepEqual(Object.keys(EVENT_TIMEFRAMES), ["5m", "15m", "30m", "1h", "2h"]);
  assert.equal(EVENT_TIMEFRAMES["2h"].ms, 120 * MINUTE);
});

test("timezoneOffsetMs is east-positive: +07:00 becomes 25 200 000 ms", () => {
  assert.equal(typeof timezoneOffsetMs(Date.now()), "number");
  assert.equal(PLUS_SEVEN, 25_200_000);
});
