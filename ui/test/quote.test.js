import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  isQuoteFresh,
  mergeQuote,
  QUOTE_FRESH_MS,
  quoteFor,
  quoteKey
} from "../src/lib/quote.js";

test("merges a live quote under a case-insensitive item id", () => {
  const quotes = mergeQuote(
    {},
    {
      itemId: "64490FC1547CFAB9A2759638",
      price: "0.25",
      stock: 7,
      allowedPriceMin: 0.1,
      allowedPriceMax: 0.9,
      itemType: "rack",
      at: 1_000
    },
    2_000
  );

  assert.equal(quoteKey("64490fc1547cfab9a2759638"), "64490fc1547cfab9a2759638");
  const quote = quoteFor(quotes, "https://rollercoin.com/marketplace/buy/rack/64490FC1547CFAB9A2759638");
  assert.equal(quote.price, 0.25);
  assert.equal(quote.stock, 7);
  assert.equal(quote.receivedAt, 2_000);
});

test("keeps the last allowed band when a frame omits it", () => {
  const first = mergeQuote(
    {},
    {
      itemId: "abc123",
      price: 0.25,
      allowedPriceMin: 0.1,
      allowedPriceMax: 0.9
    },
    1_000
  );
  const next = mergeQuote(first, { itemId: "ABC123", price: 0.3 }, 2_000);

  assert.equal(quoteFor(next, "abc123").price, 0.3);
  assert.equal(quoteFor(next, "abc123").allowedPriceMin, 0.1);
  assert.equal(quoteFor(next, "abc123").allowedPriceMax, 0.9);
});

test("ignores frames without an id or a usable price", () => {
  const original = mergeQuote({}, { itemId: "abc123", price: 0.25 }, 1_000);
  assert.equal(mergeQuote(original, { itemId: "", price: 1 }, 2_000), original);
  assert.equal(mergeQuote(original, { itemId: "abc123" }, 2_000), original);
  assert.equal(mergeQuote(original, { itemId: "abc123", price: "NaN" }, 2_000), original);
  assert.deepEqual(mergeQuote(undefined, null, 2_000), {});
});

test("marks quotes stale once the freshness window closes", () => {
  const quote = { receivedAt: 1_000 };
  assert.equal(isQuoteFresh(quote, 1_000 + QUOTE_FRESH_MS), true);
  assert.equal(isQuoteFresh(quote, 1_001 + QUOTE_FRESH_MS), false);
  assert.equal(isQuoteFresh(null, 1_000), false);
  assert.equal(isQuoteFresh({}, 1_000), false);
});
