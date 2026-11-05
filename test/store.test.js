import { test } from "bun:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { openSqlite } from "../src/worker/db.js";
import { LOOT_LOG_PAGE_SIZE, MAX_HISTORY } from "../src/shared/constants.js";
import { Store, jwtExpiry } from "../src/worker/store.js";

function tempDir() {
  return mkdtempSync(join(tmpdir(), "turuc-store-"));
}

function readDb(dir) {
  return openSqlite(join(dir, "rollercoin.db"), { readonly: true });
}

test("store generates, verifies and rotates worker keys", () => {
  const dir = tempDir();
  try {
    const store = new Store({ dataDir: dir, logger: null }).load();
    const generated = store.ensureWorkerToken("");
    assert.equal(generated.source, "generated");
    assert.ok(generated.plaintext.length >= 32);
    assert.equal(store.verifyWorkerToken(generated.plaintext), true);
    assert.equal(store.verifyWorkerToken("wrong"), false);

    const rotated = store.rotateWorkerToken();
    assert.equal(store.verifyWorkerToken(rotated), true);
    assert.equal(store.verifyWorkerToken(generated.plaintext), false);

    if (process.platform !== "win32") {
      assert.equal(statSync(join(dir, "rollercoin.db")).mode & 0o777, 0o600);
    }
    assert.equal(existsSync(join(dir, "state.json")), false, "no JSON file is created");
    store.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("store persists session, targets and history in SQLite", () => {
  const dir = tempDir();
  try {
    const store = new Store({ dataDir: dir, logger: null }).load();
    store.setSession(
      { accessToken: "access-token-123456", refreshToken: "refresh-token-123456", cookie: "cf=1" },
      { source: "test" }
    );
    store.upsertConfig({
      itemId: "6571aa39ca8bfef34adcd9e9",
      label: "NEON SHARD",
      targetPrice: 0.1,
      desiredQty: 10,
      highStockThreshold: 20,
      minAcceptableQty: 1,
      allowPartialOrder: true,
      active: true
    });
    store.addHistory({
      id: "order-1",
      orderId: "order-1",
      itemId: "6571aa39ca8bfef34adcd9e9",
      label: "NEON SHARD",
      qty: 10,
      unitPrice: 0.09,
      totalPrice: 0.9,
      targetPrice: 0.1,
      profitRlt: 0.1,
      status: "filled",
      at: Date.now(),
      latencyMs: 400,
      note: null
    });
    store.flush();

    const db = readDb(dir);
    const order = db.prepare("SELECT * FROM orders WHERE id = ?").get("order-1");
    const targets = db.prepare("SELECT * FROM targets").all();
    const session = db.prepare("SELECT * FROM session WHERE id = 1").get();
    db.close();

    assert.equal(targets.length, 1);
    assert.equal(targets[0].label, "NEON SHARD");
    assert.equal(order.profit_rlt, 0.1);
    assert.equal(order.message, "-");
    assert.equal(session.access_token, "access-token-123456");

    if (process.platform !== "win32") {
      assert.equal(statSync(join(dir, "rollercoin.db")).mode & 0o777, 0o600);
    }

    const reloaded = new Store({ dataDir: dir, logger: null }).load();
    assert.equal(reloaded.state.session.accessToken, "access-token-123456");
    assert.equal(reloaded.state.configs.length, 1);
    assert.equal(reloaded.state.configs[0].label, "NEON SHARD");
    assert.equal(reloaded.state.configs[0].targetPrice, 0.1, "the base target is what gets persisted");
    assert.equal(reloaded.state.history.length, 1);
    assert.equal(reloaded.state.history[0].profitRlt, 0.1);
    assert.equal(reloaded.state.history[0].message, "-");
    assert.equal(reloaded.state.stats.filled, 0);
    reloaded.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("store clears session credentials on explicit flags", () => {
  const dir = tempDir();
  try {
    const store = new Store({ dataDir: dir, logger: null }).load();
    store.setSession({
      accessToken: "access-token-123456",
      refreshToken: "refresh-token-123456",
      cookie: "cf=1"
    });

    const cleared = store.setSession({ clearAccessToken: true, clearRefreshToken: true, clearCookie: true });
    assert.equal(cleared.hasAccessToken, false);
    assert.equal(cleared.hasRefreshToken, false);
    assert.equal(cleared.hasCookie, false);

    store.close();
    const reloaded = new Store({ dataDir: dir, logger: null }).load();
    assert.equal(reloaded.state.session.accessToken, "");
    assert.equal(reloaded.state.session.refreshToken, "");
    assert.equal(reloaded.state.session.cookie, "");
    reloaded.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("store can clear a single credential while keeping the others", () => {
  const dir = tempDir();
  try {
    const store = new Store({ dataDir: dir, logger: null }).load();
    store.setSession({
      accessToken: "access-token-123456",
      refreshToken: "refresh-token-123456",
      cookie: "cf=1"
    });

    const cleared = store.setSession({ clearCookie: true });
    assert.equal(cleared.hasAccessToken, true);
    assert.equal(cleared.hasRefreshToken, true);
    assert.equal(cleared.hasCookie, false);
    store.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("backfills loot messages: dash for fills, reason for failures", () => {
  const dir = tempDir();
  try {
    writeFileSync(
      join(dir, "loot-log.json"),
      JSON.stringify({
        history: [
          {
            id: "failed-1",
            itemId: "6571aa39ca8bfef34adcd9e9",
            label: "LEGACY FAIL",
            qty: 0,
            status: "failed",
            at: Date.now(),
            note: "marketplace rejected the order"
          },
          {
            id: "filled-1",
            itemId: "6571aa39ca8bfef34adcd9e9",
            label: "LEGACY FILL",
            qty: 3,
            status: "filled",
            at: Date.now(),
            note: null
          }
        ]
      })
    );

    const store = new Store({ dataDir: dir, logger: null }).load();
    const byId = Object.fromEntries(store.state.history.map((entry) => [entry.id, entry]));
    assert.equal(byId["failed-1"].message, "marketplace rejected the order");
    assert.equal(byId["filled-1"].message, "-");
    store.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("imports legacy state.json once and archives the JSON files", () => {
  const dir = tempDir();
  try {
    const legacyEntry = {
      id: "legacy-1",
      itemId: "6571aa39ca8bfef34adcd9e9",
      label: "LEGACY",
      qty: 5,
      unitPrice: 0.09,
      totalPrice: 0.45,
      targetPrice: 0.1,
      profitRlt: 0.05,
      status: "filled",
      at: Date.now()
    };
    writeFileSync(
      join(dir, "state.json"),
      JSON.stringify(
        {
          version: 1,
          createdAt: Date.now(),
          updatedAt: Date.now(),
          configs: [],
          settings: {},
          stats: {},
          history: [legacyEntry]
        },
        null,
        2
      )
    );

    const store = new Store({ dataDir: dir, logger: null }).load();
    assert.equal(store.state.history.length, 1);
    assert.equal(store.state.history[0].id, "legacy-1");
    assert.equal(store.state.history[0].message, "-");

    assert.equal(existsSync(join(dir, "state.json")), false, "legacy file is renamed");
    assert.equal(
      readdirSync(dir).filter((name) => name.startsWith("state.json.migrated-")).length,
      1
    );

    const db = readDb(dir);
    const order = db.prepare("SELECT * FROM orders WHERE id = ?").get("legacy-1");
    db.close();
    assert.equal(order.label, "LEGACY");

    const reloaded = new Store({ dataDir: dir, logger: null }).load();
    assert.equal(reloaded.state.history.length, 1, "reload does not import twice");
    assert.equal(reloaded.state.history[0].id, "legacy-1");
    store.close();
    reloaded.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("store drops invalid saved targets instead of crashing", () => {
  const dir = tempDir();
  try {
    const store = new Store({ dataDir: dir, logger: null }).load();
    store.state.configs = [
      { itemId: "bad id", targetPrice: -1, desiredQty: 0, highStockThreshold: 0, minAcceptableQty: 0 }
    ];
    store.dirty = true;
    store.flush();
    store.close();

    const reloaded = new Store({ dataDir: dir, logger: null }).load();
    assert.deepEqual(reloaded.state.configs, []);
    reloaded.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("market snapshots and price points survive a restart", () => {
  const dir = tempDir();
  const itemId = "6571aa39ca8bfef34adcd9e9";
  const now = Date.now();
  const bucket = Math.floor(now / 60_000) * 60_000;

  try {
    const store = new Store({ dataDir: dir, logger: null }).load();
    store.upsertConfig({
      itemId,
      label: "NEON SHARD",
      targetPrice: 0.1,
      desiredQty: 10,
      highStockThreshold: 20,
      minAcceptableQty: 1,
      allowPartialOrder: true,
      active: true
    });
    store.flush();
    store.saveMarketSnapshot({
      itemId,
      lastPrice: 0.05,
      lastStock: 12,
      allowedPriceMin: 0.01,
      allowedPriceMax: 0.2,
      allowedAt: now,
      itemType: "rack",
      seenAt: now
    });
    store.recordPricePoint(itemId, bucket - 60_000, 0.06);
    store.recordPricePoint(itemId, bucket, 0.05);
    store.recordPricePoint(itemId, now - 20 * 24 * 60 * 60 * 1000, 0.5);

    // A flush must not wipe the snapshot (targets are upserted, not deleted wholesale).
    store.flush();
    store.close();

    const reloaded = new Store({ dataDir: dir, logger: null }).load();
    const snapshot = reloaded.loadMarketSnapshots()[itemId];
    assert.equal(snapshot.lastPrice, 0.05);
    assert.equal(snapshot.lastStock, 12);
    assert.equal(snapshot.itemType, "rack");

    const points = reloaded.loadPriceHistory(itemId);
    assert.deepEqual(
      points.map((point) => point.p),
      [0.06, 0.05],
      "old points are pruned and the rest stay in chronological order"
    );
    assert.equal(reloaded.loadPriceHistory(itemId, { limit: 1 }).length, 1);

    reloaded.removeConfig(itemId);
    reloaded.flush();
    assert.deepEqual(reloaded.loadMarketSnapshots(), {}, "removing a target cascades to its snapshot");
    reloaded.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("report and lifetime stats survive loot-log rotation", () => {
  const dir = tempDir();
  try {
    const store = new Store({ dataDir: dir, logger: null }).load();
    for (let index = 0; index < 5; index += 1) {
      store.addHistory({
        id: `order-${index}`,
        itemId: "6571aa39ca8bfef34adcd9e9",
        label: "NEON SHARD",
        qty: 1,
        unitPrice: 0.09,
        totalPrice: 0.09,
        targetPrice: 0.1,
        profitRlt: 0.01,
        status: index === 4 ? "failed" : "filled",
        at: 1_000 + index
      });
    }
    store.close();

    const reloaded = new Store({ dataDir: dir, logger: null }).load();
    const report = reloaded.reportView();
    assert.equal(report.items.length, 1);
    assert.equal(report.items[0].fills, 4);
    assert.equal(report.items[0].failed, 1);
    reloaded.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("permanent loot log keeps every order, pages with a cursor and searches by product", () => {
  const dir = tempDir();
  try {
    const store = new Store({ dataDir: dir, logger: null }).load();
    const total = MAX_HISTORY + 25;
    store.addHistory({
      id: "percent-1",
      itemId: "item-pct",
      label: "100% Widget",
      qty: 1,
      unitPrice: 0.1,
      totalPrice: 0.1,
      targetPrice: 0.2,
      profitRlt: 0.1,
      status: "filled",
      at: 500
    });
    for (let index = 0; index < total; index += 1) {
      store.addHistory({
        id: `order-${index}`,
        itemId: index % 2 ? "item-a" : "item-b",
        label: index % 2 ? "Rare Widget" : "Common Gear",
        qty: 1,
        unitPrice: 0.1,
        totalPrice: 0.1,
        targetPrice: 0.2,
        profitRlt: 0.1,
        status: index % 7 === 0 ? "failed" : "filled",
        at: 1_000 + index
      });
    }

    assert.equal(store.state.history.length, MAX_HISTORY, "in-memory window stays capped");
    assert.equal(store.historyCount(), total + 1, "every order is stored permanently");
    assert.equal(store.hasOrder("order-0"), true);
    assert.equal(store.hasOrder("percent-1"), true);
    assert.equal(store.hasOrder("missing-order"), false);

    const first = store.historyPage({ limit: LOOT_LOG_PAGE_SIZE });
    assert.equal(first.total, total + 1);
    assert.equal(first.entries.length, LOOT_LOG_PAGE_SIZE);
    assert.equal(first.hasMore, true);
    assert.equal(first.entries[0].id, `order-${total - 1}`, "newest order comes first");

    // The cursor is stable: no row is repeated or skipped across pages.
    const oldest = first.entries.at(-1);
    const second = store.historyPage({
      limit: LOOT_LOG_PAGE_SIZE,
      beforeAt: oldest.at,
      beforeId: oldest.id
    });
    const seen = new Set([...first.entries, ...second.entries].map((entry) => entry.id));
    assert.equal(seen.size, first.entries.length + second.entries.length);

    const searched = store.historyPage({ q: "rare", limit: 200 });
    assert.equal(searched.total, Math.floor(total / 2), "LIKE search is case-insensitive");
    assert.ok(searched.entries.every((entry) => entry.label === "Rare Widget"));

    const failed = store.historyPage({ status: "failed", limit: 200 });
    assert.ok(failed.total > 0);
    assert.ok(failed.entries.every((entry) => entry.status === "failed"));

    assert.equal(store.historyPage({ q: "%", limit: 10 }).total, 1, "LIKE wildcards are escaped");
    assert.equal(store.historyPage({ q: "_", limit: 10 }).total, 0);

    store.close();
    const reloaded = new Store({ dataDir: dir, logger: null }).load();
    assert.equal(reloaded.historyCount(), total + 1, "log survives a restart");
    assert.equal(reloaded.historyPage({ q: "common" }).total, Math.ceil(total / 2));
    reloaded.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("jwtExpiry reads an exp claim and rejects opaque tokens", () => {
  const payload = Buffer.from(JSON.stringify({ exp: 1_800_000_000 })).toString("base64url");
  assert.equal(jwtExpiry(`header.${payload}.sig`), 1_800_000_000_000);
  assert.equal(jwtExpiry("opaque-token"), null);
  assert.equal(jwtExpiry(null), null);
});
