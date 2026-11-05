/**
 * Depth-aware price estimation.
 *
 * The marketplace frame carries a short ladder of asks plus an aggregated
 * `rest` tail. The cheapest ask alone is a poor market reference: a single
 * quantity-1 listing can sit far below a deep level that is the only place a
 * real order would fill.
 *
 * `estimateFillPrice` answers a narrower, more useful question: what would one
 * unit cost, on average, if this order size were filled from the visible
 * ladder? A 1-unit listing therefore contributes 1/q of the average instead of
 * defining the whole price. When the requested size runs past the visible
 * ladder, `rest` is consumed as one aggregate level and the result is flagged
 * as approximate, because upstream only exposes its floor, not its shape.
 */

function positive(value) {
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? number : null;
}

/** Normalises a listing into `{ price, quantity }`; accepts raw or normalised frames. */
function normaliseLevel(level) {
  if (!level || typeof level !== "object") return null;
  const price = positive(level.unitPrice ?? level.price);
  const quantity = positive(level.quantity ?? level.qty ?? level.count);
  if (price == null || quantity == null) return null;
  return { price, quantity: Math.floor(quantity) };
}

/**
 * @param {object} args
 * @param {Array<{unitPrice?:number, price?:number, quantity:number}>} args.listings
 * @param {{unitPrice?:number, price?:number, quantity:number}|null} [args.rest]
 * @param {number} args.qty desired order size in units
 * @returns {null | {
 *   price: number,
 *   filled: number,
 *   requested: number,
 *   partial: boolean,
 *   approx: boolean,
 *   levels: number
 * }}
 */
export function estimateFillPrice({ listings, rest = null, qty = 1 } = {}) {
  const levels = (Array.isArray(listings) ? listings : [])
    .map(normaliseLevel)
    .filter(Boolean)
    .sort((a, b) => a.price - b.price || b.quantity - a.quantity);

  const requested = Math.max(1, Math.floor(Number(qty) || 1));
  let remaining = requested;
  let cost = 0;
  let filled = 0;

  for (const level of levels) {
    if (remaining <= 0) break;
    const take = Math.min(remaining, level.quantity);
    cost += take * level.price;
    filled += take;
    remaining -= take;
  }

  let approx = false;
  if (remaining > 0) {
    const tail = normaliseLevel(rest);
    if (tail) {
      const take = Math.min(remaining, tail.quantity);
      cost += take * tail.price;
      filled += take;
      remaining -= take;
      approx = true;
    }
  }

  if (filled <= 0 || cost <= 0) return null;

  return {
    price: cost / filled,
    filled,
    requested,
    partial: remaining > 0,
    approx,
    levels: levels.length
  };
}
