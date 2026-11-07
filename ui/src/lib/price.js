/**
 * Price-history chart maths.
 *
 * The worker stores one close per minute. The deck fetches a longer window for
 * the 3d/7d periods and re-buckets client-side, aligning each boundary to the
 * viewer's local clock the same way `events.js` does for market activity.
 * Inside a bucket the close is the last tick, not an average, so the line keeps
 * reading as "the price at that moment".
 */

export const PRICE_PERIODS = Object.freeze([
  { key: "1", label: "1D", days: 1 },
  { key: "3", label: "3D", days: 3 },
  { key: "7", label: "7D", days: 7 }
]);

export const PRICE_BUCKETS = Object.freeze({
  "1m": { label: "1M", ms: 60_000, minutes: 1 },
  "5m": { label: "5M", ms: 5 * 60_000, minutes: 5 },
  "30m": { label: "30M", ms: 30 * 60_000, minutes: 30 },
  "1h": { label: "1H", ms: 60 * 60_000, minutes: 60 },
  "4h": { label: "4H", ms: 4 * 60 * 60_000, minutes: 240 },
  "1d": { label: "1D", ms: 86_400_000, minutes: 1_440 }
});

export const PRICE_DEFAULT_PERIOD = "1";
export const PRICE_DEFAULT_BUCKET = "5m";

const MINUTE = 60_000;
const DAY = 86_400_000;

/** Local-clock offset for an instant, in milliseconds east of UTC. */
export function timezoneOffsetMs(now = Date.now()) {
  const offset = new Date(now).getTimezoneOffset();
  return Number.isFinite(offset) ? -offset * MINUTE : 0;
}

/**
 * Keeps the newest close in each `bucketMs` bucket, clipped to the requested
 * window. Quiet buckets are not invented: a price chart should connect the
 * ticks it actually has instead of drawing a flat line through a gap.
 */
export function bucketPrice(
  points,
  { days = 1, bucketMs = 5 * MINUTE, now = Date.now(), tzOffsetMs } = {}
) {
  const size = Number(bucketMs);
  if (!Number.isFinite(size) || size <= 0) return [];
  const span = Math.max(1, Number(days) || 1) * DAY;
  const offset = Number.isFinite(tzOffsetMs) ? Number(tzOffsetMs) : timezoneOffsetMs(now);
  const align = (at) => Math.floor((at + offset) / size) * size - offset;

  const from = now - span;
  const buckets = new Map();
  for (const point of Array.isArray(points) ? points : []) {
    const at = Number(point?.t);
    const price = Number(point?.p);
    if (!Number.isFinite(at) || !Number.isFinite(price) || at < from || at > now) continue;
    const key = align(at);
    const current = buckets.get(key);
    if (!current || at >= current.t) buckets.set(key, { t: key, p: price });
  }
  return [...buckets.values()].sort((a, b) => a.t - b.t);
}

/**
 * Merges the fetched long-range series with the live 24h window. Live points
 * win on the same minute, so a polled snapshot never overwrites a fresh tick.
 */
export function mergePriceSeries(stored, live) {
  const merged = new Map();
  for (const point of Array.isArray(stored) ? stored : []) {
    const at = Number(point?.t);
    const price = Number(point?.p);
    if (!Number.isFinite(at) || !Number.isFinite(price)) continue;
    merged.set(at, { t: at, p: price });
  }
  for (const point of Array.isArray(live) ? live : []) {
    const at = Number(point?.t);
    const price = Number(point?.p);
    if (!Number.isFinite(at) || !Number.isFinite(price)) continue;
    merged.set(at, { t: at, p: price });
  }
  return [...merged.values()].sort((a, b) => a.t - b.t);
}

/** First/last readings used by the header change badge and the y-domain. */
export function summarizePrice(series) {
  const points = Array.isArray(series) ? series : [];
  const first = points[0] || null;
  const last = points[points.length - 1] || null;
  const change =
    first && last && first.p > 0 ? ((last.p - first.p) / first.p) * 100 : null;
  let high = null;
  let low = null;
  for (const point of points) {
    if (!Number.isFinite(point?.p)) continue;
    if (high === null || point.p > high) high = point.p;
    if (low === null || point.p < low) low = point.p;
  }
  return { first, last, change, high, low };
}
