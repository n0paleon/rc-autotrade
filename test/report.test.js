import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import Database from "better-sqlite3";
import { MAX_HISTORY } from "../src/shared/constants.js";
import { Store } from "../src/worker/store.js";

function tempDir() {
  return mkdtempSync(join(tmpdir(), "turuc-report-"));
}

function fill(overrides = {}) {
  const at = overrides.at ?? Date.now();
  return {
    id: `order-${Math.random().toString(36).slice(2)}`,
    orderId: null,
    itemId: "item-a",
    label: "Rare Widget",
    qty: 1,
    unitPrice: 0.1,
    totalPrice: 0.1,
    targetPrice: 0.12,
    profitRlt: 0.02,
    status: "filled",
    at,
    latencyMs: null,
    note: null,
    ...overrides
  };
}

function closeTo(actual, expected, epsilon = 1e-9) {
  assert.ok(Math.abs(actual - expected) < epsilon, `expected ${actual} to be close to ${expected}`);
}

function failure(overrides = {}) {
  return fill({ status: "failed", qty: 0, totalPrice: 0, profitRlt: 0, ...overrides });
}

test("report keeps per-product totals past the loot-log cap", () => {
  const dir = tempDir();
  try {
    const store = new Store({ dataDir: dir, logger: null }).load();
    for (let index = 0; index < MAX_HISTORY + 15; index += 1) {
      store.addHistory(fill({ at: 1_000 + index }));
    }
    for (let index = 0; index < 3; index += 1) {
      store.addHistory(failure({ at: 2_000 + index }));
    }

    assert.equal(store.state.history.length, MAX_HISTORY, "loot log stays capped");
    const report = store.reportView();
    assert.equal(report.items.length, 1);
    const product = report.items[0];
    assert.equal(product.itemId, "item-a");
    assert.equal(product.fills, MAX_HISTORY + 15);
    assert.equal(product.failed, 3);
    assert.equal(product.qty, MAX_HISTORY + 15);
    closeTo(product.spentRlt, (MAX_HISTORY + 15) * 0.1);
    closeTo(product.profitRlt, (MAX_HISTORY + 15) * 0.02);
    assert.equal(product.avgBuy, 0.1);
    assert.equal(product.marginPct, 20);
    assert.equal(product.successPct, Math.round(((MAX_HISTORY + 15) / (MAX_HISTORY + 18)) * 100));

    store.flush();
    const reloaded = new Store({ dataDir: dir, logger: null }).load();
    const persisted = reloaded.reportView();
    assert.equal(persisted.items[0].fills, MAX_HISTORY + 15);
    assert.equal(persisted.items[0].failed, 3);
    assert.equal(persisted.totals.profitRlt, persisted.items[0].profitRlt);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("report separates products, counts failures and resets with stats", () => {
  const dir = tempDir();
  try {
    const store = new Store({ dataDir: dir, logger: null }).load();
    store.addHistory(fill({ itemId: "item-a", label: "Widget A", at: 1_000 }));
    store.addHistory(fill({ itemId: "item-a", label: "Widget A", at: 2_000, totalPrice: 0.2, profitRlt: 0.04 }));
    store.addHistory(fill({ itemId: "item-b", label: "Widget B", at: 3_000, totalPrice: 0.5, profitRlt: 0.1 }));
    store.addHistory(failure({ itemId: "item-b", label: "Widget B", at: 4_000 }));

    const report = store.reportView();
    assert.equal(report.items.length, 2);
    const [best, other] = report.items;
    assert.equal(best.itemId, "item-b");
    assert.equal(best.fills, 1);
    assert.equal(best.failed, 1);
    assert.equal(best.successPct, 50);
    assert.equal(other.itemId, "item-a");
    assert.equal(other.fills, 2);
    assert.equal(other.profitRlt, 0.06);
    assert.equal(other.spentRlt, 0.3);
    assert.equal(report.totals.fills, 3);
    assert.equal(report.totals.failed, 1);
    assert.equal(report.totals.profitRlt, 0.16);

    store.resetStats();
    const cleared = store.reportView();
    assert.deepEqual(cleared.items, []);
    assert.equal(cleared.totals.fills, 0);
    assert.equal(cleared.totals.profitRlt, 0);
    assert.equal(cleared.since > 0, true);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("legacy loot log backfills the report on first load", () => {
  const dir = tempDir();
  try {
    writeFileSync(
      join(dir, "loot-log.json"),
      JSON.stringify({
        version: 1,
        updatedAt: 5_000,
        history: [
          fill({ itemId: "item-a", label: "Widget A", at: 2_000 }),
          failure({ itemId: "item-b", label: "Widget B", at: 3_000 })
        ]
      })
    );
    writeFileSync(
      join(dir, "state.json"),
      JSON.stringify({
        version: 1,
        stats: { since: 1_500, profitRlt: 0.02, spentRlt: 0.1, bought: 1, filled: 1, failed: 1, ordersSent: 1 }
      })
    );

    const store = new Store({ dataDir: dir, logger: null }).load();
    const report = store.reportView();
    assert.equal(report.since, 1_500);
    assert.equal(report.items.length, 2);
    assert.equal(report.totals.fills, 1);
    assert.equal(report.totals.failed, 1);

    const db = new Database(join(dir, "rollercoin.db"), { readonly: true });
    const persisted = db.prepare("SELECT COUNT(*) AS n FROM report_products").get();
    db.close();
    assert.equal(persisted.n, 2, "backfill must be written to SQLite");

    const reloaded = new Store({ dataDir: dir, logger: null }).load();
    assert.equal(reloaded.reportView().totals.fills, 1, "backfilled report is persisted exactly once");
    assert.equal(reloaded.reportView().totals.failed, 1);
    store.close();
    reloaded.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
