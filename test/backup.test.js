import { test } from "bun:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  BACKUP_FORMAT_VERSION,
  BACKUP_KIND,
  backupFilename,
  buildBackup,
  createDatabaseSnapshot,
  databaseFilename,
  parseBackup,
  restoreBackup
} from "../src/worker/backup.js";
import { openSqlite } from "../src/worker/db.js";
import { SCHEMA_VERSION } from "../src/worker/schema.js";
import { Store } from "../src/worker/store.js";

const TARGET_A = "6571aa39ca8bfef34adcd9e9";
const TARGET_B = "66b0a1c2d3e4f5061728394a";

function tempDir(label = "backup") {
  return mkdtempSync(join(tmpdir(), `turuc-${label}-`));
}

function seedStore(dir, { targetId = TARGET_A, label = "NEON SHARD", token = "access-token-123456" } = {}) {
  const store = new Store({ dataDir: dir, logger: null }).load();
  store.setSession(
    {
      accessToken: token,
      refreshToken: "refresh-token-123456",
      cookie: "cf_clearance=abc"
    },
    { source: "test" }
  );
  store.upsertConfig({
    itemId: targetId,
    label,
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

function addOrder(store, id, { itemId = TARGET_A, label = "NEON SHARD", status = "filled" } = {}) {
  store.addHistory({
    id,
    orderId: id,
    itemId,
    label,
    qty: status === "filled" ? 10 : 0,
    unitPrice: status === "filled" ? 0.09 : 0,
    totalPrice: status === "filled" ? 0.9 : 0,
    targetPrice: 0.1,
    profitRlt: status === "filled" ? 0.1 : 0,
    status,
    at: Date.now(),
    latencyMs: status === "filled" ? 400 : null,
    note: status === "failed" ? "no items obtained" : null
  });
}

test("buildBackup exports portable rows and counts, credentials optional", () => {
  const dir = tempDir();
  try {
    const store = seedStore(dir);
    addOrder(store, "order-filled");
    addOrder(store, "order-failed", { status: "failed" });
    store.state.stats.filled = 1;
    store.state.stats.failed = 1;
    store.state.stats.spentRlt = 0.9;
    store.state.stats.profitRlt = 0.1;
    store.flush();

    const backup = buildBackup(store, { now: 1_790_000_000_000 });
    assert.equal(backup.kind, BACKUP_KIND);
    assert.equal(backup.formatVersion, BACKUP_FORMAT_VERSION);
    assert.equal(backup.schemaVersion, SCHEMA_VERSION);
    assert.equal(backup.exportedAt, 1_790_000_000_000);
    assert.deepEqual(backup.counts, { targets: 1, orders: 2, reportProducts: 1 });
    assert.equal(backup.credentials, undefined);
    assert.equal(backup.data.targets[0].itemId, TARGET_A);
    assert.equal(backup.data.targets[0].label, "NEON SHARD");
    assert.equal(backup.data.orders.length, 2);
    assert.equal(backup.data.orders[0].message, "-");
    assert.equal(backup.data.orders[1].message, "no items obtained");
    assert.equal(backup.data.reportProducts[0].fills, 1);
    assert.equal(backup.data.reportProducts[0].failed, 1);
    assert.equal(backup.data.stats.filled, 1);
    assert.equal(backup.data.settings.dryRun, false);
    assert.ok(!("session" in backup.data), "credentials are never exported by default");

    const withCredentials = buildBackup(store, { includeCredentials: true });
    assert.equal(withCredentials.credentials.session.accessToken, "access-token-123456");
    assert.equal(withCredentials.credentials.session.cookie, "cf_clearance=abc");

    assert.match(backupFilename(backup.exportedAt), /^turu-rc-backup-\d{8}T\d{6}Z\.json$/);
    assert.match(databaseFilename(backup.exportedAt), /^turu-rc-db-\d{8}T\d{6}Z\.db$/);
    store.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("parseBackup rejects foreign files and future versions", () => {
  assert.equal(parseBackup(null).ok, false);
  assert.equal(parseBackup([]).ok, false);
  assert.match(parseBackup({ kind: "not-ours" }).errors.kind, /not a TURU\.RC backup/);

  const futureFormat = parseBackup({
    kind: BACKUP_KIND,
    formatVersion: BACKUP_FORMAT_VERSION + 1,
    schemaVersion: SCHEMA_VERSION,
    data: {}
  });
  assert.equal(futureFormat.ok, false);
  assert.match(futureFormat.errors.formatVersion, /newer than this build/);

  const futureSchema = parseBackup({
    kind: BACKUP_KIND,
    formatVersion: BACKUP_FORMAT_VERSION,
    schemaVersion: SCHEMA_VERSION + 1,
    data: {}
  });
  assert.equal(futureSchema.ok, false);
  assert.match(futureSchema.errors.schemaVersion, /schema v/);

  const missingData = parseBackup({
    kind: BACKUP_KIND,
    formatVersion: BACKUP_FORMAT_VERSION,
    schemaVersion: SCHEMA_VERSION
  });
  assert.equal(missingData.ok, false);
  assert.match(missingData.errors.data, /no data block/);
});

test("a full round trip replaces data and writes a restorable safety snapshot", () => {
  const sourceDir = tempDir("src");
  const destDir = tempDir("dst");
  try {
    const source = seedStore(sourceDir, { targetId: TARGET_A, label: "NEON SHARD" });
    addOrder(source, "order-a");
    source.patchSettings({ dryRun: true, paused: true });
    source.state.stats.since = 1_700_000_000_000;
    source.state.stats.filled = 7;
    source.state.stats.failed = 2;
    source.state.stats.spentRlt = 4.25;
    source.state.stats.profitRlt = 1.5;
    source.flush();

    const parsed = parseBackup(buildBackup(source, { includeCredentials: true }));
    assert.equal(parsed.ok, true, parsed.warnings.join(" "));
    assert.deepEqual(parsed.warnings, []);

    const dest = seedStore(destDir, { targetId: TARGET_B, label: "PIXEL CORE", token: "destination-token-999" });
    addOrder(dest, "order-b", { itemId: TARGET_B, label: "PIXEL CORE" });
    dest.saveMarketSnapshot({
      itemId: TARGET_B,
      lastPrice: 0.04,
      lastStock: 12,
      itemType: "mutation_component",
      seenAt: Date.now()
    });
    dest.flush();
    assert.equal(Object.keys(dest.loadMarketSnapshots()).length, 1);

    const result = restoreBackup(dest, parsed.value, { now: 1_790_000_100_000 });
    assert.deepEqual(result.counts, { targets: 1, orders: 1, reportProducts: 1 });
    assert.equal(result.credentialsRestored, true);
    assert.match(result.safetyBackup, /^backups\/pre-import-\d{8}T\d{6}Z\.db$/);

    assert.equal(dest.state.configs.length, 1);
    assert.equal(dest.state.configs[0].itemId, TARGET_A);
    assert.equal(dest.state.configs[0].label, "NEON SHARD");
    assert.equal(dest.state.session.accessToken, "access-token-123456");
    assert.equal(dest.state.settings.dryRun, true);
    assert.equal(dest.state.settings.paused, true);
    assert.equal(dest.state.stats.filled, 7);
    assert.equal(dest.state.stats.failed, 2);
    assert.equal(dest.state.stats.spentRlt, 4.25);
    assert.equal(dest.state.stats.profitRlt, 1.5);
    assert.equal(dest.historyCount(), 1);
    assert.equal(dest.state.history[0].id, "order-a");
    assert.equal(dest.reportView().items.length, 1);
    assert.equal(dest.reportView().items[0].itemId, TARGET_A);
    assert.deepEqual(dest.loadMarketSnapshots(), {}, "stale market cache is dropped");

    // The safety file must contain the data that existed *before* the import.
    const safetyPath = join(destDir, result.safetyBackup);
    assert.equal(existsSync(safetyPath), true);
    if (process.platform !== "win32") {
      assert.equal(statSync(safetyPath).mode & 0o777, 0o600);
    }
    const safety = openSqlite(safetyPath, { readonly: true });
    assert.equal(safety.pragma("user_version", { simple: true }), SCHEMA_VERSION);
    assert.equal(safety.prepare("SELECT label FROM targets").get().label, "PIXEL CORE");
    assert.equal(safety.prepare("SELECT id FROM orders").get().id, "order-b");
    safety.close();

    // The restored database also survives a real reopen.
    dest.close();
    const reopened = new Store({ dataDir: destDir, logger: null }).load();
    assert.equal(reopened.state.configs[0].itemId, TARGET_A);
    assert.equal(reopened.state.session.accessToken, "access-token-123456");
    assert.equal(reopened.historyCount(), 1);
    reopened.close();
    source.close();
  } finally {
    rmSync(sourceDir, { recursive: true, force: true });
    rmSync(destDir, { recursive: true, force: true });
  }
});

test("restore can keep the current session while replacing everything else", () => {
  const sourceDir = tempDir("src");
  const destDir = tempDir("dst");
  try {
    const source = seedStore(sourceDir);
    const parsed = parseBackup(buildBackup(source, { includeCredentials: true }));
    const dest = seedStore(destDir, { targetId: TARGET_B, token: "keep-this-token" });

    restoreBackup(dest, parsed.value, { restoreCredentials: false });
    assert.equal(dest.state.session.accessToken, "keep-this-token");
    assert.equal(dest.state.configs[0].itemId, TARGET_A);

    // A backup without credentials must never clear the destination session.
    const anonymous = parseBackup(buildBackup(source));
    restoreBackup(dest, anonymous.value, { restoreCredentials: true });
    assert.equal(dest.state.session.accessToken, "keep-this-token");

    source.close();
    dest.close();
  } finally {
    rmSync(sourceDir, { recursive: true, force: true });
    rmSync(destDir, { recursive: true, force: true });
  }
});

test("parseBackup salvages valid rows, warns, and rebuilds a missing report", () => {
  const dir = tempDir();
  try {
    const store = seedStore(dir);
    addOrder(store, "order-valid");
    const backup = buildBackup(store, { includeCredentials: true });

    backup.data.targets.push({ itemId: "bad" });
    backup.data.orders.push({ id: "missing-at", orderId: "x", status: "filled" });
    backup.data.orders.push({ ...backup.data.orders[0], label: "DUPLICATE" });
    backup.data.settings.delayPerOrder = -10;
    delete backup.data.reportProducts;
    backup.counts = { targets: 99, orders: 99, reportProducts: 99 };

    const parsed = parseBackup(backup);
    assert.equal(parsed.ok, true);
    assert.ok(parsed.warnings.length >= 4, parsed.warnings.join("\n"));
    assert.ok(parsed.warnings.some((warning) => /Skipped target/.test(warning)));
    assert.ok(parsed.warnings.some((warning) => /Skipped loot entry/.test(warning)));
    assert.ok(parsed.warnings.some((warning) => /Skipped duplicate loot/.test(warning)));
    assert.ok(parsed.warnings.some((warning) => /claims 99/.test(warning)));
    assert.ok(parsed.warnings.some((warning) => /rebuilt from the loot log/.test(warning)));
    assert.equal(parsed.value.targets.length, 1);
    assert.equal(parsed.value.orders.length, 1);
    assert.equal(parsed.value.reportProducts.length, 1);
    assert.equal(parsed.value.settings.delayPerOrder, 200);

    restoreBackup(store, parsed.value);
    assert.equal(store.state.configs.length, 1);
    assert.equal(store.historyCount(), 1);
    assert.equal(store.reportView().items.length, 1);
    store.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a migration keeps the destination access key and never leaks key material", () => {
  const sourceDir = tempDir("src");
  const destDir = tempDir("dst");
  try {
    // The whole point of a portable backup: the target worker is already
    // running and someone has to be logged into it to upload the file.
    const source = seedStore(sourceDir);
    const sourceKey = source.ensureWorkerToken("");
    const destination = seedStore(destDir, { targetId: TARGET_B, label: "PIXEL CORE" });
    const destinationKey = destination.ensureWorkerToken("");

    const backup = buildBackup(source, { includeCredentials: true });
    const serialized = JSON.stringify(backup);
    assert.ok(!serialized.includes("worker_token"), "key material is never exported");
    assert.ok(!serialized.includes(sourceKey.preview));

    const parsed = parseBackup(backup);
    assert.equal(parsed.ok, true, parsed.warnings.join(" "));
    restoreBackup(destination, parsed.value);

    // The destination keeps its own key, so open tabs and bridge snippets on
    // the target worker stay valid after the data swap.
    assert.equal(destination.stmts.metaGet.get("worker_token_preview")?.value, destinationKey.preview);
    assert.notEqual(destination.stmts.metaGet.get("worker_token_preview")?.value, sourceKey.preview);
    assert.ok(destination.stmts.metaGet.get("worker_token_hash")?.value);
    assert.ok(destination.stmts.metaGet.get("bootstrapped")?.value);

    source.close();
    destination.close();
  } finally {
    rmSync(sourceDir, { recursive: true, force: true });
    rmSync(destDir, { recursive: true, force: true });
  }
});

test("a restore is atomic: a failure mid-import leaves the old data intact", () => {
  const dir = tempDir();
  try {
    const store = seedStore(dir, { targetId: TARGET_B, label: "PIXEL CORE" });
    addOrder(store, "order-keep");
    const before = store.historyCount();

    const parsed = parseBackup({
      kind: BACKUP_KIND,
      formatVersion: BACKUP_FORMAT_VERSION,
      schemaVersion: SCHEMA_VERSION,
      data: {
        targets: [
          {
            itemId: TARGET_A,
            label: "NEON SHARD",
            targetPrice: 0.1,
            desiredQty: 10,
            highStockThreshold: 20,
            minAcceptableQty: 1
          }
        ],
        orders: [],
        stats: {},
        settings: {}
      }
    });
    assert.equal(parsed.ok, true);

    const original = store.db.prepare.bind(store.db);
    let calls = 0;
    // Fail once the transaction is already replacing rows.
    store.db.prepare = (sql) => {
      calls += 1;
      if (calls === 6) throw new Error("simulated disk failure");
      return original(sql);
    };

    assert.throws(() => restoreBackup(store, parsed.value, { snapshot: false }), /simulated disk failure/);
    store.db.prepare = original;

    // Everything survived: the rollback restored the pre-import world.
    assert.equal(store.historyCount(), before);
    assert.equal(store.state.configs.length, 1);
    assert.equal(store.state.configs[0].itemId, TARGET_B);
    store.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("createDatabaseSnapshot writes a consistent standalone copy", () => {
  const dir = tempDir();
  try {
    const store = seedStore(dir);
    addOrder(store, "order-snapshot");
    const file = createDatabaseSnapshot(store, { prefix: "export", now: 1_790_000_000_000 });
    assert.match(file, /backups[/\\]export-\d{8}T\d{6}Z\.db$/);
    assert.equal(existsSync(file), true);

    const copy = openSqlite(file, { readonly: true });
    assert.equal(copy.pragma("journal_mode", { simple: true }), "delete");
    assert.equal(copy.prepare("SELECT count(*) AS n FROM orders").get().n, 1);
    assert.equal(copy.prepare("SELECT access_token FROM session WHERE id = 1").get().access_token, "access-token-123456");
    copy.close();
    store.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
