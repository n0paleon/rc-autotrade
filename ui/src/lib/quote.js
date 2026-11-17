/**
 * Live-quote helpers for the target form.
 *
 * The worker pushes a compact `quote` frame while a form watches one item:
 * `{ itemId, price, allowedPriceMin, allowedPriceMax, stock, itemType, at }`.
 * These helpers keep the browser-side merge rules testable without React:
 * ids are case-insensitive, the allowed band sticks across frames that omit it
 * (the worker only writes it when the upstream payload carries it), and a
 * malformed frame is dropped instead of blanking a good readout.
 */

import { normalizeItemId } from "./format.js";

/** A quote stops wearing the live badge after this long without a frame. */
export const QUOTE_FRESH_MS = 5_000;

export function quoteKey(itemId) {
  return normalizeItemId(itemId).toLowerCase();
}

function priceOr(value, fallback) {
  if (value === null || value === undefined || value === "") return fallback ?? null;
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback ?? null;
}

/**
 * Folds one socket frame into the quote map.
 *
 * Returns the previous map unchanged when the frame has no usable item id or
 * price, so a partial payload cannot hide the last good quote.
 */
export function mergeQuote(previous, data, receivedAt = Date.now()) {
  const key = quoteKey(data?.itemId);
  const price = priceOr(data?.price, null);
  if (!key || price === null) return previous || {};
  const prior = previous?.[key];
  return {
    ...(previous || {}),
    [key]: {
      ...data,
      itemId: key,
      price,
      stock: priceOr(data?.stock, prior?.stock),
      allowedPriceMin: priceOr(data?.allowedPriceMin, prior?.allowedPriceMin),
      allowedPriceMax: priceOr(data?.allowedPriceMax, prior?.allowedPriceMax),
      receivedAt
    }
  };
}

/** The newest quote for an item, or `null` when the worker has not sent one. */
export function quoteFor(quotes, itemId) {
  const key = quoteKey(itemId);
  return key ? quotes?.[key] || null : null;
}

/** True while a quote is recent enough to present as live. */
export function isQuoteFresh(quote, now = Date.now(), maxAgeMs = QUOTE_FRESH_MS) {
  return Boolean(
    quote &&
      Number.isFinite(quote.receivedAt) &&
      now - quote.receivedAt >= 0 &&
      now - quote.receivedAt <= maxAgeMs
  );
}
