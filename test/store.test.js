import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Store, jwtExpiry } from "../src/worker/store.js";

function tempDir() {
  return mkdtempSync(join(tmpdir(), "turuc-store-"));
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
      assert.equal(statSync(join(dir, "state.json")).mode & 0o777, 0o600);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("store persists session, targets and history", () => {
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

    const persistedState = JSON.parse(readFileSync(join(dir, "state.json"), "utf8"));
    assert.equal(Object.prototype.hasOwnProperty.call(persistedState, "history"), false);
    const lootLog = JSON.parse(readFileSync(join(dir, "loot-log.json"), "utf8"));
    assert.equal(lootLog.history.length, 1);
    assert.equal(lootLog.history[0].profitRlt, 0.1);

    if (process.platform !== "win32") {
      assert.equal(statSync(join(dir, "loot-log.json")).mode & 0o777, 0o600);
    }

    const reloaded = new Store({ dataDir: dir, logger: null }).load();
    assert.equal(reloaded.state.session.accessToken, "access-token-123456");
    assert.equal(reloaded.state.configs.length, 1);
    assert.equal(reloaded.state.configs[0].label, "NEON SHARD");
    assert.equal(reloaded.state.history.length, 1);
    assert.equal(reloaded.state.history[0].profitRlt, 0.1);
    assert.equal(reloaded.state.stats.filled, 0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("migrates legacy state.json history into a separate loot-log.json", () => {
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

    const persistedState = JSON.parse(readFileSync(join(dir, "state.json"), "utf8"));
    const lootLog = JSON.parse(readFileSync(join(dir, "loot-log.json"), "utf8"));
    assert.equal(Object.prototype.hasOwnProperty.call(persistedState, "history"), false);
    assert.equal(lootLog.history[0].id, "legacy-1");
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

    const reloaded = new Store({ dataDir: dir, logger: null }).load();
    assert.deepEqual(reloaded.state.configs, []);
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
