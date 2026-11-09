/**
 * Per-order timing helpers shared by the loot detail debugger.
 *
 * Every mark is an absolute epoch millisecond on the worker clock, so a span
 * is just `to - from`. The TTFB sample is the exception: `ttfbMs` is a
 * monotonic duration measured inside the HTTP client, so the first-byte
 * instant has to be reconstructed from the dispatch mark.
 */

function positive(value) {
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? number : null;
}

/**
 * Wall-clock instant of the purchase request's first response byte.
 *
 * Prefer the monotonic TTFB sample (`sendAt + ttfbMs`) instead of trusting a
 * wall-clock response mark. A 401 refresh keeps `sendAt` on the first dispatch
 * while `ttfbMs` belongs to the retried request, so those rows fall back to the
 * retry's own `responseAt`. Rows recorded before the TTFB sample existed have
 * no `ttfbMs` and keep the same fallback.
 */
export function ttfbAt(timings) {
  const responseAt = positive(timings?.responseAt);
  const attempts = Number(timings?.attempts) || 1;
  if (attempts > 1) return responseAt;

  const sendAt = positive(timings?.sendAt);
  const ttfbMs = Number(timings?.ttfbMs);
  if (sendAt != null && Number.isFinite(ttfbMs) && ttfbMs >= 0) {
    return sendAt + ttfbMs;
  }
  return responseAt;
}
