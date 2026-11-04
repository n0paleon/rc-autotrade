import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import Database from "better-sqlite3";

import { DbMetrics, sqlSnippet, statementKind } from "../src/worker/db-metrics.js";

function memoryDb() {
  const db = new Database(":memory:");
  db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)");
  return db;
}

test("statementKind categorises SQL and sqlSnippet normalises text", () => {
  assert.equal(statementKind("SELECT * FROM t"), "read");
  assert.equal(statementKind("  WITH x AS (SELECT 1) SELECT * FROM x"), "read");
  assert.equal(statementKind("-- comment\nINSERT INTO t (v) VALUES (?)"), "write");
  assert.equal(statementKind("delete from t"), "write");
  assert.equal(statementKind("PRAGMA journal_mode"), "pragma");
  assert.equal(statementKind("CREATE TABLE t (id)"), "schema");
  assert.equal(statementKind(""), "other");
  assert.equal(statementKind(undefined), "other");

  assert.equal(sqlSnippet("  SELECT\n  *   FROM t "), "SELECT * FROM t");
  const long = sqlSnippet("x".repeat(300));
  assert.equal(long.length, 120);
  assert.equal(long.endsWith("…"), true);
});

test("instrument times reads, writes and exposes SQLite storage", () => {
  const db = memoryDb();
  const metrics = new DbMetrics({ db, now: () => 1_000, slowMs: 20 }).instrument();

  db.prepare("INSERT INTO t (v) VALUES (?)").run("a");
  db.prepare("SELECT * FROM t WHERE v = ?").get("a");
  db.prepare("SELECT * FROM t").all();
  const view = metrics.sample();

  assert.equal(view.instrumented, true);
  assert.equal(view.queries.statements, 3);
  assert.equal(view.queries.write, 1);
  assert.equal(view.queries.read, 2);
  assert.equal(view.queries.errors, 0);
  assert.ok(view.queries.totalMs >= 0);
  assert.ok(view.queries.maxMs >= 0);
  assert.equal(view.window.count, 3);
  assert.equal(view.window.read, 2);
  assert.equal(view.window.write, 1);
  assert.equal(view.queries.avgMs >= 0, true);

  assert.equal(view.sqlite.journalMode, "memory");
  assert.ok(view.storage.pageSize > 0);
  assert.ok(view.storage.pageCount > 0);
  assert.ok(view.storage.dataBytes > 0);
  assert.equal(view.thresholds.slowMs, 20);
  assert.deepEqual(view.recentSlow, []);
  assert.ok(Array.isArray(view.history) && view.history.length === 1);

  db.close();
});

test("failed statements and slow queries are counted", () => {
  const db = memoryDb();
  const metrics = new DbMetrics({ db, now: () => 2_000, slowMs: 0 }).instrument();

  db.prepare("INSERT INTO t (v) VALUES (?)").run("dupe");
  assert.throws(() => db.prepare("INSERT INTO t (v) VALUES (?)").run("dupe"));
  db.prepare("SELECT * FROM t").all();

  const view = metrics.sample();
  assert.equal(view.queries.statements, 3);
  assert.equal(view.queries.errors, 1);
  assert.equal(view.window.errors, 1);
  assert.equal(view.queries.slow, 3, "slowMs 0 makes every statement slow");
  assert.equal(view.recentSlow.length, 3);
  assert.match(view.recentSlow[0].sql, /SELECT/);
  assert.equal(view.recentSlow.some((item) => item.error), true);

  db.close();
});

test("the rolling window ages out while totals survive", () => {
  const db = memoryDb();
  let now = 100_000;
  const metrics = new DbMetrics({ db, now: () => now, slowMs: 20 }).instrument();

  db.prepare("SELECT 1").get();
  metrics.sample();
  now += 61_000;
  const view = metrics.sample();

  assert.equal(view.window.count, 0);
  assert.equal(view.queries.statements, 1);
  assert.equal(view.history.length, 2, "each sample appends a history point");
  assert.equal(view.history[1].qps, 0);

  db.close();
});

test("transactions are timed separately from their statements", () => {
  const db = memoryDb();
  const metrics = new DbMetrics({ db, now: () => 3_000 }).instrument();

  const insert = db.transaction((value) => db.prepare("INSERT INTO t (v) VALUES (?)").run(value));
  insert("tx-1");
  const view = metrics.sample();

  assert.equal(view.transactions.count, 1);
  assert.equal(view.transactions.errors, 0);
  assert.ok(view.transactions.totalMs >= 0);
  assert.equal(view.queries.write, 1, "statements inside the transaction are still counted");

  db.close();
});

test("file sizes come from the db, wal and shm paths", () => {
  const dir = mkdtempSync(join(tmpdir(), "turuc-dbmetrics-"));
  const dbPath = join(dir, "probe.db");
  const db = new Database(dbPath);
  db.pragma("journal_mode = WAL");
  db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)");
  db.prepare("INSERT INTO t DEFAULT VALUES").run();

  const metrics = new DbMetrics({
    db,
    files: { db: dbPath, wal: `${dbPath}-wal`, shm: `${dbPath}-shm` },
    now: () => 4_000
  }).instrument();
  const view = metrics.sample();

  assert.equal(view.sqlite.journalMode, "wal");
  assert.ok(view.files.db > 0, "main database file must be measured");
  assert.ok(view.files.wal > 0, "WAL file must be measured");
  assert.equal(view.files.total, view.files.db + view.files.wal + view.files.shm);

  db.close();
  rmSync(dir, { recursive: true, force: true });
});

test("snapshot stays safe after the database is closed", () => {
  const db = memoryDb();
  const metrics = new DbMetrics({ db, now: () => 5_000 }).instrument();
  metrics.sample();
  db.close();

  const view = metrics.sample();
  assert.equal(view.sqlite.journalMode, null);
  assert.equal(view.instrumented, true);
  assert.doesNotThrow(() => JSON.stringify(view));
});
