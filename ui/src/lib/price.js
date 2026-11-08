/**
 * Price-history chart maths.
 *
 * The worker stores one close per minute plus the low/high band of the prices
 * that actually held during that minute. The deck fetches a longer window for
 * the 3d/7d periods and re-buckets client-side, aligning each boundary to the
 * viewer's local clock the same way `events.js` does for market activity.
 * Inside a bucket the close is the newest minute's close; the band is the
 * widest low/high any minute in that bucket reached.
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

function finite(value) {
  return Number.isFinite(Number(value)) && value !== null && value !== "";
}

/** Low edge of a point's band, falling back to its close. */
export function priceLow(point, fallback) {
  return point?.l != null && finite(point.l) ? Number(point.l) : fallback;
}

/** High edge of a point's band, falling back to its close. */
export function priceHigh(point, fallback) {
  return point?.h != null && finite(point.h) ? Number(point.h) : fallback;
}

/** Local-clock offset for an instant, in milliseconds east of UTC. */
export function timezoneOffsetMs(now = Date.now()) {
  const offset = new Date(now).getTimezoneOffset();
  return Number.isFinite(offset) ? -offset * MINUTE : 0;
}

/**
 * Keeps the newest close in each `bucketMs` bucket, clipped to the requested
 * window, and widens that bucket's low/high band over every minute it covers.
 * Quiet buckets are not invented: a price chart should connect the ticks it
 * actually has instead of drawing a flat line through a gap.
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
  const newest = new Map();
  for (const point of Array.isArray(points) ? points : []) {
    const at = Number(point?.t);
    const price = Number(point?.p);
    if (!Number.isFinite(at) || !Number.isFinite(price) || at < from || at > now) continue;
    const key = align(at);
    const low = Math.min(priceLow(point, price), price);
    const high = Math.max(priceHigh(point, price), price);
    const current = buckets.get(key);
    if (!current) {
      buckets.set(key, { t: key, p: price, l: low, h: high });
      newest.set(key, at);
      continue;
    }
    current.l = Math.min(current.l, low);
    current.h = Math.max(current.h, high);
    if (at >= newest.get(key)) {
      current.p = price;
      newest.set(key, at);
    }
  }
  return [...buckets.values()].sort((a, b) => a.t - b.t);
}

/**
 * Stands in for a missing bucket so a line chart can break instead of drawing a
 * straight segment across a quiet stretch. Recharts joins every point in a
 * numeric series; a single `p: null` marker tells it to stop the stroke there.
 * The band edges travel with the marker as nulls so the ranged area breaks too.
 */
export function breakPriceGaps(series, bucketMs) {
  const size = Number(bucketMs);
  const points = Array.isArray(series) ? series : [];
  if (!Number.isFinite(size) || size <= 0) return points;
  const out = [];
  let previous = null;
  for (const point of points) {
    const at = Number(point?.t);
    const price = Number(point?.p);
    if (!Number.isFinite(at) || !Number.isFinite(price)) continue;
    if (previous !== null && at - previous > size * 1.5) {
      out.push({ t: previous + size, p: null, l: null, h: null });
    }
    out.push({
      t: at,
      p: price,
      l: priceLow(point, price),
      h: priceHigh(point, price)
    });
    previous = at;
  }
  return out;
}

/**
 * Merges the fetched long-range series with the live 24h window. Live points
 * win on the same minute, so a polled snapshot never overwrites a fresh tick.
 * A live point that carries no band (the boot snapshot's history only has
 * closes) keeps the stored band and widens it to include the live close.
 */
export function mergePriceSeries(stored, live) {
  const merged = new Map();
  for (const point of Array.isArray(stored) ? stored : []) {
    const at = Number(point?.t);
    const price = Number(point?.p);
    if (!Number.isFinite(at) || !Number.isFinite(price)) continue;
    merged.set(at, {
      t: at,
      p: price,
      l: priceLow(point, price),
      h: priceHigh(point, price)
    });
  }
  for (const point of Array.isArray(live) ? live : []) {
    const at = Number(point?.t);
    const price = Number(point?.p);
    if (!Number.isFinite(at) || !Number.isFinite(price)) continue;
    const previous = merged.get(at);
    const low = priceLow(
      point,
      previous ? Math.min(previous.l, price) : price
    );
    const high = priceHigh(
      point,
      previous ? Math.max(previous.h, price) : price
    );
    merged.set(at, { t: at, p: price, l: Math.min(low, price), h: Math.max(high, price) });
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
    const bandLow = priceLow(point, point.p);
    const bandHigh = priceHigh(point, point.p);
    if (high === null || bandHigh > high) high = bandHigh;
    if (low === null || bandLow < low) low = bandLow;
  }
  return { first, last, change, high, low };
}
