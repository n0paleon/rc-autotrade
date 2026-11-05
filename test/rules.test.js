import { test } from "bun:test";
import assert from "node:assert/strict";
import { createRateLimiter, deriveAutoTargetPrice, microToRlt, planOrder, roundPrice } from "../src/shared/rules.js";

const CONFIG = {
  itemId: "6571aa39ca8bfef34adcd9e9",
  targetPrice: 0.1,
  desiredQty: 10,
  highStockThreshold: 20,
  minAcceptableQty: 1,
  allowPartialOrder: true,
  active: true
};

test("converts micro-RLT and rounds prices", () => {
  assert.equal(microToRlt(100_000), 0.1);
  assert.equal(roundPrice(0.1234567), 0.123457);
});

test("derives the auto target from the allowed price floor", () => {
  assert.equal(deriveAutoTargetPrice(0.05, 5), 0.0525);
  assert.equal(deriveAutoTargetPrice(0.05, 0), 0.05);
  assert.equal(deriveAutoTargetPrice(0.1, 12.5), 0.1125);
  assert.equal(deriveAutoTargetPrice(0.0525, 5), 0.055125, "keeps six-decimal RLT precision");

  assert.equal(deriveAutoTargetPrice(0, 5), null, "no floor means no derived target");
  assert.equal(deriveAutoTargetPrice(-1, 5), null);
  assert.equal(deriveAutoTargetPrice(0.05, -1), null);
  assert.equal(deriveAutoTargetPrice(Number.NaN, 5), null);

  assert.equal(
    deriveAutoTargetPrice(1_000_000, 100),
    1_000_000,
    "derived targets stay under the regular price ceiling"
  );
});

test("planOrder accepts a runtime target override", () => {
  const plan = planOrder({ config: CONFIG, unitPrice: 0.11, available: 15, targetPrice: 0.12 });
  assert.equal(plan.action, "buy");
  assert.equal(plan.qty, 10);

  const skipped = planOrder({ config: CONFIG, unitPrice: 0.11, available: 100, targetPrice: 0.105 });
  assert.equal(skipped.action, "skip");
  assert.equal(skipped.reason, "price-above-target");
});

test("skips listings above the target", () => {
  const plan = planOrder({ config: CONFIG, unitPrice: 0.11, available: 100 });
  assert.equal(plan.action, "skip");
  assert.equal(plan.reason, "price-above-target");
});

test("buys the desired quantity at or below target", () => {
  const plan = planOrder({ config: CONFIG, unitPrice: 0.1, available: 15 });
  assert.equal(plan.action, "buy");
  assert.equal(plan.qty, 10);
  assert.equal(plan.doubled, false);
  assert.equal(plan.notional, 1);
});

test("doubles the order when the book is stacked", () => {
  const plan = planOrder({ config: CONFIG, unitPrice: 0.099, available: 20 });
  assert.equal(plan.action, "buy");
  assert.equal(plan.qty, 20);
  assert.equal(plan.doubled, true);
});

test("never buys more than is available", () => {
  const plan = planOrder({ config: CONFIG, unitPrice: 0.099, available: 12 });
  assert.equal(plan.qty, 10);
  const partial = planOrder({
    config: { ...CONFIG, desiredQty: 30, highStockThreshold: 60 },
    unitPrice: 0.099,
    available: 12
  });
  assert.equal(partial.action, "buy");
  assert.equal(partial.qty, 12);
});

test("respects partial-order rules", () => {
  const noPartial = planOrder({
    config: { ...CONFIG, allowPartialOrder: false },
    unitPrice: 0.05,
    available: 4
  });
  assert.equal(noPartial.action, "skip");
  assert.equal(noPartial.reason, "stock-below-minimum");

  const partial = planOrder({ config: { ...CONFIG, desiredQty: 50 }, unitPrice: 0.05, available: 4 });
  assert.equal(partial.action, "buy");
  assert.equal(partial.qty, 4);
});

test("respects pause, inactive and empty stock", () => {
  assert.equal(planOrder({ config: CONFIG, unitPrice: 0.05, available: 5, paused: true }).reason, "paused");
  assert.equal(planOrder({ config: { ...CONFIG, active: false }, unitPrice: 0.05, available: 5 }).reason, "inactive");
  assert.equal(planOrder({ config: CONFIG, unitPrice: 0.05, available: 0 }).reason, "no-stock");
});

test("rate limiter caps bursts inside the window", () => {
  let now = 1_000_000;
  const take = createRateLimiter({ maxPerMinute: 3, now: () => now });
  assert.equal(take(), true);
  assert.equal(take(), true);
  assert.equal(take(), true);
  assert.equal(take(), false);
  now += 60_001;
  assert.equal(take(), true);
});
