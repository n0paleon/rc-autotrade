import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  formatBytes,
  formatPercent,
  formatPreciseMs,
  itemTypeLabel,
  itemTypeOf,
  ITEM_TYPE_ORDER,
  marketplaceUrl,
  mergeHistory,
  runtimeHot,
  thresholdClass,
  utilizationClass
} from "../src/lib/format.js";

test("maps every itemType id to a human readable label", () => {
  assert.equal(itemTypeLabel("miner"), "miners");
  assert.equal(itemTypeLabel("mutation_component"), "parts");
  assert.equal(itemTypeLabel("battery"), "batteries");
  assert.equal(itemTypeLabel("rack"), "racks");
  assert.equal(itemTypeLabel("utility_item"), "other");
  assert.deepEqual(
    [...ITEM_TYPE_ORDER],
    ["miner", "mutation_component", "battery", "rack", "utility_item"]
  );
});

test("falls back to a readable label for unknown item types", () => {
  assert.equal(itemTypeLabel("seed_pack"), "seed pack");
  assert.equal(itemTypeLabel(""), "other");
});

test("resolves the item type from the config or its runtime", () => {
  assert.equal(itemTypeOf({ itemType: "RACK" }), "rack");
  assert.equal(itemTypeOf({ runtime: { itemType: "Miner" } }), "miner");
});

test("runtimeHot compares the market price against the saved target", () => {
  const config = {
    itemId: "x",
    active: true,
    targetPrice: 0.1,
    minAcceptableQty: 1,
    runtime: { lastPrice: 0.1, lastStock: 5 }
  };
  assert.equal(runtimeHot(config), true);
  assert.equal(runtimeHot({ ...config, runtime: { ...config.runtime, lastPrice: 0.11 } }), false);
  assert.equal(runtimeHot({ ...config, active: false }), false);
  assert.equal(runtimeHot({ ...config, runtime: { ...config.runtime, lastStock: 0 } }), false);
});

test("builds the Rollercoin marketplace URL for an item", () => {
  assert.equal(
    marketplaceUrl("mutation_component", "6319f81fa8ce530569eee9dd"),
    "https://rollercoin.com/marketplace/buy/mutation_component/6319f81fa8ce530569eee9dd"
  );
  assert.equal(
    marketplaceUrl("", "abc123"),
    "https://rollercoin.com/marketplace/buy/mutation_component/abc123"
  );
});

test("formats byte sizes with binary units and null-safety", () => {
  assert.equal(formatBytes(0), "0 B");
  assert.equal(formatBytes(512), "512 B");
  assert.equal(formatBytes(1024), "1.0 KB");
  assert.equal(formatBytes(1536), "1.5 KB");
  assert.equal(formatBytes(48.4 * 1024 * 1024), "48.4 MB");
  assert.equal(formatBytes(150 * 1024 * 1024), "150 MB");
  assert.equal(formatBytes(2.5 * 1024 * 1024 * 1024, 2), "2.50 GB");
  assert.equal(formatBytes(null), "—");
  assert.equal(formatBytes(Number.NaN), "—");
  assert.equal(formatBytes(-1), "—");
});

test("formats percentages and colors utilization brackets", () => {
  assert.equal(formatPercent(0), "0%");
  assert.equal(formatPercent(42.46, 1), "42.5%");
  assert.equal(formatPercent(null), "—");
  assert.equal(formatPercent(undefined), "—");
  assert.equal(formatPercent(Number.NaN), "—");

  assert.equal(utilizationClass(10), "text-signal");
  assert.equal(utilizationClass(85), "text-warn");
  assert.equal(utilizationClass(97), "text-danger");
  assert.equal(utilizationClass(null), "text-muted");
  assert.equal(thresholdClass(50, 80, 92), "text-signal");
  assert.equal(thresholdClass(85, 80, 92), "text-warn");
  assert.equal(thresholdClass(95, 80, 92), "text-danger");
  assert.equal(thresholdClass(undefined, 80, 92), "text-muted");
});

test("formatPreciseMs keeps sub-millisecond SQLite timings visible", () => {
  assert.equal(formatPreciseMs(0), "0.00ms");
  assert.equal(formatPreciseMs(0.42), "0.42ms");
  assert.equal(formatPreciseMs(9.876), "9.88ms");
  assert.equal(formatPreciseMs(42.4), "42ms");
  assert.equal(formatPreciseMs(1200), "1.20s");
  assert.equal(formatPreciseMs(null), "—");
  assert.equal(formatPreciseMs(Number.NaN), "—");
  assert.equal(formatPreciseMs(-1), "—");
});

test("mergeHistory carries the live minute bands and never narrows them", () => {
  const first = mergeHistory([], 0.1, 0, 0.09, 0.11);
  assert.deepEqual(first, [
    { t: 0, p: 0.1, l: 0.09, h: 0.11, rl: 0.09, rh: 0.11 }
  ]);

  const next = mergeHistory(first, 0.12, 60_000, 0.11, 0.13);
  assert.deepEqual(next, [
    { t: 0, p: 0.1, l: 0.09, h: 0.11, rl: 0.09, rh: 0.11 },
    { t: 60_000, p: 0.12, l: 0.11, h: 0.13, rl: 0.11, rh: 0.13 }
  ]);

  // A close-only tick widens the bucket's bands to include its own price and
  // preserves the raw edges it already had.
  const widened = mergeHistory(
    [{ t: 60_000, p: 0.12, l: 0.11, h: 0.13, rl: 0.10, rh: 0.135 }],
    0.14,
    60_000
  );
  assert.deepEqual(widened, [
    { t: 60_000, p: 0.14, l: 0.11, h: 0.14, rl: 0.10, rh: 0.14 }
  ]);
});
