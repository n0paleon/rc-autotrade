import { test } from "bun:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Engine } from "../src/worker/engine.js";
import { Store } from "../src/worker/store.js";

const ITEM_ID = "6319f7baa8ce530569ed16b9";
const SILENT = { info() {}, warn() {}, error() {}, debug() {} };

function tempDir() {
  return mkdtempSync(join(tmpdir(), "turuc-cache-"));
}

function seedStore(dir) {
  const store = new Store({ dataDir: dir, logger: null }).load();
  store.upsertConfig({
    itemId: ITEM_ID,
    label: "Test Item",
    targetPrice: 0.1,
    desiredQty: 10,
    highStockThreshold: 20,
    minAcceptableQty: 1,
    allowPartialOrder: true,
    active: true
  });
  store.flush();
  return store;
}

function marketFrame(priceRaw, quantity) {
  return {
    raw: {
      cmd: "marketplace_orders_update",
      value: {
        item_id: ITEM_ID,
        currency: "RLT",
        item_type: "mutation_component",
        data: { list: [{ price: priceRaw, quantity }] }
      }
    }
  };
}

function harness(store, clock) {
  let purchases = 0;
  const engine = new Engine({
    store,
    api: {
      async purchase() {
        purchases += 1;
        return { ok: false, error: "rejected" };
      }
    },
    session: { getAccessToken: () => "" },
    logger: SILENT,
    broadcast: () => {},
    clock
  });
  return { engine, purchases: () => purchases };
}

test("hydrates cached prices on boot without planning any order", () => {
  const dir = tempDir();
  const seenAt = Date.now();

  try {
    const store = seedStore(dir);
    store.saveMarketSnapshot({
      itemId: ITEM_ID,
      lastPrice: 0.08,
      lastStock: 7,
      allowedPriceMin: 0.01,
      allowedPriceMax: 0.3,
      allowedAt: seenAt - 60_000,
      itemType: "mutation_component",
      seenAt
    });
    store.recordPricePoint(ITEM_ID, seenAt - 120_000, 0.07);
    store.recordPricePoint(ITEM_ID, seenAt - 60_000, 0.08);
    store.close();

    const reopened = new Store({ dataDir: dir, logger: null }).load();
    const { engine, purchases } = harness(reopened, () => seenAt);

    assert.equal(engine.hydrateMarket(), 1);
    const runtime = engine.runtimeSnapshot()[ITEM_ID];
    assert.equal(runtime.stale, true);
    assert.equal(runtime.lastPrice, 0.08);
    assert.equal(runtime.lastStock, 7);
    assert.equal(runtime.seenAt, seenAt);
    assert.deepEqual(
      runtime.history.map((point) => point.p),
      [0.07, 0.08]
    );
    assert.equal(purchases(), 0, "cached data must never trigger a purchase");

    engine.stop();
    reopened.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("live frames refresh the cache, clear stale and survive shutdown", () => {
  const dir = tempDir();
  let now = Date.now();

  try {
    const store = seedStore(dir);
    const { engine } = harness(store, () => now);

    engine.handleFrame(marketFrame(200_000, 5)); // 0.2 RLT, above target: no order
    let runtime = engine.runtimeSnapshot()[ITEM_ID];
    assert.equal(runtime.stale, false);
    assert.equal(runtime.lastPrice, 0.2);
    assert.equal(store.loadMarketSnapshots()[ITEM_ID].lastPrice, 0.2);
    assert.deepEqual(
      store.loadPriceHistory(ITEM_ID).map((point) => point.p),
      [0.2]
    );

    // Inside the throttle window the row is only refreshed on shutdown.
    now += 500;
    engine.handleFrame(marketFrame(300_000, 4)); // 0.3 RLT
    assert.equal(store.loadMarketSnapshots()[ITEM_ID].lastPrice, 0.2);

    engine.stop();
    assert.equal(store.loadMarketSnapshots()[ITEM_ID].lastPrice, 0.3);
    runtime = engine.runtimeSnapshot()[ITEM_ID];
    assert.equal(runtime.lastSeenAt, now);
    store.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
