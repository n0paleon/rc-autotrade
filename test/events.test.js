import { test } from "bun:test";
import assert from "node:assert/strict";
import { normalizeUpstreamEvent } from "../src/worker/events.js";

test("normalises a marketplace update and sorts listings", () => {
  const event = normalizeUpstreamEvent({
    cmd: "marketplace_orders_update",
    value: {
      item_id: "6571AA39CA8BFEF34ADCD9E9",
      currency: "RLT",
      item_type: "mutation_component",
      data: {
        list: [
          { price: 120_000, quantity: 4 },
          { price: 100_000, quantity: 7 },
          { price: 0, quantity: 9 }
        ]
      }
    }
  });

  assert.equal(event.kind, "market");
  assert.equal(event.itemId, "6571aa39ca8bfef34adcd9e9");
  assert.equal(event.currency, "RLT");
  assert.equal(event.listings.length, 2);
  assert.equal(event.listings[0].unitPrice, 0.1);
  assert.equal(event.listings[0].quantity, 7);
});

test("normalises the allowed price band from a marketplace update", () => {
  const event = normalizeUpstreamEvent({
    cmd: "marketplace_orders_update",
    value: {
      currency: "RLT",
      item_type: "rack",
      item_id: "64490fc1547cfab9a2759638",
      data: {
        allowedPriceMin: 367_500,
        allowedPriceMax: 666_225_000,
        list: [{ price: 735_000, quantity: 7 }]
      }
    }
  });

  assert.equal(event.kind, "market");
  assert.equal(event.allowedPriceMin, 0.3675);
  assert.equal(event.allowedPriceMax, 666.225);
});

test("leaves the allowed price band null when the frame omits it", () => {
  const event = normalizeUpstreamEvent({
    cmd: "marketplace_orders_update",
    value: {
      item_id: "64490fc1547cfab9a2759638",
      currency: "RLT",
      data: { list: [{ price: 735_000, quantity: 7 }] }
    }
  });

  assert.equal(event.allowedPriceMin, null);
  assert.equal(event.allowedPriceMax, null);
});

test("normalises a fill with a total price", () => {
  const event = normalizeUpstreamEvent({
    cmd: "marketplace_orders_filled",
    value: {
      item_id: "abc12345",
      order_id: "order-7",
      count: 5,
      price: 500_000,
      status: 12
    }
  });

  assert.equal(event.kind, "fill");
  assert.equal(event.fill.orderId, "order-7");
  assert.equal(event.fill.count, 5);
  assert.equal(event.fill.unitPrice, 0.1);
  assert.equal(event.fill.status, 12);
});

test("ignores frames without a market or fill shape", () => {
  const event = normalizeUpstreamEvent({ cmd: "notification", value: { message: "hello" } });
  assert.equal(event.kind, "other");
  assert.equal(event.itemId, null);
  assert.deepEqual(event.listings, []);
  assert.equal(event.fill, null);
});

test("normalises the aggregated tail of the book", () => {
  const event = normalizeUpstreamEvent({
    cmd: "marketplace_orders_update",
    value: {
      currency: "RLT",
      item_type: "rack",
      item_id: "6a7b40a8f188bab31b1c2195",
      data: {
        list: [
          { price: 3_150_000, quantity: 4 },
          { price: 3_222_000, quantity: 1 }
        ],
        rest: { price: 3_360_000, quantity: 223 },
        totalCount: 228
      }
    }
  });

  assert.equal(event.kind, "market");
  assert.equal(event.rest.unitPrice, 3.36);
  assert.equal(event.rest.quantity, 223);
  assert.equal(event.totalCount, 228);
});

test("leaves the tail null when the frame has no rest block", () => {
  const event = normalizeUpstreamEvent({
    cmd: "marketplace_orders_update",
    value: { item_id: "abc12345", data: { list: [{ price: 100_000, quantity: 2 }] } }
  });
  assert.equal(event.rest, null);
  assert.equal(event.totalCount, null);
});
