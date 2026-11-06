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
  assert.ok(filledEntry.timings, "filled loot rows carry latency marks");
  assert.equal(filledEntry.timings.signalAt, 1_006_000);
  assert.equal(filledEntry.timings.fillAt, 1_006_000);
  assert.ok(filledEntry.timings.sendAt >= filledEntry.timings.signalAt);
  assert.ok(filledEntry.timings.responseAt >= filledEntry.timings.sendAt);
  assert.ok(filledEntry.timings.completeAt >= filledEntry.timings.responseAt);
  assert.ok(filledEntry.timings.acceptedAt >= filledEntry.timings.completeAt);
  assert.ok(filledEntry.timings.fillAt >= filledEntry.timings.acceptedAt);
});

test("a rejected purchase records the response boundary for the debugger", async () => {
  const harness = createHarness();
  harness.engine.handleFrame(harness.marketFrame());
  await tick();

  const rejected = harness.store.state.history.find((entry) => entry.status === "failed");
  assert.ok(rejected.timings, "failed loot rows carry latency marks too");
  assert.ok(rejected.timings.sendAt >= rejected.timings.signalAt);
  assert.ok(rejected.timings.responseAt >= rejected.timings.sendAt);
  assert.ok(rejected.timings.failedAt >= rejected.timings.completeAt);
  assert.equal(rejected.timings.acceptedAt, undefined);
  assert.equal(rejected.timings.fillAt, undefined);
});
