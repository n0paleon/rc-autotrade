/**
 * Per-order latency marks used by the loot-log debugger.
 *
 * Every timestamp is an absolute epoch millisecond taken on the worker clock.
 * Durations are derived in the UI so a single order can be inspected from
 * several angles (signal → send, signal → response, signal → fulfillment).
 *
 * The object is stored as JSON in `orders.timings`; keep the shape small and
 * versioned so old rows keep loading after future additions.
 */

const TIME_KEYS = [
  "signalAt",
  "handleAt",
  "orderAt",
  "dispatchAt",
  "sendAt",
  "responseAt",
  "completeAt",
  "acceptedAt",
  "fillAt",
  "fillHandledAt",
  "failedAt"
];

const DURATION_KEYS = ["dnsMs"];

function finiteNumber(value) {
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function roundMs(value) {
  return Math.round(value * 1000) / 1000;
}

/**
 * Keeps only sane numeric marks. Returns null when there is nothing useful,
 * so old loot rows (which have no timings column) stay untouched.
 */
export function sanitizeTimings(raw) {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const out = { v: 1 };

  for (const key of TIME_KEYS) {
    const value = finiteNumber(raw[key]);
    if (value !== null && value > 0) out[key] = roundMs(value);
  }

  for (const key of DURATION_KEYS) {
    const value = finiteNumber(raw[key]);
    if (value !== null && value >= 0) out[key] = roundMs(value);
  }

  const attempts = finiteNumber(raw.attempts);
  if (attempts !== null && attempts > 0) out.attempts = Math.min(10, Math.trunc(attempts));
  if (raw.unauthorized === true) out.unauthorized = true;

  if (typeof raw.signalCmd === "string" && raw.signalCmd.trim()) {
    out.signalCmd = raw.signalCmd.trim().slice(0, 64);
  }
  if (typeof raw.dnsHost === "string" && raw.dnsHost.trim()) {
    out.dnsHost = raw.dnsHost.trim().slice(0, 128);
  }
  if (typeof raw.dnsMode === "string" && raw.dnsMode.trim()) {
    out.dnsMode = raw.dnsMode.trim().slice(0, 16);
  }
  if (typeof raw.dnsAddress === "string" && raw.dnsAddress.trim()) {
    out.dnsAddress = raw.dnsAddress.trim().slice(0, 64);
  }
  const family = finiteNumber(raw.dnsFamily);
  if (family === 4 || family === 6) out.dnsFamily = family;
  if (typeof raw.dnsError === "string" && raw.dnsError.trim()) {
    out.dnsError = raw.dnsError.trim().slice(0, 120);
  }

  const hasTime = TIME_KEYS.some((key) => Object.hasOwn(out, key));
  return hasTime ? out : null;
}

/** Serializes a timings object for the `orders.timings` TEXT column. */
export function encodeTimings(raw) {
  const timings = sanitizeTimings(raw);
  return timings ? JSON.stringify(timings) : null;
}

/** Reads the TEXT column (or an already-parsed object) back into a clean object. */
export function decodeTimings(value) {
  if (!value) return null;
  if (typeof value === "object") return sanitizeTimings(value);
  try {
    return sanitizeTimings(JSON.parse(value));
  } catch {
    return null;
  }
}
