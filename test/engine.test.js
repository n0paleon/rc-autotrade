import test from "node:test";
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
  const marketFrame = () => ({
    raw: {
      cmd: "marketplace_orders_update",
      value: {
        item_id: ITEM_ID,
        currency: "RLT",
        item_type: "mutation_component",
        data: { list: [{ price: 90_000, quantity: 10 }] }
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
});
