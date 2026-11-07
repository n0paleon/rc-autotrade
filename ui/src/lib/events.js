/**
 * Market-activity chart maths.
 *
 * The worker stores one row per 5-minute bucket and returns them untouched.
 * Summing happens here so every timeframe boundary is aligned to the viewer's
 * local clock: `bucketEvents` shifts by the timezone offset before flooring,
 * then shifts back, which keeps a "1 hour" bar on the hour even when the
 * worker and the browser sit in different zones.
 */

export const EVENT_PERIODS = Object.freeze([
  { key: "1", label: "1D", days: 1 },
  { key: "3", label: "3D", days: 3 },
  { key: "7", label: "7D", days: 7 }
]);

export const EVENT_TIMEFRAMES = Object.freeze({
  "5m": { label: "5M", ms: 5 * 60_000, minutes: 5 },
  "15m": { label: "15M", ms: 15 * 60_000, minutes: 15 },
  "30m": { label: "30M", ms: 30 * 60_000, minutes: 30 },
  "1h": { label: "1H", ms: 60 * 60_000, minutes: 60 },
  "2h": { label: "2H", ms: 2 * 60 * 60_000, minutes: 120 }
});

export const EVENT_DEFAULT_PERIOD = "1";
export const EVENT_DEFAULT_TIMEFRAME = "5m";

const MINUTE = 60_000;
const DAY = 86_400_000;

/** Local-clock offset for an instant, in milliseconds east of UTC. */
export function timezoneOffsetMs(now = Date.now()) {
  const offset = new Date(now).getTimezoneOffset();
  return Number.isFinite(offset) ? -offset * MINUTE : 0;
}

/**
 * Sums 5-minute points into `bucketMs` buckets aligned to local time and fills
 * the quiet buckets with zeros, so a gap in the chart means "no events", not
 * "no data". Points outside the window are ignored.
 */
export function bucketEvents(
  points,
  { days = 1, bucketMs = 5 * MINUTE, now = Date.now(), tzOffsetMs } = {}
) {
  const size = Number(bucketMs);
  if (!Number.isFinite(size) || size <= 0) return [];
  const span = Math.max(1, Number(days) || 1) * DAY;
  const offset = Number.isFinite(tzOffsetMs) ? Number(tzOffsetMs) : timezoneOffsetMs(now);
  const align = (at) => Math.floor((at + offset) / size) * size - offset;

  const last = align(now);
  const first = align(now - span);
  if (!(last >= first)) return [];

  const buckets = new Map();
  for (let at = first; at <= last; at += size) buckets.set(at, 0);

  for (const point of Array.isArray(points) ? points : []) {
    const at = Number(point?.t);
    const count = Number(point?.n);
    if (!Number.isFinite(at) || !Number.isFinite(count) || count <= 0) continue;
    const key = align(at);
    if (key < first || key > last) continue;
    buckets.set(key, (buckets.get(key) || 0) + count);
  }

  return [...buckets.entries()].map(([t, n]) => ({ t, n }));
}

/** Totals used by the panel header and the "busiest bar" highlight. */
export function summarizeEvents(series) {
  let total = 0;
  let peak = null;
  for (const point of Array.isArray(series) ? series : []) {
    const count = Number(point?.n);
    if (!Number.isFinite(count) || count <= 0) continue;
    total += count;
    if (!peak || count > peak.n) peak = point;
  }
  const buckets = Array.isArray(series) ? series.length : 0;
  return {
    total,
    peak,
    average: buckets > 0 ? total / buckets : 0
  };
}
