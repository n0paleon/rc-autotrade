import { test } from "bun:test";
import assert from "node:assert/strict";
import { estimateFillPrice } from "../src/shared/depth.js";

const BOOK = [
  { unitPrice: 2.9444, quantity: 1 },
  { unitPrice: 3.024, quantity: 1 },
  { unitPrice: 3.0345, quantity: 4 },
  { unitPrice: 3.045, quantity: 88 },
  { unitPrice: 3.0975, quantity: 40 }
];
const TAIL = { unitPrice: 3.129, quantity: 268 };

test("a quantity-1 cheap ask barely moves a size-25 fill", () => {
  const fill = estimateFillPrice({ listings: BOOK, rest: TAIL, qty: 25 });
  assert.ok(fill);
  assert.equal(fill.filled, 25);
  assert.equal(fill.partial, false);
  assert.equal(fill.approx, false);
  assert.ok(Math.abs(fill.price - 3.038456) < 1e-6, `got ${fill.price}`);
});

test("a size-1 order still fills at the cheapest ask", () => {
  const fill = estimateFillPrice({ listings: BOOK, rest: TAIL, qty: 1 });
  assert.equal(fill.price, 2.9444);
  assert.equal(fill.filled, 1);
});

test("larger sizes walk deeper and use the aggregated tail", () => {
  const fill = estimateFillPrice({ listings: BOOK, rest: TAIL, qty: 500 });
  assert.equal(fill.filled, 402);
  assert.equal(fill.partial, true);
  assert.equal(fill.approx, true);
  assert.ok(Math.abs(fill.price - 3.105817) < 1e-6, `got ${fill.price}`);
});

test("unsorted levels are sorted and empty levels are ignored", () => {
  const fill = estimateFillPrice({
    listings: [
      { unitPrice: 3.045, quantity: 88 },
      { unitPrice: 0, quantity: 10 },
      { unitPrice: 2.9444, quantity: 0 },
      { unitPrice: 2.9444, quantity: 1 }
    ],
    qty: 2
  });
  assert.equal(fill.filled, 2);
  assert.ok(Math.abs(fill.price - 2.9947) < 1e-6, `got ${fill.price}`);
});

test("an empty book has no fill price", () => {
  assert.equal(estimateFillPrice({ listings: [], rest: null, qty: 10 }), null);
  assert.equal(estimateFillPrice({ listings: null, qty: 10 }), null);
});

test("the requested size defaults to one unit", () => {
  const fill = estimateFillPrice({ listings: BOOK });
  assert.equal(fill.requested, 1);
  assert.equal(fill.filled, 1);
});
