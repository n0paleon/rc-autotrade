import { LIMITS, PRICE_SCALE } from "./constants.js";

export function roundPrice(value) {
  return Math.round((Number(value) + Number.EPSILON) * PRICE_SCALE) / PRICE_SCALE;
}

/**
 * Converts a raw micro-RLT integer (as sent by Rollercoin) to RLT.
 */
export function microToRlt(raw) {
  const value = Number(raw);
  return Number.isFinite(value) ? value / PRICE_SCALE : NaN;
}

/**
 * Pure decision function that mirrors (and hardens) the original trade.js flow.
 *
 * @param {object} args
 * @param {object} args.config target configuration
 * @param {number} args.unitPrice cheapest listing price in RLT
 * @param {number} args.available quantity available on the cheapest listing
 * @param {boolean} [args.paused] global trading pause
 * @returns {{
 *   action: "buy" | "skip",
 *   reason?: string,
 *   qty?: number,
 *   doubled?: boolean,
 *   notional?: number
 * }}
 */
export function planOrder({ config, unitPrice, available, paused = false }) {
  if (paused) return { action: "skip", reason: "paused" };
  if (!config?.active) return { action: "skip", reason: "inactive" };
  if (!Number.isFinite(unitPrice) || unitPrice <= 0) {
    return { action: "skip", reason: "invalid-price" };
  }
  if (!Number.isFinite(available) || available < 1) {
    return { action: "skip", reason: "no-stock" };
  }
  if (unitPrice > config.targetPrice + 1e-9) {
    return { action: "skip", reason: "price-above-target" };
  }

  let qty = 0;
  let doubled = false;

  if (available >= config.desiredQty) {
    qty = config.desiredQty;
    // The original script doubles the desired qty when the market is stacked.
    if (available >= config.highStockThreshold) {
      qty *= 2;
      doubled = true;
    }
  } else if (config.allowPartialOrder && available >= config.minAcceptableQty) {
    qty = available;
  } else {
    return { action: "skip", reason: "stock-below-minimum" };
  }

  qty = Math.max(1, Math.min(Math.floor(qty), Math.floor(available), LIMITS.qtyMax));

  return {
    action: "buy",
    qty,
    doubled,
    notional: roundPrice(unitPrice * qty)
  };
}

/**
 * Small fixed-window guard so a burst of socket frames cannot turn into an
 * order storm. Returns true when the order is allowed.
 */
export function createRateLimiter({ maxPerMinute = 60, now = () => Date.now() } = {}) {
  const stamps = [];
  return function take() {
    const at = now();
    const cutoff = at - 60_000;
    while (stamps.length > 0 && stamps[0] < cutoff) stamps.shift();
    if (stamps.length >= maxPerMinute) return false;
    stamps.push(at);
    return true;
  };
}
