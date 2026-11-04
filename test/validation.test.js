import test from "node:test";
import assert from "node:assert/strict";
import {
  normalizeItemId,
  validateSessionInput,
  validateSettings,
  validateTarget,
  validateTargets
} from "../src/shared/validation.js";

const SAMPLE = {
  itemId: "6571aa39ca8bfef34adcd9e9",
  targetPrice: 0.1,
  desiredQty: 10,
  highStockThreshold: 20,
  minAcceptableQty: 1
};

test("accepts the documented sample target", () => {
  const result = validateTarget(SAMPLE);
  assert.equal(result.ok, true);
  assert.deepEqual(result.value, {
    ...SAMPLE,
    itemType: "mutation_component",
    label: "",
    allowPartialOrder: true,
    active: true
  });
});

test("extracts an item id from a marketplace URL", () => {
  assert.equal(
    normalizeItemId("https://rollercoin.com/marketplace/mutation-components/6571aa39ca8bfef34adcd9e9?ref=x"),
    "6571aa39ca8bfef34adcd9e9"
  );
  assert.equal(normalizeItemId("  abort_test-01  "), "abort_test-01");
  assert.equal(normalizeItemId("bad id"), null);
});

test("rejects contradictory thresholds and quantities", () => {
  const result = validateTarget({ ...SAMPLE, highStockThreshold: 5, minAcceptableQty: 11 });
  assert.equal(result.ok, false);
  assert.match(result.errors.highStockThreshold, /greater than or equal/);
  assert.match(result.errors.minAcceptableQty, /cannot exceed/);
});

test("rejects over-precise and out-of-range prices", () => {
  assert.equal(validateTarget({ ...SAMPLE, targetPrice: 0.1234567 }).ok, false);
  assert.equal(validateTarget({ ...SAMPLE, targetPrice: 0 }).ok, false);
  assert.equal(validateTarget({ ...SAMPLE, targetPrice: 2_000_000 }).ok, false);
});

test("rejects duplicate ids when adding", () => {
  const result = validateTarget(SAMPLE, { existingIds: [SAMPLE.itemId] });
  assert.equal(result.ok, false);
  assert.match(result.errors.itemId, /already on the target list/);
});

test("allows the same id when editing it", () => {
  const result = validateTarget(SAMPLE, { existingIds: [SAMPLE.itemId], originalId: SAMPLE.itemId });
  assert.equal(result.ok, true);
});

test("validates a full target list and catches duplicates", () => {
  const result = validateTargets([SAMPLE, SAMPLE]);
  assert.equal(result.ok, false);
  assert.ok(result.errors["1.itemId"]);
  assert.equal(result.values.length, 1);
});

test("normalises labels and booleans", () => {
  const result = validateTarget({
    ...SAMPLE,
    label: "  Neon\nShard  ",
    active: false,
    allowPartialOrder: false
  });
  assert.equal(result.ok, true);
  assert.equal(result.value.label, "NeonShard");
  assert.equal(result.value.active, false);
  assert.equal(result.value.allowPartialOrder, false);
});

test("settings stay inside safe bounds", () => {
  assert.equal(validateSettings({ delayPerOrder: 49 }).ok, false);
  assert.equal(validateSettings({ maxOrdersPerMinute: 601 }).ok, false);
  assert.equal(validateSettings({ dryRun: "yes" }).ok, false);
  assert.deepEqual(validateSettings({ delayPerOrder: 500, dryRun: true }).value, {
    delayPerOrder: 500,
    dryRun: true
  });
});

test("session input strips a bearer prefix and ignores empty refresh", () => {
  const result = validateSessionInput({
    accessToken: "Bearer abcdefghijklmnop",
    refreshToken: "",
    cookie: "cf_clearance=abc"
  });
  assert.equal(result.ok, true);
  assert.equal(result.value.accessToken, "abcdefghijklmnop");
  assert.equal(result.value.refreshToken, "");
  assert.equal(result.value.cookie, "cf_clearance=abc");
});

test("session input accepts explicit clear flags", () => {
  const result = validateSessionInput({
    clearAccessToken: true,
    clearRefreshToken: true,
    clearCookie: true
  });
  assert.equal(result.ok, true);
  assert.deepEqual(result.value, {
    clearAccessToken: true,
    clearRefreshToken: true,
    clearCookie: true
  });

  assert.equal(validateSessionInput({ clearAccessToken: "yes" }).ok, false);
  assert.equal(validateSessionInput({ clearCookie: 1 }).ok, false);
});
