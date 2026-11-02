import test from "node:test";
import assert from "node:assert/strict";
import { ItemInfoService } from "../src/worker/item-info.js";

function makeStore(label = "") {
  const config = {
    itemId: "6319f7baa8ce530569ed16b9",
    label,
    itemType: "mutation_component",
    targetPrice: 0.1,
    desiredQty: 10,
    highStockThreshold: 20,
    minAcceptableQty: 1
  };
  return {
    state: { configs: [config] },
    saves: 0,
    save() {
      this.saves += 1;
    },
    config
  };
}

function makeLogger() {
  return { info() {}, warn() {} };
}

test("composes rarity + name and caches item-info", async () => {
  const store = makeStore();
  const api = {
    calls: 0,
    async fetchItemInfo() {
      this.calls += 1;
      return {
        ok: true,
        status: 200,
        data: {
          name: { en: "Fan" },
          rarityGroup: { title: { en: "Uncommon" }, baseHexColor: "2bd600" },
          itemType: "mutation_component"
        }
      };
    }
  };
  const service = new ItemInfoService({ store, api, logger: makeLogger(), now: () => 1_000_000 });
  const updates = [];
  service.on("updated", (payload) => updates.push(payload));

  const first = await service.enrich(store.config.itemId, store.config.itemType);
  assert.equal(first.ok, true);
  assert.equal(store.config.label, "Uncommon Fan");
  assert.equal(store.config.rarityColor, "2bd600");
  assert.equal(store.saves, 1);
  assert.equal(updates.length, 1);

  await service.enrich(store.config.itemId, store.config.itemType);
  assert.equal(api.calls, 1, "second enrichment should use the cache");
});

test("falls back to the item name when rarityGroup is absent", async () => {
  const store = makeStore();
  const api = {
    async fetchItemInfo() {
      return {
        ok: true,
        status: 200,
        data: { name: { en: "Fan" }, itemType: "mutation_component" }
      };
    }
  };
  const service = new ItemInfoService({ store, api, logger: makeLogger() });
  const result = await service.enrich(store.config.itemId, store.config.itemType);
  assert.equal(result.ok, true);
  assert.equal(store.config.label, "Fan");
  assert.equal(store.config.rarityColor, undefined);
});

test("falls back to the item name when rarityGroup.title is missing", async () => {
  const store = makeStore();
  const api = {
    async fetchItemInfo() {
      return {
        ok: true,
        status: 200,
        data: { name: { en: "Booster" }, rarityGroup: { baseHexColor: "nope" } }
      };
    }
  };
  const service = new ItemInfoService({ store, api, logger: makeLogger() });
  await service.enrich(store.config.itemId, store.config.itemType);
  assert.equal(store.config.label, "Booster");
  assert.equal(store.config.rarityColor, undefined);
});

test("uses a safe fallback when name and rarity are both missing", async () => {
  const store = makeStore();
  const api = {
    async fetchItemInfo() {
      return { ok: true, status: 200, data: {} };
    }
  };
  const service = new ItemInfoService({ store, api, logger: makeLogger() });
  await service.enrich(store.config.itemId, store.config.itemType);
  assert.match(store.config.label, /^ITEM /);
});

test("discovers a non-default item type such as rack", async () => {
  const store = makeStore();
  const api = {
    calls: [],
    async fetchItemInfo({ itemType }) {
      this.calls.push(itemType);
      if (itemType === "rack") {
        return {
          ok: true,
          status: 200,
          data: {
            name: { en: "Jet-Black Rack 6" },
            itemType: "rack"
          }
        };
      }
      return { ok: false, status: 400, error: "Item not found" };
    }
  };
  const service = new ItemInfoService({ store, api, logger: makeLogger() });
  const result = await service.enrich(store.config.itemId, "mutation_component");
  assert.equal(result.ok, true);
  assert.equal(store.config.label, "Jet-Black Rack 6");
  assert.equal(store.config.itemType, "rack");
  assert.deepEqual(api.calls, ["mutation_component", "miner", "rack"]);
});

test("does not throw when item-info fails", async () => {
  const store = makeStore("Old label");
  const api = {
    async fetchItemInfo() {
      return { ok: false, status: 500, error: "nope" };
    }
  };
  const service = new ItemInfoService({ store, api, logger: makeLogger() });
  const result = await service.enrich(store.config.itemId, store.config.itemType);
  assert.equal(result.ok, false);
  assert.equal(store.config.label, "Old label");
});
