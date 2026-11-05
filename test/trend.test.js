import { test } from "bun:test";
import assert from "node:assert/strict";
import { buildTrend, classifyState, computeTrendModel, TREND } from "../src/shared/trend.js";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const NOW = Date.UTC(2026, 4, 5, 12, 0, 0);

/** Evenly spaced minute points ending exactly at `NOW`. */
function series(hours, priceAt, { stepMinutes = 1, end = NOW } = {}) {
  const points = [];
  const start = end - hours * HOUR;
  for (let t = start; t <= end; t += stepMinutes * MINUTE) {
    points.push({ t, p: priceAt(t) });
  }
  return points;
}

test("a flat market yields a stable trend and a below-trend target", () => {
  const history = series(6, () => 2.0);
  const signal = buildTrend(history, { now: NOW, targetPrice: 1.8 });

  assert.equal(signal.status, "ok");
  assert.ok(Math.abs(signal.trend - 2.0) < 1e-6);
  assert.ok(Math.abs(signal.proximityPct + 10) < 0.01);
  assert.equal(signal.state, "BELOW");
  assert.equal(signal.reliable, true);
  assert.equal(signal.direction, "flat");
  assert.equal(signal.outliers, 0);
});

test("a single cheap spike is filtered before it reaches the trend", () => {
  const history = series(6, (t) => (Math.abs(t - (NOW - 3 * HOUR)) < MINUTE / 2 ? 0.5 : 2.0));
  const model = computeTrendModel(history, { now: NOW });

  assert.ok(model.outliers >= 1, "the spike must be flagged");
  assert.ok(model.trend > 1.99, `trend stayed near 2.0, got ${model.trend}`);
  assert.ok(Math.abs(model.momentumPct) < 0.2, `momentum must stay small, got ${model.momentumPct}`);
});

test("a sustained decline reads as a downward trend and an above-trend target", () => {
  const history = series(6, (t) => 2.5 - (1.5 * (t - (NOW - 6 * HOUR))) / (6 * HOUR));
  const signal = buildTrend(history, { now: NOW, targetPrice: 2.0 });

  assert.equal(signal.status, "ok");
  assert.ok(signal.slopePctPerHour < 0, `slope should be negative, got ${signal.slopePctPerHour}`);
  assert.equal(signal.direction, "down");
  assert.equal(signal.state, "ABOVE");
  assert.ok(signal.confidence >= TREND.reliableConfidence);
});

test("hysteresis keeps a boundary target inside NEAR until it clearly leaves", () => {
  const history = series(6, () => 2.0);

  const fresh = buildTrend(history, { now: NOW, targetPrice: 2.05 });
  assert.equal(fresh.state, "ABOVE");

  const held = buildTrend(history, { now: NOW, targetPrice: 2.05, previousState: "NEAR" });
  assert.equal(held.state, "NEAR");

  const lowerHeld = buildTrend(history, { now: NOW, targetPrice: 1.95, previousState: "NEAR" });
  assert.equal(lowerHeld.state, "NEAR");

  const lowerFresh = buildTrend(history, { now: NOW, targetPrice: 1.95 });
  assert.equal(lowerFresh.state, "BELOW");
});

test("deep discounts and above-trend targets are classified apart", () => {
  assert.equal(classifyState(8), "ABOVE");
  assert.equal(classifyState(0), "NEAR");
  assert.equal(classifyState(-10), "BELOW");
  assert.equal(classifyState(-25), "FAR_BELOW");
});

test("too few samples produce NO_DATA instead of a guess", () => {
  const history = series(1, () => 2.0).slice(0, 4);
  const signal = buildTrend(history, { now: NOW, targetPrice: 1.9 });

  assert.equal(signal.status, "no_data");
  assert.equal(signal.state, "NO_DATA");
  assert.equal(signal.trend, null);
  assert.equal(signal.reliable, false);
});

test("a frozen feed is marked stale and cannot be reliable", () => {
  const history = series(6, () => 2.0, { end: NOW - 2 * HOUR });
  const signal = buildTrend(history, { now: NOW, targetPrice: 1.8 });

  assert.equal(signal.status, "stale");
  assert.equal(signal.state, "STALE");
  assert.equal(signal.stale, true);
  assert.equal(signal.reliable, false);
  assert.ok(signal.confidence <= 20);
});

test("confidence grows as the observation window grows", () => {
  const short = buildTrend(series(1, () => 2.0), { now: NOW, targetPrice: 1.9 });
  const long = buildTrend(series(8, () => 2.0), { now: NOW, targetPrice: 1.9 });

  assert.ok(long.confidence > short.confidence);
});

test("the depth series removes a thin-book false positive", () => {
  const thinTop = series(6, () => 2.913);
  const depth = series(6, () => 3.0385);

  const onTop = buildTrend(thinTop, { now: NOW, targetPrice: 3.0, source: "top" });
  const onDepth = buildTrend(depth, {
    now: NOW,
    targetPrice: 3.0,
    source: "depth",
    fill: { price: 3.0385, filled: 25, partial: false, approx: false }
  });

  assert.equal(onTop.state, "ABOVE");
  assert.equal(onDepth.state, "NEAR");
  assert.ok(onDepth.proximityPct < 0, "the depth series puts the target under the market");
  assert.equal(onDepth.source, "depth");
  assert.equal(onDepth.fillQty, 25);
});

test("the model does not mutate the history it is given", () => {
  const history = series(2, () => 2.0);
  const snapshot = JSON.stringify(history);
  buildTrend(history, { now: NOW, targetPrice: 1.9 });
  assert.equal(JSON.stringify(history), snapshot);
});
