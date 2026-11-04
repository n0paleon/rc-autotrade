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

function tempDir() {
  return mkdtempSync(join(tmpdir(), "turuc-sqlite-"));
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
    assert.equal(db.pragma("user_version", { simple: true }), 1);
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
