/**
 * Order-latency chart maths.
 *
 * Unlike the price chart, the order benchmark is event-driven: a quiet bucket
 * simply has no orders. The pro view therefore plots the buckets that exist and
 * lets Recharts connect them, instead of inventing a flat line through a gap.
 * Boundaries are aligned to the viewer's clock, the same convention as
 * `price.js` and `events.js`.
 */

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** Ranges offered by the expanded order chart. */
export const ORDER_PERIODS = Object.freeze([
  { key: "1h", label: "1H", hours: 1, bucket: "5m" },
  { key: "6h", label: "6H", hours: 6, bucket: "15m" },
  { key: "24h", label: "24H", hours: 24, bucket: "1h" },
  { key: "7d", label: "7D", hours: 168, bucket: "6h" }
]);

export const ORDER_BUCKETS = Object.freeze([
  { key: "5m", label: "5M", ms: 5 * MINUTE },
  { key: "15m", label: "15M", ms: 15 * MINUTE },
  { key: "30m", label: "30M", ms: 30 * MINUTE },
  { key: "1h", label: "1H", ms: HOUR },
  { key: "3h", label: "3H", ms: 3 * HOUR },
  { key: "6h", label: "6H", ms: 6 * HOUR },
  { key: "12h", label: "12H", ms: 12 * HOUR },
  { key: "1d", label: "1D", ms: DAY }
]);

/** Pipeline stages the chart can aggregate; labels match the benchmark table. */
export const ORDER_STAGES = Object.freeze([
  { key: "total", label: "End-to-end" },
  { key: "submit", label: "Submit" },
  { key: "ttfb", label: "TTFB" },
  { key: "fillWait", label: "TTFB → fill" }
]);

const TICK_STEPS = Object.freeze([
  5 * MINUTE,
  15 * MINUTE,
  30 * MINUTE,
  HOUR,
  2 * HOUR,
  3 * HOUR,
  6 * HOUR,
  12 * HOUR,
  DAY,
  2 * DAY
]);

export function orderPeriod(key) {
  return ORDER_PERIODS.find((option) => option.key === key) ?? ORDER_PERIODS[2];
}

export function orderBucket(key) {
  return ORDER_BUCKETS.find((option) => option.key === key) ?? ORDER_BUCKETS[3];
}

export function orderStage(key) {
  return ORDER_STAGES.find((option) => option.key === key) ?? ORDER_STAGES[0];
}

/** `/api/benchmark/history` query for one period/bucket/stage combination. */
export function orderHistoryPath({
  hours = 24,
  bucket = "1h",
  stage = "total",
  tzOffsetMs = 0
} = {}) {
  const params = new URLSearchParams({
    hours: String(hours),
    bucket: String(bucket),
    stage: String(stage),
    tzOffsetMs: String(tzOffsetMs)
  });
  return `/api/benchmark/history?${params.toString()}`;
}

function pad(value) {
  return String(value).padStart(2, "0");
}

function day(ts) {
  return new Date(ts).toLocaleDateString("en-GB", { day: "2-digit", month: "short" });
}

/** Human label for one bucket, local time. */
export function orderRangeLabel(start, bucketMs) {
  const from = new Date(start);
  const to = new Date(start + bucketMs);
  const sameDay =
    from.getDate() === to.getDate() &&
    from.getMonth() === to.getMonth() &&
    from.getFullYear() === to.getFullYear();
  const clock = (value) => `${pad(value.getHours())}:${pad(value.getMinutes())}`;
  if (sameDay) return `${clock(from)}–${clock(to)}`;
  return `${day(start)} ${clock(from)}–${day(start + bucketMs)} ${clock(to)}`;
}

/** Local-clock tick label for a latency chart of `spanMs`. */
export function orderAxisTick(value, spanMs, bucketMs) {
  const date = new Date(value);
  if (bucketMs >= DAY) return `${date.getDate()}/${date.getMonth() + 1}`;
  const clock = `${pad(date.getHours())}:${pad(date.getMinutes())}`;
  if (spanMs <= DAY) return clock;
  return `${date.getDate()}/${date.getMonth() + 1} ${clock}`;
}

/** Local-clock ticks sized so roughly three to six labels fit the plot. */
export function orderTimeTicks(start, end, tzOffsetMs = 0) {
  const span = end - start;
  if (!Number.isFinite(span) || span <= 0) return undefined;
  const step = TICK_STEPS.find((size) => span / size <= 5) ?? 7 * DAY;
  const offset = Number.isFinite(tzOffsetMs) ? tzOffsetMs : 0;
  const first = Math.ceil((start + offset) / step) * step - offset;
  const ticks = [];
  for (let at = first; at <= end; at += step) ticks.push(at);
  return ticks.length >= 2 ? ticks : undefined;
}

/**
 * Vertical domain for latency values. Unlike a price series, zero is a
 * meaningful floor: a negative millisecond value is a bug, not data.
 */
export function latencyDomain(values, padRatio = 0.12) {
  const list = (Array.isArray(values) ? values : [])
    .map(Number)
    .filter((value) => Number.isFinite(value) && value >= 0);
  if (list.length === 0) return [0, 1000];
  const min = Math.min(...list);
  const max = Math.max(...list);
  const span = max - min || Math.max(max * 0.1, 50);
  const pad = span * padRatio;
  return [Math.max(0, min - pad), max + pad];
}

/** Averages that never produce NaN or a negative jitter band. */
export function jitterBand(point) {
  if (point?.avg === null || point?.avg === undefined) return null;
  const avg = Number(point.avg);
  const jitter = Number(point.jitter);
  if (!Number.isFinite(avg)) return null;
  const spread = Number.isFinite(jitter) ? jitter : 0;
  return [Math.max(0, avg - spread), avg + spread];
}
