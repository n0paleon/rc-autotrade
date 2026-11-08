import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  breakPriceGaps,
  bucketPrice,
  mergePriceSeries,
  PRICE_BUCKETS,
  PRICE_PERIODS,
  summarizePrice,
  timezoneOffsetMs
} from "../src/lib/price.js";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const PLUS_SEVEN = 7 * HOUR;

test("keeps the newest close in each local-clock bucket", () => {
  const now = Date.UTC(2026, 10, 7, 0, 0, 0); // 07:00 in +07:00
  const series = bucketPrice(
    [
      { t: now - 14 * MINUTE, p: 0.09 },
      { t: now - 12 * MINUTE, p: 0.1 },
      { t: now - 9 * MINUTE, p: 0.12 },
      { t: now - 3 * MINUTE, p: 0.13 }
    ],
    { days: 1, bucketMs: 5 * MINUTE, now, tzOffsetMs: PLUS_SEVEN }
  );

  assert.deepEqual(series, [
    { t: now - 15 * MINUTE, p: 0.1, l: 0.09, h: 0.1, rl: 0.09, rh: 0.1 },
    { t: now - 10 * MINUTE, p: 0.12, l: 0.12, h: 0.12, rl: 0.12, rh: 0.12 },
    { t: now - 5 * MINUTE, p: 0.13, l: 0.13, h: 0.13, rl: 0.13, rh: 0.13 }
  ]);
});

test("widens a bucket's held and raw bands over every minute it covers", () => {
  const now = Date.UTC(2026, 10, 7, 0, 0, 0);
  const series = bucketPrice(
    [
      { t: now - 14 * MINUTE, p: 0.10, l: 0.08, h: 0.11, rl: 0.05, rh: 0.12 },
      { t: now - 12 * MINUTE, p: 0.12, l: 0.09, h: 0.14, rl: 0.07, rh: 0.15 },
      { t: now - 9 * MINUTE, p: 0.13, l: 0.12, h: 0.13, rl: 0.12, rh: 0.13 },
      // The newest close wins inside its bucket, but it cannot narrow a band.
      { t: now - 8 * MINUTE, p: 0.13, l: 0.125, h: 0.131, rl: 0.121, rh: 0.134 }
    ],
    { days: 1, bucketMs: 5 * MINUTE, now, tzOffsetMs: PLUS_SEVEN }
  );

  assert.deepEqual(series, [
    { t: now - 15 * MINUTE, p: 0.12, l: 0.08, h: 0.14, rl: 0.05, rh: 0.15 },
    { t: now - 10 * MINUTE, p: 0.13, l: 0.12, h: 0.131, rl: 0.12, rh: 0.134 }
  ]);
});

test("aligns buckets to the viewer's clock, not UTC", () => {
  const now = Date.UTC(2026, 10, 7, 0, 20, 0); // 07:20 in +07:00
  const series = bucketPrice([{ t: now, p: 0.5 }], {
    days: 1,
    bucketMs: HOUR,
    now,
    tzOffsetMs: PLUS_SEVEN
  });
  assert.equal(series.length, 1);
  assert.equal(series[0].t, Date.UTC(2026, 10, 7, 0, 0, 0));
});

test("daily buckets land on local midnight", () => {
  const now = Date.UTC(2026, 10, 6, 18, 0, 0); // 01:00 on the 7th in +07:00
  const series = bucketPrice(
    [
      { t: now - 2 * HOUR, p: 0.2 },
      { t: now, p: 0.3 }
    ],
    { days: 3, bucketMs: DAY, now, tzOffsetMs: PLUS_SEVEN }
  );
  assert.deepEqual(series, [
    { t: Date.UTC(2026, 10, 5, 17, 0, 0), p: 0.2, l: 0.2, h: 0.2, rl: 0.2, rh: 0.2 },
    { t: Date.UTC(2026, 10, 6, 17, 0, 0), p: 0.3, l: 0.3, h: 0.3, rl: 0.3, rh: 0.3 }
  ]);
});

test("drops points outside the requested window and invalid rows", () => {
  const now = Date.UTC(2026, 10, 7, 5, 0, 0);
  const series = bucketPrice(
    [
      { t: now - 9 * DAY, p: 99 },
      { t: now - 30 * MINUTE, p: 0.4 },
      { t: "nope", p: 1 },
      { t: now, p: "nope" }
    ],
    { days: 1, bucketMs: 5 * MINUTE, now, tzOffsetMs: 0 }
  );
  assert.deepEqual(series, [
    { t: now - 30 * MINUTE, p: 0.4, l: 0.4, h: 0.4, rl: 0.4, rh: 0.4 }
  ]);
});

test("mergePriceSeries lets the live window win on the same minute", () => {
  const at = Date.UTC(2026, 10, 7, 5, 0, 0);
  const merged = mergePriceSeries(
    [
      { t: at - MINUTE, p: 0.1, l: 0.09, h: 0.11 },
      { t: at, p: 0.2, l: 0.18, h: 0.21 }
    ],
    [{ t: at, p: 0.25 }]
  );
  assert.deepEqual(merged, [
    { t: at - MINUTE, p: 0.1, l: 0.09, h: 0.11, rl: 0.09, rh: 0.11 },
    // The live close wins; its missing bands keep the stored ones and widen
    // them to include the new close.
    { t: at, p: 0.25, l: 0.18, h: 0.25, rl: 0.18, rh: 0.25 }
  ]);
});

test("mergePriceSeries trusts an explicit live band", () => {
  const at = Date.UTC(2026, 10, 7, 5, 0, 0);
  const merged = mergePriceSeries(
    [{ t: at, p: 0.2, l: 0.18, h: 0.21, rl: 0.15, rh: 0.23 }],
    [{ t: at, p: 0.2, l: 0.19, h: 0.22, rl: 0.17, rh: 0.24 }]
  );
  assert.deepEqual(merged, [
    { t: at, p: 0.2, l: 0.19, h: 0.22, rl: 0.17, rh: 0.24 }
  ]);
});

test("summarizePrice reports first, last, change, high and low", () => {
  const summary = summarizePrice([
    { t: 1, p: 0.2 },
    { t: 2, p: 0.4 },
    { t: 3, p: 0.3 }
  ]);
  assert.equal(summary.first.p, 0.2);
  assert.equal(summary.last.p, 0.3);
  assert.ok(Math.abs(summary.change - 50) < 1e-9);
  assert.equal(summary.high, 0.4);
  assert.equal(summary.low, 0.2);
  assert.deepEqual(summarizePrice([]), {
    first: null,
    last: null,
    change: null,
    high: null,
    low: null
  });
});

test("summarizePrice includes intra-bucket wicks in its high and low", () => {
  const summary = summarizePrice([
    { t: 1, p: 0.2, l: 0.18, h: 0.21 },
    { t: 2, p: 0.2, l: 0.15, h: 0.24 },
    { t: 3, p: 0.21, l: 0.2, h: 0.22 }
  ]);
  assert.equal(summary.high, 0.24);
  assert.equal(summary.low, 0.15);
  // The change badge still uses closes, not the band.
  assert.ok(Math.abs(summary.change - 5) < 1e-9);
});

test("breakPriceGaps inserts a null break across a quiet stretch", () => {
  const series = breakPriceGaps(
    [
      { t: 0, p: 0.1 },
      { t: MINUTE, p: 0.2 },
      { t: 5 * MINUTE, p: 0.3 }
    ],
    MINUTE
  );

  assert.deepEqual(series, [
    { t: 0, p: 0.1, l: 0.1, h: 0.1, rl: 0.1, rh: 0.1 },
    { t: MINUTE, p: 0.2, l: 0.2, h: 0.2, rl: 0.2, rh: 0.2 },
    { t: 2 * MINUTE, p: null, l: null, h: null, rl: null, rh: null },
    { t: 5 * MINUTE, p: 0.3, l: 0.3, h: 0.3, rl: 0.3, rh: 0.3 }
  ]);
});

test("breakPriceGaps skips invalid rows and still breaks the gap", () => {
  const series = breakPriceGaps(
    [
      { t: 0, p: 0.1 },
      { t: MINUTE, p: Number.NaN },
      { t: 2 * MINUTE, p: 0.2 }
    ],
    MINUTE
  );

  assert.deepEqual(series, [
    { t: 0, p: 0.1, l: 0.1, h: 0.1, rl: 0.1, rh: 0.1 },
    { t: MINUTE, p: null, l: null, h: null, rl: null, rh: null },
    { t: 2 * MINUTE, p: 0.2, l: 0.2, h: 0.2, rl: 0.2, rh: 0.2 }
  ]);
});

test("exposes the requested period and bucket options", () => {
  assert.deepEqual(
    PRICE_PERIODS.map((period) => period.key),
    ["1", "3", "7"]
  );
  assert.deepEqual(Object.keys(PRICE_BUCKETS), ["1m", "5m", "30m", "1h", "4h", "1d"]);
  assert.equal(PRICE_BUCKETS["1d"].ms, DAY);
  assert.equal(PRICE_BUCKETS["4h"].ms, 4 * HOUR);
});

test("timezoneOffsetMs is east-positive: +07:00 becomes 25 200 000 ms", () => {
  assert.equal(typeof timezoneOffsetMs(Date.now()), "number");
  assert.equal(PLUS_SEVEN, 25_200_000);
});
