#!/usr/bin/env bun
/**
 * Measures how far the trend level usually sits from the price that is actually
 * trading, and writes the result into `src/shared/trend-calibration.js`.
 *
 * The model estimates its own uncertainty from dispersion and lag: cheap,
 * always available, and only roughly calibrated. This script replays the stored
 * price history, compares that estimate with the error that actually happened,
 * and records the factor that makes the published band cover the requested
 * share of cases. `errorBandPct` then means what the UI claims it means: the
 * level was that close to the traded price in `target` of the observed moments.
 *
 * A single factor per series type is deliberate. Bucketing the estimate into a
 * curve reads better on paper but needs thousands of samples before the bucket
 * edges stop being noise; on a 12-hour database it produced a 24% band for the
 * calmest bucket because two jumps landed there. The factor carries the same
 * information at a fraction of the variance, and it cannot invert.
 *
 * Usage:
 *   bun run calibrate:trend                       # last 14 days, 80th percentile
 *   bun run calibrate:trend --days=7 --target=0.9
 *   bun scripts/calibrate-trend.js --db=data/rollercoin.db --out=src/shared/trend-calibration.js
 *
 * The replay is deterministic: same database in, same numbers out. Run it once
 * the worker has collected a few days of price points; below the sample floor
 * the script refuses to write rather than encode noise.
 */

import { Database } from "bun:sqlite";
import { writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { computeTrendModel, TREND } from "../src/shared/trend.js";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** Below this the factor is noise; keep the built-in estimate instead. */
const MIN_TOTAL_SAMPLES = 200;
/** A per-source factor needs its own evidence before it may override the pooled one. */
const MIN_SOURCE_SAMPLES = 200;
/** Sanity rails: a factor outside this range means the replay is broken, not the market. */
const FACTOR_MIN = 0.75;
const FACTOR_MAX = 2;

const args = new Map(
  process.argv.slice(2).map((arg) => {
    const [key, value = "true"] = arg.replace(/^--/, "").split("=");
    return [key, value];
  })
);

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");
const dbPath = resolve(root, args.get("db") ?? "data/rollercoin.db");
const outPath = resolve(root, args.get("out") ?? "src/shared/trend-calibration.js");
const days = Number(args.get("days") ?? 14);
const stepMinutes = Math.max(1, Number(args.get("step") ?? 10));
const target = Math.min(0.95, Math.max(0.5, Number(args.get("target") ?? 0.8)));

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value));
}

function pct(value) {
  return value == null ? "n/a" : `${(value * 100).toFixed(1)}%`;
}

function quantile(values, p) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil(p * sorted.length) - 1));
  return sorted[index];
}

function loadSeries(db) {
  const cutoff = Date.now() - days * DAY;
  const rows = db
    .query(
      "SELECT item_id, bucket_at, price, fill_price FROM price_points WHERE bucket_at >= ? ORDER BY item_id, bucket_at"
    )
    .all(cutoff);

  const items = new Map();
  for (const row of rows) {
    if (!items.has(row.item_id)) items.set(row.item_id, { top: [], fill: [] });
    const entry = items.get(row.item_id);
    if (Number.isFinite(row.price) && row.price > 0) {
      entry.top.push({ t: row.bucket_at, p: row.price });
    }
    if (Number.isFinite(row.fill_price) && row.fill_price > 0) {
      entry.fill.push({ t: row.bucket_at, p: row.fill_price });
    }
  }
  return items;
}

/** The built-in estimate, exactly as `computeTrendModel` builds it. */
function rawEstimate(model) {
  return clamp(
    TREND.errorBandNoiseFactor * (model.noisePct ?? 0) +
      TREND.errorBandLagFactor * Math.abs(model.momentumPct ?? 0),
    TREND.errorBandMinPct,
    TREND.errorBandMaxPct
  );
}

/**
 * The next price after a 60-minute horizon, used only to score the direction
 * label. Wide tolerance because the feed is sparse.
 */
function forwardReturn(series, now, price) {
  const next = series.find((point) => point.t >= now + 60 * MINUTE && point.t <= now + 85 * MINUTE);
  return next ? Math.log(next.p / price) : null;
}

/**
 * One sample per replayed instant and series: the raw estimate the model would
 * publish, the distance it actually ended up from the traded price, and what
 * its direction label implied about the next hour.
 */
function collectSamples(items) {
  const samples = [];
  for (const [, series] of items) {
    const top = series.top;
    if (top.length < TREND.minPoints) continue;
    const start = top[0].t + 40 * MINUTE;
    const end = top[top.length - 1].t;

    for (let now = start; now <= end; now += stepMinutes * MINUTE) {
      const upto = top.filter((point) => point.t <= now);
      if (upto.length < TREND.minPoints) continue;
      const model = computeTrendModel(upto, { now, source: "top" });
      if (model.status === "ok") {
        const price = upto[upto.length - 1].p;
        samples.push({
          t: now,
          source: "top",
          raw: rawEstimate(model),
          err: Math.abs(model.trend / price - 1) * 100,
          dir: model.direction,
          slope: model.slopePctPerHour,
          fwd: forwardReturn(top, now, price)
        });
      }

      const fill = series.fill.filter((point) => point.t <= now);
      const span = fill.length > 1 ? fill[fill.length - 1].t - fill[0].t : 0;
      const fresh = fill.length > 0 && now - fill[fill.length - 1].t <= TREND.maxGapMs;
      if (fill.length < TREND.minPoints || span < TREND.minSpanMs || !fresh) continue;
      const depth = computeTrendModel(fill, { now, source: "depth" });
      if (depth.status !== "ok") continue;
      const price = fill[fill.length - 1].p;
      samples.push({
        t: now,
        source: "depth",
        raw: rawEstimate(depth),
        err: Math.abs(depth.trend / price - 1) * 100,
        dir: depth.direction,
        slope: depth.slopePctPerHour,
        fwd: forwardReturn(series.fill, now, price)
      });
    }
  }
  samples.sort((a, b) => a.t - b.t);
  return samples;
}

/**
 * Is the direction label a forecast? Scored against the next hour, next to the
 * naive base rate: if a coin flip beats it, the label is a description of the
 * last three hours and nothing more. Printed on every run so the claim can rot
 * visibly instead of silently.
 */
function scoreDirection(samples) {
  const scored = samples.filter((sample) => Number.isFinite(sample.fwd));
  if (scored.length === 0) return null;
  const up = scored.filter((sample) => sample.fwd > 0).length;
  const calls = scored.filter((sample) => sample.dir === "up" || sample.dir === "down");
  const hits = calls.filter((sample) => (sample.dir === "up") === (sample.fwd > 0)).length;
  const slopeCalls = scored.filter(
    (sample) => Number.isFinite(sample.slope) && Math.abs(sample.slope) > 0.3
  );
  const slopeHits = slopeCalls.filter(
    (sample) => (sample.slope > 0) === (sample.fwd > 0)
  ).length;
  return {
    samples: scored.length,
    baseUp: up / scored.length,
    calls: calls.length,
    hit: calls.length > 0 ? hits / calls.length : null,
    slopeCalls: slopeCalls.length,
    slopeHit: slopeCalls.length > 0 ? slopeHits / slopeCalls.length : null
  };
}

/**
 * The factor that makes `raw` cover `target` of the observed errors. Uses the
 * ratio rather than the error itself so dense and quiet series contribute on
 * the same scale.
 */
function measureFactor(samples) {
  const ratios = samples.map((sample) => sample.err / Math.max(sample.raw, 0.5));
  const factor = quantile(ratios, target);
  return Number(clamp(factor ?? 1, FACTOR_MIN, FACTOR_MAX).toFixed(2));
}

/**
 * Share of samples whose realised error fits inside the published band. Accepts
 * either one factor or the per-source map, so the score can be taken with the
 * exact numbers the worker will use.
 */
function coverage(samples, factors) {
  if (samples.length === 0) return 0;
  const factorFor = (sample) =>
    typeof factors === "number" ? factors : (factors[sample.source] ?? factors.pooled ?? 1);
  const covered = samples.filter(
    (sample) =>
      sample.err <=
      clamp(factorFor(sample) * sample.raw, TREND.errorBandMinPct, TREND.errorBandMaxPct)
  ).length;
  return covered / samples.length;
}

function renderModule({ generatedAt, samples, factors, counts, coveragePct, holdoutPct, target }) {
  const block = (value) =>
    JSON.stringify(value, null, 2)
      .split("\n")
      .join("\n  ");
  return `/**
 * How much the built-in uncertainty estimate under- or over-states the error
 * this worker actually sees, measured from its own price series.
 *
 * Generated by \`bun run calibrate:trend\` — do not edit by hand. An empty table
 * is valid: the model then falls back to the built-in estimate
 * (\`errorBandNoiseFactor\` / \`errorBandLagFactor\`).
 *
 * Generated: ${generatedAt}
 * Replayed: ${samples} instants · top ${counts.top} · depth ${counts.depth}
 * Target quantile: ${target.toFixed(2)} · coverage in-sample ${(coveragePct * 100).toFixed(1)}%${
    holdoutPct == null ? "" : ` · holdout ${(holdoutPct * 100).toFixed(1)}%`
  }
 *
 * \`factors\` multiplies the raw estimate (0.9 × noisePct + 1.2 × |momentumPct|)
 * before it is clamped. 1.00 means the built-in estimate was already right; a
 * value below 1 means it was too pessimistic. Sources without enough samples
 * inherit \`pooled\`.
 */
export const TREND_CALIBRATION = Object.freeze({
  version: 1,
  generatedAt: "${generatedAt}",
  target: ${target},
  samples: ${samples},
  counts: Object.freeze(${block(counts)}),
  factors: Object.freeze(${block(factors)})
});
`;
}

async function main() {
  let db;
  try {
    db = new Database(dbPath, { readonly: true });
  } catch (error) {
    console.error(`cannot open ${dbPath}: ${error.message}`);
    process.exit(1);
  }

  const items = loadSeries(db);
  db.close();

  const samples = collectSamples(items);
  if (samples.length < MIN_TOTAL_SAMPLES) {
    console.error(
      `only ${samples.length} samples (< ${MIN_TOTAL_SAMPLES}); keeping the built-in estimate.\n` +
        "Let the worker collect a few more days of price points, then run this again."
    );
    process.exit(1);
  }

  const pooledFactor = measureFactor(samples);
  const topSamples = samples.filter((sample) => sample.source === "top");
  const depthSamples = samples.filter((sample) => sample.source === "depth");
  const factors = {
    pooled: pooledFactor,
    top: topSamples.length >= MIN_SOURCE_SAMPLES ? measureFactor(topSamples) : pooledFactor,
    depth: depthSamples.length >= MIN_SOURCE_SAMPLES ? measureFactor(depthSamples) : pooledFactor
  };
  const counts = { pooled: samples.length, top: topSamples.length, depth: depthSamples.length };

  // Holdout: measure on the older 70%, score on the newer 30%. A factor that
  // only fits the window it was measured on is a factor that memorised a mood.
  const split = Math.floor(samples.length * 0.7);
  const holdoutFactor = measureFactor(samples.slice(0, split));
  const newer = samples.slice(split);
  const holdoutPct = newer.length > 0 ? coverage(newer, holdoutFactor) : null;

  const coveragePct = coverage(samples, factors);
  const baselinePct = coverage(samples, 1);

  const generatedAt = new Date().toISOString();
  await writeFile(
    outPath,
    renderModule({
      generatedAt,
      samples: samples.length,
      factors,
      counts,
      coveragePct,
      holdoutPct,
      target
    }),
    "utf8"
  );

  console.log(`samples    ${samples.length} (top ${counts.top} · depth ${counts.depth})`);
  console.log(
    `baseline   ${(baselinePct * 100).toFixed(1)}% covered by the built-in estimate (factor 1.00)`
  );
  console.log(
    `calibrated ${(coveragePct * 100).toFixed(1)}% in-sample with factors top ${factors.top.toFixed(2)} / depth ${factors.depth.toFixed(2)}`
  );
  if (holdoutPct != null) {
    console.log(
      `holdout    ${(holdoutPct * 100).toFixed(1)}% on the newest 30% (factor ${holdoutFactor.toFixed(2)})`
    );
  }
  const direction = scoreDirection(samples);
  if (direction) {
    console.log(
      `direction  ${pct(direction.hit)} hit on ${direction.calls} non-flat calls · ` +
        `slope sign ${pct(direction.slopeHit)} on ${direction.slopeCalls} · ` +
        `base rate up ${pct(direction.baseUp)} (60m horizon)`
    );
    if (direction.hit != null && direction.hit <= Math.max(direction.baseUp, 1 - direction.baseUp)) {
      console.log("           ^ no better than the base rate: treat it as a description, not a forecast");
    }
  }
  console.log(`written    ${outPath}`);
}

main().catch((error) => {
  console.error(error?.stack || error?.message || String(error));
  process.exit(1);
});
