/**
 * Latency tracker for worker → upstream (Rollercoin).
 *
 * Measures the passive round-trip of every real call and keeps a bounded
 * rolling sample per channel so the HUD/panel can show last / avg / p95,
 * min/max, jitter and a sparkline without persisting anything to disk.
 *
 * Channels:
 *   http         — purchase, item-info, session refresh (any RollercoinApi call)
 *   keepalive    — warm-probe GET (same URL as the keepalive service)
 *   wsHandshake  — market-feed WebSocket connect (socket construction → open)
 *   wsPing       — market-feed ping → pong round-trip
 *
 * Failures are recorded too: a failed call still consumed upstream time
 * (e.g. a 401 that took 800 ms is still 800 ms of latency).
 */

export const LATENCY_KEYS = ["http", "keepalive", "wsHandshake", "wsPing"];

export const LATENCY_LABELS = Object.freeze({
  http: "HTTP",
  keepalive: "KEEPALIVE",
  wsHandshake: "WS HANDSHAKE",
  wsPing: "WS PING"
});

const DEFAULT_CAPACITY = 120;
const MAX_CAPACITY = 600;

function clampSample(value) {
  const number = Number(value);
  return Number.isFinite(number) && number >= 0 ? Math.round(number * 10) / 10 : null;
}

function channelStats(bucket) {
  const samples = bucket.samples;
  const count = samples.length;
  if (count === 0) {
    return {
      count: 0,
      last: null,
      lastAt: null,
      lastOk: null,
      min: null,
      max: null,
      avg: null,
      p50: null,
      p95: null,
      jitter: null,
      errors: 0,
      lastError: null,
      recent: []
    };
  }
  const sorted = [...samples].sort((a, b) => a - b);
  const sum = sorted.reduce((total, value) => total + value, 0);
  const avg = sum / count;
  const variance = sorted.reduce((total, value) => total + (value - avg) ** 2, 0) / count;
  const round1 = (value) => Math.round(value * 10) / 10;
  return {
    count,
    last: samples[count - 1],
    lastAt: bucket.lastAt,
    lastOk: bucket.lastOk,
    min: round1(sorted[0]),
    max: round1(sorted[count - 1]),
    avg: round1(avg),
    p50: round1(sorted[Math.min(count - 1, Math.floor(0.5 * count))]),
    p95: round1(sorted[Math.min(count - 1, Math.floor(0.95 * count))]),
    jitter: round1(Math.sqrt(variance)),
    errors: bucket.errors,
    lastError: bucket.lastError,
    recent: samples.slice(-24)
  };
}

export class LatencyTracker {
  constructor({ capacity = DEFAULT_CAPACITY, now = () => Date.now() } = {}) {
    const cap = Math.floor(Number(capacity));
    this.capacity = Number.isInteger(cap) && cap > 0 ? Math.min(MAX_CAPACITY, cap) : DEFAULT_CAPACITY;
    this.now = now;
    this.channels = Object.fromEntries(
      LATENCY_KEYS.map((key) => [
        key,
        { samples: [], lastAt: null, lastOk: null, errors: 0, lastError: null }
      ])
    );
  }

  /** Record one completed sample. `ok === false` marks a failed call. */
  record(channel, ms, { ok = true, error = null } = {}) {
    if (!Object.hasOwn(this.channels, channel)) return false;
    const value = clampSample(ms);
    if (value === null) return false;
    const bucket = this.channels[channel];
    bucket.samples.push(value);
    if (bucket.samples.length > this.capacity) {
      bucket.samples.splice(0, bucket.samples.length - this.capacity);
    }
    bucket.lastAt = this.now();
    bucket.lastOk = Boolean(ok);
    if (!ok) {
      bucket.errors += 1;
      bucket.lastError = error ? String(error).slice(0, 160) : null;
    }
    return true;
  }

  /**
   * Wrap an async operation and record its duration on settle.
   * `okFn(result)` decides transport success; defaults to `result.ok !== false`,
   * so pass `{ okFn: () => true }` for raw fetch calls where an HTTP error
   * status is still a successful (measured) round-trip.
   */
  async measure(channel, fn, { okFn = (result) => result?.ok !== false } = {}) {
    const started = this.now();
    try {
      const result = await fn();
      const ok = okFn(result);
      this.record(channel, this.now() - started, { ok, error: ok ? null : result?.error });
      return result;
    } catch (error) {
      this.record(channel, this.now() - started, {
        ok: false,
        error: error?.message || String(error)
      });
      throw error;
    }
  }

  stats() {
    const out = {};
    for (const key of LATENCY_KEYS) out[key] = channelStats(this.channels[key]);
    return out;
  }

  /** Freshest recent sample across channels (for the HUD chip). */
  freshest() {
    let best = null;
    for (const key of LATENCY_KEYS) {
      const bucket = this.channels[key];
      if (bucket.lastAt === null) continue;
      if (!best || bucket.lastAt > best.bucket.lastAt) best = { key, bucket };
    }
    if (!best) return null;
    const samples = best.bucket.samples;
    return {
      channel: best.key,
      last: samples.length ? samples[samples.length - 1] : null,
      lastAt: best.bucket.lastAt,
      lastOk: best.bucket.lastOk
    };
  }

  snapshot() {
    return {
      channels: this.stats(),
      freshest: this.freshest(),
      measuredAt: this.now()
    };
  }

  reset() {
    for (const bucket of Object.values(this.channels)) {
      bucket.samples.length = 0;
      bucket.lastAt = null;
      bucket.lastOk = null;
      bucket.errors = 0;
      bucket.lastError = null;
    }
  }
}
