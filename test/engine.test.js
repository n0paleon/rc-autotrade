import { test } from "bun:test";
import assert from "node:assert/strict";
import { Engine } from "../src/worker/engine.js";

const ITEM_ID = "6319f7baa8ce530569ed16b9";

function createHarness() {
  const config = {
    itemId: ITEM_ID,
    label: "Test Item",
    itemType: "mutation_component",
    targetPrice: 0.1,
    desiredQty: 10,
    highStockThreshold: 20,
    minAcceptableQty: 1,
    allowPartialOrder: true,
    active: true
  };
  const store = {
    state: {
      configs: [config],
      settings: {
        delayPerOrder: 200,
        maxOrdersPerMinute: 60,
        dryRun: false,
        paused: false,
        autoRefresh: true
      },
      stats: {
        ordersSent: 0,
        filled: 0,
        failed: 0,
        bought: 0,
        spentRlt: 0,
        profitRlt: 0
      },
      history: []
    },
    save() {},
    addHistory(entry) {
      this.state.history.unshift(entry);
      if (this.state.history.length > 200) this.state.history.length = 200;
    },
    hasOrder(id) {
      return this.state.history.some((entry) => entry.id === id);
    },
    reportView() {
      return { since: 0, totals: {}, items: [] };
    }
  };
  let now = 1_000_000;
  let outcome = { ok: false, error: "rejected" };
  const api = {
    async purchase() {
      return outcome;
    }
  };
  const events = [];
  const engine = new Engine({
    store,
    api,
    session: { getAccessToken: () => "token" },
    logger: { info() {}, warn() {}, error() {}, debug() {} },
    broadcast: (type, data) => events.push({ type, data }),
    clock: () => now
  });
  const marketFrame = (data = {}) => ({
    raw: {
      cmd: "marketplace_orders_update",
      value: {
        item_id: ITEM_ID,
        currency: "RLT",
        item_type: "mutation_component",
        data: { list: [{ price: 90_000, quantity: 10 }], ...data }
      }
    }
  });
  return {
    config,
    store,
    engine,
    events,
    marketFrame,
    setOutcome(value) {
      outcome = value;
    },
    advance(ms) {
      now += ms;
    }
  };
}

const tick = () => new Promise((resolve) => setTimeout(resolve, 5));

test("caches the allowed price band per target and keeps it when a frame omits it", () => {
  const harness = createHarness();
  harness.engine.handleFrame(
    harness.marketFrame({ allowedPriceMin: 50_000, allowedPriceMax: 250_000 })
  );

  let runtime = harness.engine.runtimeSnapshot()[ITEM_ID];
  assert.equal(runtime.allowedPriceMin, 0.05);
  assert.equal(runtime.allowedPriceMax, 0.25);
  assert.equal(runtime.allowedAt, 1_000_000);

  let market = harness.events.filter((event) => event.type === "market").at(-1);
  assert.equal(market.data.allowedPriceMin, 0.05);
  assert.equal(market.data.allowedPriceMax, 0.25);

  harness.advance(1500);
  harness.engine.handleFrame(harness.marketFrame());

  runtime = harness.engine.runtimeSnapshot()[ITEM_ID];
  assert.equal(runtime.allowedPriceMin, 0.05, "a frame without the band must not clear the cache");
  assert.equal(runtime.allowedPriceMax, 0.25);
  assert.equal(runtime.allowedAt, 1_000_000, "the band timestamp only moves when a new band arrives");

  market = harness.events.filter((event) => event.type === "market").at(-1);
  assert.equal(market.data.allowedPriceMin, 0.05, "broadcasts keep serving the cached band");
  assert.equal(market.data.allowedPriceMax, 0.25);
});

test("auto target derives the ceiling from allowedPriceMin without touching the saved base", () => {
  const harness = createHarness();
  harness.config.autoTarget = true;
  harness.config.autoTargetMarginPct = 5;

  // Floor 0.05 + 5% = 0.0525, below the 0.09 listing: the frame stays cold.
  harness.engine.handleFrame(
    harness.marketFrame({ allowedPriceMin: 50_000, allowedPriceMax: 250_000 })
  );

  let runtime = harness.engine.runtimeSnapshot()[ITEM_ID];
  assert.equal(runtime.autoTargetPrice, 0.0525);
  assert.equal(runtime.autoTargetAt, 1_000_000);
  assert.equal(runtime.effectiveTargetPrice, 0.0525);

  let market = harness.events.filter((event) => event.type === "market").at(-1);
  assert.equal(market.data.targetPrice, 0.0525);
  assert.equal(market.data.baseTargetPrice, 0.1);
  assert.equal(market.data.autoTarget, true);
  assert.equal(market.data.autoTargetMarginPct, 5);
  assert.equal(market.data.hot, false);

  // The adjusted price exists only in the runtime state; the configuration
  // (and therefore SQLite) keeps the base price.
  assert.equal(harness.config.targetPrice, 0.1);
  assert.equal(harness.store.state.configs[0].targetPrice, 0.1);

  // A frame that omits the band keeps the last derived target.
  harness.advance(1500);
  harness.engine.handleFrame(harness.marketFrame());
  runtime = harness.engine.runtimeSnapshot()[ITEM_ID];
  assert.equal(runtime.autoTargetPrice, 0.0525);
  assert.equal(runtime.autoTargetAt, 1_000_000);

  // Editing the target invalidation drops the override; the saved base applies
  // again until another live frame carries a band.
  harness.engine.invalidateAutoTarget(ITEM_ID);
  runtime = harness.engine.runtimeSnapshot()[ITEM_ID];
  assert.equal(runtime.autoTargetPrice, null);
  harness.engine.handleFrame(harness.marketFrame());
  market = harness.events.filter((event) => event.type === "market").at(-1);
  assert.equal(market.data.targetPrice, 0.1);
  assert.equal(market.data.autoTargetPrice, null);
});

test("auto target can raise the ceiling above the saved base", () => {
  const harness = createHarness();
  harness.config.autoTarget = true;
  harness.config.autoTargetMarginPct = 5;
  harness.config.targetPrice = 0.05;

  harness.engine.handleFrame(harness.marketFrame({ allowedPriceMin: 100_000 }));

  const market = harness.events.filter((event) => event.type === "market").at(-1);
  assert.equal(market.data.targetPrice, 0.105);
  assert.equal(market.data.hot, true, "the live floor + margin can replace a lower base");
  assert.equal(harness.store.state.stats.ordersSent, 1);
});

test("a restart ignores hydrated bands and starts from the saved base price", () => {
  const harness = createHarness();
  harness.config.autoTarget = true;
  harness.config.autoTargetMarginPct = 5;

  // Simulate the SQLite market cache surviving a restart.
  harness.store.loadMarketSnapshots = () => ({
    [ITEM_ID]: {
      lastPrice: 0.09,
      lastStock: 10,
      allowedPriceMin: 0.05,
      allowedPriceMax: 0.25,
      allowedAt: 900_000,
      itemType: "mutation_component",
      seenAt: 950_000
    }
  });
  harness.store.loadPriceHistory = () => [];
  harness.engine.hydrateMarket();

  let runtime = harness.engine.runtimeSnapshot()[ITEM_ID];
  assert.equal(runtime.allowedPriceMin, 0.05, "the band itself is still restored for display");
  assert.equal(runtime.autoTargetPrice, null, "hydration must not invent a runtime target");

  // The first live frame has no band, so the base price is used.
  harness.engine.handleFrame(harness.marketFrame());
  let market = harness.events.filter((event) => event.type === "market").at(-1);
  assert.equal(market.data.targetPrice, 0.1);
  assert.equal(market.data.autoTargetPrice, null);

  // A frame with a band re-arms auto mode while the worker runs.
  harness.engine.handleFrame(harness.marketFrame({ allowedPriceMin: 50_000 }));
  runtime = harness.engine.runtimeSnapshot()[ITEM_ID];
  market = harness.events.filter((event) => event.type === "market").at(-1);
  assert.equal(runtime.autoTargetPrice, 0.0525);
  assert.equal(market.data.targetPrice, 0.0525);
});

test("a rejected purchase never increases the book stock", async () => {
  const harness = createHarness();
  harness.engine.handleFrame(harness.marketFrame());
  await tick();

  const runtime = harness.engine.runtimeSnapshot()[ITEM_ID];
  assert.equal(runtime.lastStock, 10, "stock must stay at the market book quantity");
  assert.equal(harness.store.state.stats.failed, 1);
  assert.equal(runtime.failures, 1);
  assert.equal(runtime.fills, 0);
  assert.equal(runtime.lastResult.status, "rejected");
  assert.equal(harness.store.state.history[0].message, "rejected", "failures carry the rejection reason");
});

test("a filled purchase reduces the book stock immediately", async () => {
  const harness = createHarness();
  harness.engine.handleFrame(harness.marketFrame());
  await tick();

  harness.advance(6000);
  harness.setOutcome({ ok: true, orderId: "order-1" });
  harness.engine.handleFrame(harness.marketFrame());
  await tick();

  let runtime = harness.engine.runtimeSnapshot()[ITEM_ID];
  assert.equal(runtime.lastStock, 10, "accepted order should not decrement before the fill");

  harness.engine.handleFrame({
    raw: {
      cmd: "marketplace_orders_filled",
      value: {
        item_id: ITEM_ID,
        order_id: "order-1",
        count: 10,
        price: 900_000,
        status: 12
      }
    }
  });

  runtime = harness.engine.runtimeSnapshot()[ITEM_ID];
  assert.equal(runtime.lastStock, 0, "filled quantity should leave the book immediately");
  assert.equal(runtime.fills, 1);
  assert.equal(runtime.failures, 1);
  assert.equal(harness.store.state.stats.filled, 1);
  assert.equal(harness.store.state.stats.bought, 10);

  const fillEvent = harness.events.find((event) => event.type === "order" && event.data.phase === "filled");
  assert.equal(fillEvent.data.stockAfter, 0);

  const filledEntry = harness.store.state.history.find((entry) => entry.status === "filled");
  assert.equal(filledEntry.message, "-", "filled loot rows show a dash");
});
