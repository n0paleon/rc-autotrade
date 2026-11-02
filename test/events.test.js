import test from "node:test";
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
