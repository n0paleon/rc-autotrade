import { test } from "bun:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  closeDatabase,
  DB_DRIVER,
  openDatabase,
  openSqlite,
  scanParameters
} from "../src/worker/db.js";
import { SCHEMA_VERSION } from "../src/worker/schema.js";

function tempDir() {
  return mkdtempSync(join(tmpdir(), "turuc-sqlite-"));
}

const LEGACY_TARGETS_SQL = `
  CREATE TABLE targets (
    item_id              TEXT PRIMARY KEY,
    label                TEXT NOT NULL DEFAULT '',
    item_type            TEXT NOT NULL DEFAULT 'mutation_component',
    rarity_color         TEXT,
    target_price         REAL NOT NULL,
    desired_qty          INTEGER NOT NULL,
    high_stock_threshold INTEGER NOT NULL,
    min_acceptable_qty   INTEGER NOT NULL,
    allow_partial_order  INTEGER NOT NULL DEFAULT 1,
    active               INTEGER NOT NULL DEFAULT 1,
    created_at           INTEGER,
    updated_at           INTEGER
  )
`;

/** Writes the pre-auto-target `targets` table and stamps a schema version. */
function seedLegacyTargets(dir, version) {
  const db = openSqlite(join(dir, "rollercoin.db"));
  db.exec(LEGACY_TARGETS_SQL);
  db.prepare(`
    INSERT INTO targets
      (item_id, label, target_price, desired_qty, high_stock_threshold,
       min_acceptable_qty, allow_partial_order, active)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?)
  `).run("6571aa39ca8bfef34adcd9e9", "LEGACY", 0.1, 10, 20, 1, 1, 1);
  db.pragma(`user_version = ${version}`);
  db.close();
}

test("database reports the Bun driver and a live open flag", () => {
  const db = openSqlite(":memory:");
  assert.equal(db.driver, DB_DRIVER);
  assert.equal(db.driver, "bun:sqlite");
  assert.equal(db.open, true);
  assert.equal(db.inTransaction, false);

  db.close();
  assert.equal(db.open, false);
  assert.throws(() => db.prepare("SELECT 1"), /already closed/);
});

test("named parameters accept bare and prefixed keys", () => {
  const db = openSqlite(":memory:");
  db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, flag INTEGER)");
  const insert = db.prepare("INSERT INTO t (name, flag) VALUES (@name, @flag)");
  insert.run({ name: "bare", flag: true });
  insert.run({ "@name": "prefixed", flag: false });

  assert.deepEqual(db.prepare("SELECT name, flag FROM t ORDER BY id").all(), [
    { name: "bare", flag: 1 },
    { name: "prefixed", flag: 0 }
  ]);
  assert.equal(db.prepare("SELECT name FROM t WHERE name = @name").get({ name: "bare" }).name, "bare");
  assert.equal(db.prepare("SELECT name FROM t WHERE name = :name").get({ ":name": "prefixed" }).name, "prefixed");
  db.close();
});

test("unknown and missing named parameters throw instead of binding NULL", () => {
  const db = openSqlite(":memory:");
  db.exec("CREATE TABLE t (a INTEGER, b INTEGER)");
  const insert = db.prepare("INSERT INTO t (a, b) VALUES (@a, @b)");

  assert.throws(() => insert.run({ a: 1 }), /Missing named parameter "b"/);
  assert.throws(() => insert.run({ a: 1, b: 2, c: 3 }), /Unknown named parameter "c"/);
  assert.equal(db.prepare("SELECT COUNT(*) AS n FROM t").get().n, 0);

  // Bare and prefixed keys can be mixed when the caller has a reason to.
  insert.run({ a: 1, "@b": 2 });
  assert.deepEqual(db.prepare("SELECT a, b FROM t").get(), { a: 1, b: 2 });
  db.close();
});

test("booleans and undefined are bound like the old Node driver", () => {
  const db = openSqlite(":memory:");
  assert.deepEqual(db.prepare("SELECT ? AS one, ? AS zero, ? AS empty").get(true, false, undefined), {
    one: 1,
    zero: 0,
    empty: null
  });
  db.close();
});

test("pragma supports reads, writes and simple mode", () => {
  const db = openSqlite(":memory:");
  assert.equal(db.pragma("user_version", { simple: true }), 0);
  db.pragma("user_version = 7");
  assert.equal(db.pragma("user_version", { simple: true }), 7);
  assert.deepEqual(db.pragma("user_version"), [{ user_version: 7 }]);
  db.close();
});

test("nested transactions use savepoints and only roll back the inner scope", () => {
  const db = openSqlite(":memory:");
  db.exec("CREATE TABLE t (v TEXT)");

  const insert = (value) => db.prepare("INSERT INTO t (v) VALUES (?)").run(value);
  const outer = db.transaction(() => {
    insert("outer");
    const inner = db.transaction(() => {
      insert("inner");
      throw new Error("inner failed");
    });
    assert.throws(() => inner(), /inner failed/);
    insert("after");
    return "committed";
  });

  assert.equal(outer(), "committed");
  assert.deepEqual(db.prepare("SELECT v FROM t ORDER BY rowid").all(), [
    { v: "outer" },
    { v: "after" }
  ]);

  const failing = db.transaction(() => {
    insert("rolled-back");
    throw new Error("outer failed");
  });
  assert.throws(() => failing(), /outer failed/);
  assert.equal(db.prepare("SELECT COUNT(*) AS n FROM t").get().n, 2);
  assert.equal(db.inTransaction, false);
  db.close();
});

test("scanParameters ignores placeholders inside strings and comments", () => {
  const params = scanParameters(
    "SELECT '@no', ?, :yes FROM t -- ? @no\n WHERE x = @x /* :no */ AND y = ?2"
  );
  assert.equal(params.positional, true);
  assert.deepEqual([...params.named.entries()], [
    ["yes", ":yes"],
    ["x", "@x"]
  ]);
});

test("openDatabase bootstraps the schema and readonly handles reject writes", () => {
  const dir = tempDir();
  try {
    const db = openDatabase({ dataDir: dir, logger: null });
    assert.equal(db.pragma("journal_mode", { simple: true }), "wal");
    assert.equal(db.pragma("user_version", { simple: true }), SCHEMA_VERSION);
    db.prepare("INSERT INTO meta (key, value) VALUES (@key, @value)").run({ key: "k", value: "v" });
    closeDatabase(db);
    assert.equal(db.open, false);

    const readonly = openSqlite(join(dir, "rollercoin.db"), { readonly: true });
    assert.equal(readonly.readonly, true);
    assert.equal(readonly.prepare("SELECT value FROM meta WHERE key = ?").get("k").value, "v");
    assert.throws(() => readonly.prepare("DELETE FROM meta").run());
    readonly.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("openDatabase migrates a v1 targets table without losing rows", () => {
  const dir = tempDir();
  try {
    seedLegacyTargets(dir, 1);

    const db = openDatabase({ dataDir: dir, logger: null });
    assert.equal(db.pragma("user_version", { simple: true }), SCHEMA_VERSION);
    const columns = db.pragma("table_info(targets)").map((column) => column.name);
    assert.ok(columns.includes("auto_target"));
    assert.ok(columns.includes("auto_target_margin_pct"));

    const row = db
      .prepare("SELECT * FROM targets WHERE item_id = ?")
      .get("6571aa39ca8bfef34adcd9e9");
    assert.equal(row.label, "LEGACY");
    assert.equal(row.auto_target, 0);
    assert.equal(row.auto_target_margin_pct, 0);
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("openDatabase repairs a v2 database stamped without the column migration", () => {
  const dir = tempDir();
  try {
    // A partially updated build could bump user_version to 2 while leaving
    // targets on the legacy shape. The repair migration must still add the
    // columns instead of trusting the stamped version.
    seedLegacyTargets(dir, 2);

    const db = openDatabase({ dataDir: dir, logger: null });
    assert.equal(db.pragma("user_version", { simple: true }), SCHEMA_VERSION);
    const columns = db.pragma("table_info(targets)").map((column) => column.name);
    assert.ok(columns.includes("auto_target"));
    assert.ok(columns.includes("auto_target_margin_pct"));

    const row = db
      .prepare("SELECT * FROM targets WHERE item_id = ?")
      .get("6571aa39ca8bfef34adcd9e9");
    assert.equal(row.label, "LEGACY");
    assert.equal(row.auto_target, 0);
    assert.equal(row.auto_target_margin_pct, 0);
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
