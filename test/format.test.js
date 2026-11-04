import test from "node:test";
import assert from "node:assert/strict";

import {
  formatBytes,
  formatPercent,
  itemTypeLabel,
  itemTypeOf,
  ITEM_TYPE_ORDER,
  marketplaceUrl,
  thresholdClass,
  utilizationClass
} from "../ui/src/lib/format.js";

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
