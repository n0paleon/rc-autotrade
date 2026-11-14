import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  applyTotalPriceMarkup,
  formatBytes,
  formatPercent,
  formatPreciseMs,
  itemTypeLabel,
  itemTypeOf,
  ITEM_TYPE_ORDER,
  marketplaceUrl,
  matchesLootFilter,
  mergeHistory,
  runtimeEffectiveStock,
  runtimeHot,
  runtimeReconciling,
  thresholdClass,
  utilizationClass
} from "../src/lib/format.js";

test("applies the total price markup used by the target preview", () => {
  // 0% is the exact listing total.
  assert.equal(applyTotalPriceMarkup(1, 0), 1);
  assert.equal(applyTotalPriceMarkup(0.9, 0), 0.9);
  // 10% turns 1.000000 RLT into the 1.100000 RLT request cap.
  assert.equal(applyTotalPriceMarkup(1, 10), 1.1);
  assert.equal(applyTotalPriceMarkup(0.9, 10), 0.99);
  // Worst-case max spend: target 0.1 × 20 qty × 2 doubling × 1.1.
  assert.equal(applyTotalPriceMarkup(0.1 * 20 * 2, 10), 4.4);
  // Invalid and non-positive inputs are ignored, matching the worker.
  assert.equal(applyTotalPriceMarkup(1, -5), 1);
  assert.equal(applyTotalPriceMarkup(1, Number.NaN), 1);
  assert.equal(applyTotalPriceMarkup(0, 10), 0);
});

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

test("runtimeHot follows the stock the worker may actually order", () => {
  const config = {
    itemId: "x",
    active: true,
    targetPrice: 0.1,
    minAcceptableQty: 1,
    runtime: { lastPrice: 0.09, lastStock: 5 }
  };
  // A stale frame still advertises the pre-purchase depth. While the worker is
  // reconciling, decisions use the local remainder; the deck must not light up
  // a buy the engine would refuse.
  assert.equal(
    runtimeHot({ ...config, runtime: { ...config.runtime, effectiveStock: 0 } }),
    false
  );
  assert.equal(
    runtimeHot({ ...config, runtime: { ...config.runtime, reconcile: { floor: 0 } } }),
    false
  );
  assert.equal(
    runtimeHot({
      ...config,
      runtime: { ...config.runtime, effectiveStock: 2, lastStock: 2 }
    }),
    true
  );
});

test("runtimeReconciling reads the tick flag and the snapshot deadline", () => {
  assert.equal(runtimeReconciling({ runtime: { reconciling: true } }), true);
  assert.equal(runtimeReconciling({ runtime: { reconciling: false } }), false);
  assert.equal(runtimeReconciling({}), false);
  // A hydrated snapshot carries the window deadline instead of the flag, and it
  // must expire on its own so a quiet feed cannot leave the badge on forever.
  assert.equal(
    runtimeReconciling({ runtime: { reconcile: { until: Date.now() + 60_000 } } }),
    true
  );
  assert.equal(
    runtimeReconciling({ runtime: { reconcile: { until: Date.now() - 1 } } }),
    false
  );
});

test("runtimeEffectiveStock prefers the tick over the snapshot floor", () => {
  assert.equal(runtimeEffectiveStock({ runtime: { effectiveStock: 1 } }), 1);
  assert.equal(runtimeEffectiveStock({ runtime: { effectiveStock: 0 } }), 0);
  assert.equal(runtimeEffectiveStock({ runtime: { reconcile: { floor: 4 } } }), 4);
  assert.equal(runtimeEffectiveStock({ runtime: {} }), null);
  assert.equal(runtimeEffectiveStock({}), null);
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

test("matchesLootFilter mirrors the server-side /api/history filter rules", () => {
  const filled = { label: "ASIC Miner X9", itemId: "abc123", status: "filled", profitRlt: 0.5 };
  const failed = { label: "ASIC Miner X9", itemId: "abc123", status: "failed", profitRlt: 0 };
  const loss = { label: "Fan Kit", itemId: "def456", status: "filled", profitRlt: -0.25 };

  // Empty criteria match everything.
  assert.equal(matchesLootFilter(filled, {}), true);
  assert.equal(matchesLootFilter(failed, { q: "", status: "", profit: "" }), true);

  // Search matches label or item id, case-insensitively.
  assert.equal(matchesLootFilter(filled, { q: "miner" }), true);
  assert.equal(matchesLootFilter(filled, { q: "ABC123" }), true);
  assert.equal(matchesLootFilter(filled, { q: "  asic " }), true);
  assert.equal(matchesLootFilter(filled, { q: "fan" }), false);
  assert.equal(matchesLootFilter(filled, { q: "   " }), true);

  // Status narrows to filled/failed; unknown values match everything.
  assert.equal(matchesLootFilter(filled, { status: "filled" }), true);
  assert.equal(matchesLootFilter(filled, { status: "failed" }), false);
  assert.equal(matchesLootFilter(failed, { status: "failed" }), true);
  assert.equal(matchesLootFilter(filled, { status: "all" }), true);

  // Profit narrows by sign; zero is neither positive nor negative.
  assert.equal(matchesLootFilter(filled, { profit: "positive" }), true);
  assert.equal(matchesLootFilter(loss, { profit: "positive" }), false);
  assert.equal(matchesLootFilter(loss, { profit: "negative" }), true);
  assert.equal(matchesLootFilter(filled, { profit: "negative" }), false);
  assert.equal(matchesLootFilter(failed, { profit: "positive" }), false);
  assert.equal(matchesLootFilter(failed, { profit: "negative" }), false);

  // Criteria combine.
  assert.equal(matchesLootFilter(filled, { q: "miner", status: "filled", profit: "positive" }), true);
  assert.equal(matchesLootFilter(loss, { q: "miner", status: "filled", profit: "positive" }), false);

  // Non-entries never match.
  assert.equal(matchesLootFilter(null, {}), false);
  assert.equal(matchesLootFilter(undefined, { status: "filled" }), false);
});
