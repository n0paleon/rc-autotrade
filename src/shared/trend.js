/**
 * Trend + signal model for the Rollercoin marketplace.
 *
 * The feed is sparse and event-driven: one price point per minute at best, and
 * whole minutes (or hours) can pass without a frame for a quiet item. That rules
 * out a textbook MACD over evenly spaced bars, so this model is built around
 * three properties the raw feed actually has:
 *
 *   1. Time, not bar count, is the independent variable. Every smoothing step
 *      uses `1 - exp(-dt / tau)` so a gap does not count as one giant bar.
 *   2. Single-frame spikes are noise. A Hampel filter flattens isolated
 *      excursions in log space before any average sees them.
 *   3. Confidence must be explicit. Few samples, long gaps, or a frozen feed
 *      lower a score the UI can show instead of pretending the line is solid.
 *   4. Uncertainty must be a number, not a vibe. The model publishes how far
 *      the level may sit from the live market (`errorBandPct`) and the target is
 *      classified against that band, so a ±2% label is never printed while the
 *      level itself can be 8% off.
 *
 * Everything here is pure: no clock, no store, no config mutation. The engine
 * decides when to call it; the UI only renders the result.
 */

import { TREND_CALIBRATION } from "./trend-calibration.js";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;

export const TREND = Object.freeze({
  /** A point older than this is outside the model's memory. */
  lookbackMs: 24 * HOUR,
  /** Fast leg of the oscillator — tracks the last few frames. */
  fastTauMs: 15 * MINUTE,
  /** Slow leg of the oscillator, and the dispersion baseline for `noisePct`. */
  slowTauMs: 60 * MINUTE,
  /** Window the reference level is taken from; wide enough to smooth, short enough not to lag. */
  levelWindowMs: 2 * HOUR,
  /** Used when the primary window holds fewer than `levelMinPoints` observations. */
  levelFallbackMs: 6 * HOUR,
  /** Below this the median is not a median, it is one or two quotes. */
  levelMinPoints: 3,
  /** Window for the independent regression slope (%/hour). */
  slopeWindowMs: 3 * HOUR,
  /** |t| the slope must clear before the model is willing to call a direction. */
  slopeTStat: 2,
  /** A hole this large counts against confidence and makes the depth series ineligible. */
  maxGapMs: 30 * MINUTE,
  /** A feed with no frame for this long is stale and must not drive a suggestion. */
  staleMs: 30 * MINUTE,
  /** Hampel neighbourhood (points on each side) and threshold in robust sigmas. */
  hampelWindow: 2,
  hampelSigma: 3,
  /** Minimum robust sigma in log space (~0.5%) so a flat series still rejects single-point spikes. */
  hampelFloorLog: 0.005,
  /** Below this the series is not a trend yet, just a few frames. */
  minPoints: 12,
  minSpanMs: 30 * MINUTE,
  /** Confidence (0-100) at which a signal is considered actionable. */
  reliableConfidence: 50,
  /** Target within ±this % of the trend is "at trend" (entry band). */
  nearBandPct: 2,
  /** Wider band used to *leave* NEAR, so the label does not flap on a rounding edge. */
  nearExitPct: 3,
  /** A target this far under the trend may never fill. */
  farBelowPct: 20,
  /**
   * Confidence weights. They sum to 1 and are scaled by a source-quality
   * factor, so the score can be audited and recalibrated term by term instead
   * of being a pile of magic numbers.
   */
  weight: Object.freeze({
    fresh: 0.34,
    span: 0.28,
    count: 0.22,
    gap: 0.16
  }),
  /** Penalties for a reference the model itself considers weaker than a depth VWAP. */
  topSourceQuality: 0.85,
  thinBookQuality: 0.7,
  approxFillQuality: 0.9,
  /** Coverage (fraction of the 1-minute grid) that already counts as a dense series. */
  fullCoveragePct: 25,
  /**
   * Uncertainty band: how far the smoothed level may sit from the price the
   * market actually trades at. Dispersion and lag are the two terms a replay
   * over the stored series showed to matter; the clamps keep one wild frame
   * from declaring a whole series unknowable.
   */
  errorBandNoiseFactor: 0.9,
  errorBandLagFactor: 1.2,
  errorBandMinPct: 2,
  /** Wide enough that a calibrated factor is not clipped back to the formula. */
  errorBandMaxPct: 20,
  /** Classification band, clamped to the same section as the uncertainty itself. */
  bandMaxPct: 10,
  /**
   * At or above this width the middle zone reads UNSURE instead of NEAR: the
   * level is too loose for "at trend" to say anything the error bars do not.
   * Sits near the median band measured on the stored series (~5%).
   */
  unsureBandPct: 5
});

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value));
}

function round(value, digits = 3) {
  if (!Number.isFinite(value)) return null;
  const factor = 10 ** digits;
  return Math.round((value + Number.EPSILON) * factor) / factor;
}

function median(values) {
  if (!values || values.length === 0) return NaN;
  const sorted = [...values].sort((a, b) => a - b);
  const middle = sorted.length >> 1;
  return sorted.length % 2 === 1
    ? sorted[middle]
    : (sorted[middle - 1] + sorted[middle]) / 2;
}

/** One sample per timestamp, oldest first, inside the lookback window. */
function preparePoints(history, now) {
  const byTime = new Map();
  for (const point of Array.isArray(history) ? history : []) {
    const t = Number(point?.t);
    const p = Number(point?.p);
    if (!Number.isFinite(t) || !Number.isFinite(p) || p <= 0) continue;
    if (t < now - TREND.lookbackMs || t > now + MINUTE) continue;
    byTime.set(t, { t, p });
  }
  return [...byTime.values()].sort((a, b) => a.t - b.t);
}

/**
 * Hampel filter in log space.
 *
 * A single cheap listing appears and disappears within a minute; that is a
 * spike, not a repricing. Sustained moves survive because after ~2 points the
 * neighbourhood median follows them. When the local MAD is zero (a flat
 * series), a small sigma floor still catches isolated jumps.
 */
function hampel(values, window, sigma) {
  const out = values.slice();
  if (values.length < 5) return { values: out, outliers: 0 };

  let outliers = 0;
  for (let i = 0; i < values.length; i += 1) {
    const lo = Math.max(0, i - window);
    const hi = Math.min(values.length - 1, i + window);
    const slice = values.slice(lo, hi + 1);
    const center = median(slice);
    const scale = Math.max(
      1.4826 * median(slice.map((value) => Math.abs(value - center))),
      TREND.hampelFloorLog
    );
    if (Math.abs(values[i] - center) > sigma * scale) {
      out[i] = center;
      outliers += 1;
    }
  }
  return { values: out, outliers };
}

/**
 * Time-aware EMA. `dt` is used as-is, with no cap: after a six-hour hole the
 * old average carries no information about the new price, and `alpha → 1` says
 * so on its own. Clamping `dt` at `maxGapMs` made the leg crawl toward the new
 * level (39% of the jump for the 60-minute tau), which left the oscillator
 * reporting momentum for hours after the move had already happened. On the
 * stored series dropping the cap narrowed the calibrated band (p50 7.8% → 7.3%
 * at equal coverage) and improved the rank correlation between the published
 * band and the realised error (0.21 → 0.26).
 */
function ema(values, times, tauMs) {
  const out = new Array(values.length);
  out[0] = values[0];
  for (let i = 1; i < values.length; i += 1) {
    const dt = Math.max(times[i] - times[i - 1], 0);
    const alpha = dt > 0 ? 1 - Math.exp(-dt / tauMs) : 0;
    out[i] = out[i - 1] + alpha * (values[i] - out[i - 1]);
  }
  return out;
}

/**
 * The level the target is compared against: a geometric median of the recent
 * window, not the EMA endpoint.
 *
 * The EMA is an exponential blend, so a 60-minute tau trails a move by roughly
 * a full tau; on the stored series its endpoint sat a median 4.2% from the
 * live price and 8.7% at p75, which is exactly the width of the classification
 * band it was being used to judge. A median ignores the shape of the decay and
 * tolerates a single wild quote, so it tracks the traded level far better
 * (3.4% / 7.5% on the same replay) while staying as smooth as the EMA.
 *
 * When the window is too empty to take a median — a quiet item, a long hole —
 * the next window up is tried, and only then does the caller's fallback win.
 */
function robustLevel(points, now, fallback) {
  const logsOf = (window) => Math.exp(median(window.map((point) => Math.log(point.p))));
  const recent = points.filter((point) => point.t >= now - TREND.levelWindowMs);
  if (recent.length >= TREND.levelMinPoints) return logsOf(recent);
  const wide = points.filter((point) => point.t >= now - TREND.levelFallbackMs);
  if (wide.length >= TREND.levelMinPoints) return logsOf(wide);
  return fallback;
}

/**
 * Weighted least-squares slope of log price against time over the recent
 * window, in %/hour. Each sample stands for the time until the next one, so a
 * dense burst of frames does not outvote an hour of calm.
 */
function weightedSlope(values, times, windowMs) {
  const cutoff = times[times.length - 1] - windowMs;
  let start = 0;
  while (start < times.length - 1 && times[start] < cutoff) start += 1;

  const xs = [];
  const ys = [];
  const ws = [];
  for (let i = start; i < times.length; i += 1) {
    const dtNext =
      i + 1 < times.length
        ? times[i + 1] - times[i]
        : i > 0
          ? times[i] - times[i - 1]
          : MINUTE;
    xs.push(times[i] / HOUR);
    ys.push(values[i]);
    ws.push(Math.max(dtNext, 1));
  }
  if (xs.length < 4) return null;

  const weightSum = ws.reduce((total, weight) => total + weight, 0);
  const xMean = xs.reduce((total, x, i) => total + x * ws[i], 0) / weightSum;
  const yMean = ys.reduce((total, y, i) => total + y * ws[i], 0) / weightSum;

  let numerator = 0;
  let denominator = 0;
  for (let i = 0; i < xs.length; i += 1) {
    const dx = xs[i] - xMean;
    numerator += ws[i] * dx * (ys[i] - yMean);
    denominator += ws[i] * dx * dx;
  }
  if (denominator <= 1e-12) return null;
  const rawSlope = numerator / denominator;
  // Weighted residual variance gives the slope a standard error, so "up" can
  // mean "the regression can tell it apart from noise" instead of "the sign of
  // a number that is mostly noise".
  let sse = 0;
  for (let i = 0; i < xs.length; i += 1) {
    const fitted = yMean + rawSlope * (xs[i] - xMean);
    sse += ws[i] * (ys[i] - fitted) ** 2;
  }
  const dof = xs.length - 2;
  const se = dof > 0 && sse > 0 ? Math.sqrt(sse / dof / denominator) : 0;
  return {
    // log slope × 100 ≈ percent change per hour for the small moves seen here.
    value: rawSlope * 100,
    tStat: se > 0 ? rawSlope / se : 0
  };
}

/**
 * Scale factor measured by `scripts/calibrate-trend.js`: how much the built-in
 * estimate under-states the error this worker actually observes. 1 when the
 * worker has never been calibrated, so the model degrades to its own estimate.
 */
export function calibrationFactor(source = "top") {
  const factors = TREND_CALIBRATION?.factors ?? {};
  const value = factors[source] ?? factors.pooled;
  return Number.isFinite(value) && value > 0 ? value : 1;
}

/**
 * Market-only model. Deliberately independent of `targetPrice` so a target edit
 * never invalidates the history behind the line.
 */
export function computeTrendModel(history, options = {}) {
  const now = Number.isFinite(options.now) ? options.now : Date.now();
  const points = preparePoints(history, now);
  const lastSampleAt = points.length > 0 ? points[points.length - 1].t : null;
  const bookQty = Number.isFinite(options.bookQty) ? options.bookQty : null;
  const minQty = Number.isFinite(options.minQty) ? options.minQty : null;

  const source = options.source === "depth" ? "depth" : "top";
  const fillApprox = Boolean(options.fill?.approx);

  const base = {
    status: "no_data",
    reason: "not enough samples",
    source,
    fillPrice: Number.isFinite(options.fill?.price) ? options.fill.price : null,
    fillQty: Number.isFinite(options.fill?.filled) ? options.fill.filled : null,
    fillPartial: Boolean(options.fill?.partial),
    fillApprox,
    trend: null,
    fast: null,
    momentumPct: null,
    slopePctPerHour: null,
    slopeTStat: null,
    noisePct: null,
    confidence: 0,
    /** How far the level may sit from the live market; null until the model runs. */
    errorBandPct: null,
    /** "calibrated" when the band came from the measured history, else "formula". */
    bandSource: null,
    points: points.length,
    spanMs: 0,
    maxGapMs: null,
    coveragePct: 0,
    outliers: 0,
    direction: "flat",
    stale: false,
    lastSampleAt,
    computedAt: now,
    bookQty,
    minQty,
    thinBook: bookQty != null && minQty != null ? bookQty < minQty : false
  };

  if (points.length < TREND.minPoints) return base;

  const spanMs = points[points.length - 1].t - points[0].t;
  if (spanMs < TREND.minSpanMs) {
    return { ...base, reason: "warming up", spanMs };
  }

  const age = now - points[points.length - 1].t;
  const stale = age > TREND.staleMs;

  let maxGap = 0;
  for (let i = 1; i < points.length; i += 1) {
    maxGap = Math.max(maxGap, points[i].t - points[i - 1].t);
  }

  const expected = Math.max(1, Math.round(spanMs / MINUTE) + 1);
  const coveragePct = Math.round(clamp(points.length / expected, 0, 1) * 100);

  const times = points.map((point) => point.t);
  const logs = points.map((point) => Math.log(point.p));
  const cleaned = hampel(logs, TREND.hampelWindow, TREND.hampelSigma);

  const fast = ema(cleaned.values, times, TREND.fastTauMs);
  const slow = ema(cleaned.values, times, TREND.slowTauMs);

  const last = logs.length - 1;
  // The reference level is the robust median; the EMA legs stay because
  // `momentumPct` and the dispersion baseline still need a fast/slow pair.
  const trend = robustLevel(points, now, Math.exp(slow[last]));
  const fastPrice = Math.exp(fast[last]);
  const momentumPct = (fast[last] - slow[last]) * 100;
  const slope = weightedSlope(cleaned.values, times, TREND.slopeWindowMs);
  const slopePctPerHour = slope == null ? null : slope.value;
  const slopeTStat = slope == null ? null : slope.tStat;
  const noisePct =
    median(cleaned.values.map((value, index) => Math.abs(value - slow[index]))) *
    1.4826 *
    100;

  // Confidence answers one question: how much evidence is there? Freshness,
  // window, sample count, holes and coverage. Precision is a different question
  // and has its own number (`errorBandPct`): folding dispersion and lag in here
  // as well meant a noisy series was punished twice and the score stopped being
  // explainable. `reliable` is the verdict that combines both.
  const fresh = clamp(1 - age / TREND.staleMs, 0, 1);
  // Coverage gates the whole score, not one term: a window that is 2% populated
  // cannot support *any* of the readings, no matter how fresh the last frame is
  // or how calm the quotes look. Twelve hours with 17 points used to score in
  // the fifties because "no noise" and "no lag" paid out regardless of evidence.
  // The percentage is measured against a perfect 1-minute grid, which the feed
  // never actually delivers, so 25% (a point every ~4 minutes) already counts as
  // dense for a 15/60-minute EMA.
  const density = clamp((coveragePct / 100) / (TREND.fullCoveragePct / 100), 0, 1);
  const spanScore = clamp(spanMs / (6 * HOUR), 0, 1);
  const countScore = clamp(points.length / 120, 0, 1);
  const gapScore = 1 / (1 + maxGap / (TREND.maxGapMs * 1.5));
  // A top-ask series (depth history still warming up) and a thin book are
  // weaker references than a depth VWAP for the configured order size.
  const quality =
    (source === "depth" ? 1 : TREND.topSourceQuality) *
    (base.thinBook ? TREND.thinBookQuality : 1) *
    (fillApprox ? TREND.approxFillQuality : 1);
  const weight = TREND.weight;
  let confidence =
    100 *
    quality *
    (0.4 + 0.6 * density) *
    (weight.fresh * fresh +
      weight.span * spanScore +
      weight.count * countScore +
      weight.gap * gapScore);
  if (stale) confidence = Math.min(confidence, 20);

  // How far the level may sit from where the market actually trades. The raw
  // estimate comes from dispersion (how widely quotes scatter around the trend)
  // and lag (the fast/slow gap, which is how far the smoothed level trails a
  // move). `calibrationFactor` rescales it by what the replay actually observed
  // — 1.00 when the worker has never been calibrated.
  const rawBandPct = clamp(
    TREND.errorBandNoiseFactor * noisePct +
      TREND.errorBandLagFactor * Math.abs(momentumPct),
    TREND.errorBandMinPct,
    TREND.errorBandMaxPct
  );
  const factor = calibrationFactor(source);
  const errorBandPct = clamp(
    rawBandPct * factor,
    TREND.errorBandMinPct,
    TREND.errorBandMaxPct
  );

  // A direction is only published when the regression clears its own noise
  // floor; otherwise the honest label is "flat", not a coin flip dressed as a
  // reading. Scored against the next hour on the stored series, the slope sign
  // was right about half the time — so the field describes the last three hours
  // and is never a forecast. `bun run calibrate:trend` re-measures it every run.
  const directionSignificant =
    slopeTStat != null && Math.abs(slopeTStat) >= TREND.slopeTStat;
  const directionValue = slopePctPerHour ?? momentumPct;
  const direction = !directionSignificant
    ? "flat"
    : directionValue > 0.3
      ? "up"
      : directionValue < -0.3
        ? "down"
        : "flat";

  return {
    ...base,
    status: stale ? "stale" : "ok",
    reason: stale ? "no fresh market frame" : null,
    trend: round(trend, 6),
    fast: round(fastPrice, 6),
    momentumPct: round(momentumPct, 3),
    slopePctPerHour: slopePctPerHour == null ? null : round(slopePctPerHour, 3),
    slopeTStat: slopeTStat == null ? null : round(slopeTStat, 2),
    noisePct: round(noisePct, 3),
    confidence: Math.round(confidence),
    errorBandPct: round(errorBandPct, 2),
    bandSource: TREND_CALIBRATION?.version > 0 ? "calibrated" : "formula",
    points: points.length,
    spanMs,
    maxGapMs: maxGap,
    coveragePct,
    outliers: cleaned.outliers,
    stale,
    direction,
    lastSampleAt,
    computedAt: now
  };
}

/**
 * Adds the target-relative reading on top of the market model.
 *
 * The middle band is not a constant: it is derived from the model's own
 * uncertainty, because a ±2% label is meaningless when the level it is measured
 * against can sit 8% away from the live market. `previousState` still drives
 * hysteresis, so a target parked on the boundary does not alternate between
 * "at trend" and "above trend" on every frame.
 */
export function buildTrend(history, options = {}) {
  const model = computeTrendModel(history, options);
  const target = Number(options.targetPrice);
  const validTarget = Number.isFinite(target) && target > 0;

  if (model.status !== "ok") {
    return {
      ...model,
      target: validTarget ? target : null,
      proximityPct: null,
      gapPct: null,
      bandPct: null,
      uncertain: false,
      state: model.status === "stale" ? "STALE" : "NO_DATA",
      reliable: false,
      reliableReason: model.status === "stale" ? "stale" : "no-data"
    };
  }

  if (!validTarget) {
    return {
      ...model,
      target: null,
      proximityPct: null,
      gapPct: null,
      // The band is about the market series, not the target: keep publishing it
      // so the UI can still say how loose the level is while no ceiling is set.
      bandPct: round(classificationBandPct(model.errorBandPct), 2),
      uncertain: false,
      state: "NO_TARGET",
      reliable: false,
      reliableReason: "no-target"
    };
  }

  const proximityPct = (target / model.trend - 1) * 100;
  const bandPct = classificationBandPct(model.errorBandPct);
  const enoughEvidence = model.confidence >= TREND.reliableConfidence;
  const trustworthyBand = model.errorBandPct <= TREND.bandMaxPct;
  const state = classifyState(proximityPct, options.previousState, bandPct);
  return {
    ...model,
    target,
    proximityPct: round(proximityPct, 3),
    gapPct: round(-proximityPct, 3),
    bandPct: round(bandPct, 2),
    uncertain: state === "UNSURE",
    state,
    // A reading is actionable when there is enough evidence *and* the level is
    // tight enough for the proximity to mean something; `reliableReason` says
    // which half failed, so the UI can explain instead of just dimming the row.
    reliable: enoughEvidence && trustworthyBand,
    reliableReason:
      enoughEvidence && trustworthyBand
        ? null
        : enoughEvidence
          ? "wide-band"
          : "low-confidence",
  };
}

/** Widens the entry band with the model's uncertainty, inside hard limits. */
export function classificationBandPct(errorBandPct) {
  const band = Number.isFinite(errorBandPct) ? errorBandPct : TREND.nearBandPct;
  return clamp(band, TREND.nearBandPct, TREND.bandMaxPct);
}

/**
 * Positive proximity = target above the trend (the bot would buy at market);
 * negative = target below it (waiting for a dip).
 *
 * `bandPct` defaults to the fixed 2% entry band so callers that only compare
 * numbers keep the old reading; the model passes a band derived from its own
 * uncertainty. A band wide enough that "near" and "far" stop being
 * distinguishable yields UNSURE instead of a false NEAR.
 */
export function classifyState(proximityPct, previousState = null, bandPct = TREND.nearBandPct) {
  const band = Number.isFinite(bandPct) && bandPct > 0 ? bandPct : TREND.nearBandPct;
  const held = previousState === "NEAR" || previousState === "UNSURE";
  const exit = held ? Math.max(band * 1.5, TREND.nearExitPct) : band;
  if (proximityPct > exit) return "ABOVE";
  if (proximityPct < -exit) {
    return proximityPct <= -TREND.farBelowPct ? "FAR_BELOW" : "BELOW";
  }
  if (held) return previousState;
  return band >= TREND.unsureBandPct ? "UNSURE" : "NEAR";
}
