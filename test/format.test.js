import test from "node:test";
import assert from "node:assert/strict";

import {
  itemTypeLabel,
  itemTypeOf,
  ITEM_TYPE_ORDER,
  marketplaceUrl
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
