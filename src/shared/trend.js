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
 *
 * Everything here is pure: no clock, no store, no config mutation. The engine
 * decides when to call it; the UI only renders the result.
 */

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;

export const TREND = Object.freeze({
  /** A point older than this is outside the model's memory. */
  lookbackMs: 24 * HOUR,
  /** Fast leg of the oscillator — tracks the last few frames. */
  fastTauMs: 15 * MINUTE,
  /** Slow leg — the trend level itself. */
  slowTauMs: 60 * MINUTE,
  /** Signal line: an EMA of the oscillator, like MACD's signal. */
  signalTauMs: 20 * MINUTE,
  /** Window for the independent regression slope (%/hour). */
  slopeWindowMs: 3 * HOUR,
  /** Longest gap treated as continuous; larger gaps still move the EMA but count against confidence. */
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
  farBelowPct: 20
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

/** Time-aware EMA; a gap is clamped so one late frame cannot reset the leg. */
function ema(values, times, tauMs, capMs) {
  const out = new Array(values.length);
  out[0] = values[0];
  for (let i = 1; i < values.length; i += 1) {
    const dt = clamp(times[i] - times[i - 1], 0, capMs);
    const alpha = dt > 0 ? 1 - Math.exp(-dt / tauMs) : 0;
    out[i] = out[i - 1] + alpha * (values[i] - out[i - 1]);
  }
  return out;
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
  // log slope × 100 ≈ percent change per hour for the small moves seen here.
  return (numerator / denominator) * 100;
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

  const base = {
    status: "no_data",
    reason: "not enough samples",
    source: options.source === "depth" ? "depth" : "top",
    fillPrice: Number.isFinite(options.fill?.price) ? options.fill.price : null,
    fillQty: Number.isFinite(options.fill?.filled) ? options.fill.filled : null,
    fillPartial: Boolean(options.fill?.partial),
    fillApprox: Boolean(options.fill?.approx),
    trend: null,
    fast: null,
    momentumPct: null,
    signalPct: null,
    histPct: null,
    slopePctPerHour: null,
    noisePct: null,
    confidence: 0,
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

  const fast = ema(cleaned.values, times, TREND.fastTauMs, TREND.maxGapMs);
  const slow = ema(cleaned.values, times, TREND.slowTauMs, TREND.maxGapMs);
  const momentum = fast.map((value, index) => (value - slow[index]) * 100);
  const signal = ema(momentum, times, TREND.signalTauMs, TREND.maxGapMs);

  const last = logs.length - 1;
  const trend = Math.exp(slow[last]);
  const fastPrice = Math.exp(fast[last]);
  const momentumPct = (fast[last] - slow[last]) * 100;
  const signalPct = signal[last];
  const histPct = momentumPct - signalPct;
  const slopePctPerHour = weightedSlope(cleaned.values, times, TREND.slopeWindowMs);
  const noisePct =
    median(cleaned.values.map((value, index) => Math.abs(value - slow[index]))) *
    1.4826 *
    100;

  // Confidence is a product of how much data exists, how fresh it is, and how
  // noisy it is — not of how strong the signal looks. A dramatic move on two
  // frames must still read as unreliable.
  const fresh = clamp(1 - age / TREND.staleMs, 0, 1);
  const spanScore = clamp(spanMs / (6 * HOUR), 0, 1);
  const countScore = clamp(points.length / 120, 0, 1);
  const gapScore = clamp(1 - maxGap / (TREND.maxGapMs * 2), 0, 1);
  const noiseScore = clamp(1 - noisePct / 4, 0, 1);
  let confidence =
    100 *
    (0.3 * fresh + 0.25 * spanScore + 0.2 * countScore + 0.15 * gapScore + 0.1 * noiseScore);
  if (stale) confidence = Math.min(confidence, 20);

  const directionValue = slopePctPerHour ?? momentumPct;
  const direction =
    directionValue > 0.3 ? "up" : directionValue < -0.3 ? "down" : "flat";

  return {
    ...base,
    status: stale ? "stale" : "ok",
    reason: stale ? "no fresh market frame" : null,
    trend: round(trend, 6),
    fast: round(fastPrice, 6),
    momentumPct: round(momentumPct, 3),
    signalPct: round(signalPct, 3),
    histPct: round(histPct, 3),
    slopePctPerHour: slopePctPerHour == null ? null : round(slopePctPerHour, 3),
    noisePct: round(noisePct, 3),
    confidence: Math.round(confidence),
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
 * `previousState` implements hysteresis: entering NEAR happens at ±2%, leaving
 * it needs ±3%, so a target parked on the boundary does not alternate between
 * "at trend" and "below trend" on every frame.
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
      state: model.status === "stale" ? "STALE" : "NO_DATA",
      reliable: false
    };
  }

  if (!validTarget) {
    return {
      ...model,
      target: null,
      proximityPct: null,
      gapPct: null,
      state: "NO_TARGET",
      reliable: false
    };
  }

  const proximityPct = (target / model.trend - 1) * 100;
  return {
    ...model,
    target,
    proximityPct: round(proximityPct, 3),
    gapPct: round(-proximityPct, 3),
    state: classifyState(proximityPct, options.previousState),
    reliable: model.confidence >= TREND.reliableConfidence
  };
}

/**
 * Positive proximity = target above the trend (the bot would buy at market);
 * negative = target below it (waiting for a dip).
 */
export function classifyState(proximityPct, previousState = null) {
  const band = previousState === "NEAR" ? TREND.nearExitPct : TREND.nearBandPct;
  if (proximityPct > band) return "ABOVE";
  if (proximityPct < -band) {
    return proximityPct <= -TREND.farBelowPct ? "FAR_BELOW" : "BELOW";
  }
  return "NEAR";
}
