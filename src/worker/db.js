/**
 * SQLite persistence on Bun.
 *
 * The worker runs on Bun, so the database driver is Bun's built-in
 * `bun:sqlite` — no native addon, no node-gyp, no prebuilt binary that has to
 * match another runtime's ABI. This module wraps the small API the store was
 * written against (`prepare`/`pragma`/`transaction`/`inTransaction`/`open`) and
 * normalizes the places where Bun's driver is stricter or quieter than the old
 * `better-sqlite3` one:
 *
 *   named parameters  bun:sqlite needs the `@`/`:`/`$` prefix in the object
 *                     key (a bare key silently binds NULL), so bare keys are
 *                     mapped to the prefix used by the SQL
 *   unknown/missing   Bun silently binds NULL; the wrapper throws instead
 *   booleans          converted to SQLite's 1/0 so both call styles agree
 *   transactions      explicit savepoints make nested transactions behave
 *                     predictably (rollback only the inner scope)
 *   pragma            `db.pragma(name, { simple: true })` is implemented on top
 *                     of `PRAGMA` statements
 *
 * WAL keeps the control deck readable while the frame loop writes; NORMAL sync
 * trades an fsync per commit for durability across app crashes while staying
 * safe against power loss at the WAL level. A single worker process owns the
 * writer, which is exactly how it is deployed (PM2 fork, 1 instance).
 */

import { Database } from "bun:sqlite";
import { chmodSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { SCHEMA_SQL, SCHEMA_VERSION } from "./schema.js";

export const DB_FILENAME = "rollercoin.db";
export const DB_DRIVER = "bun:sqlite";

const CHMOD_TARGETS = ["", "-wal", "-shm"];

function secureFile(file) {
  if (process.platform === "win32") return;
  for (const suffix of CHMOD_TARGETS) {
    try {
      chmodSync(`${file}${suffix}`, 0o600);
    } catch {
      /* file may not exist yet (wal/shm) or fs does not support chmod */
    }
  }
}

function snippet(sql, limit = 100) {
  const text = String(sql).replace(/\s+/g, " ").trim();
  return text.length > limit ? `${text.slice(0, limit - 1)}…` : text;
}

function isNameChar(char) {
  return (
    (char >= "a" && char <= "z") ||
    (char >= "A" && char <= "Z") ||
    (char >= "0" && char <= "9") ||
    char === "_"
  );
}

/** Skips a quoted SQL string/identifier and returns the index of its last char. */
function skipQuoted(sql, start, quote) {
  let index = start + 1;
  while (index < sql.length) {
    if (sql[index] === quote) {
      if (sql[index + 1] === quote) {
        index += 2;
        continue;
      }
      return index;
    }
    index += 1;
  }
  return sql.length;
}

/**
 * Finds the named and positional placeholders in a statement while ignoring
 * quoted strings, quoted identifiers and comments. Only the placeholder shape
 * is needed: binding validation and prefix mapping live in `#bind()`.
 */
export function scanParameters(sql) {
  const text = String(sql);
  const named = new Map();
  let positional = false;

  for (let index = 0; index < text.length; index += 1) {
    const char = text[index];

    if (char === "'" || char === '"' || char === "`") {
      index = skipQuoted(text, index, char);
      continue;
    }
    if (char === "[") {
      const end = text.indexOf("]", index + 1);
      index = end === -1 ? text.length : end;
      continue;
    }
    if (char === "-" && text[index + 1] === "-") {
      const end = text.indexOf("\n", index + 2);
      index = end === -1 ? text.length : end;
      continue;
    }
    if (char === "/" && text[index + 1] === "*") {
      const end = text.indexOf("*/", index + 2);
      index = end === -1 ? text.length : end + 1;
      continue;
    }
    if (char === "?") {
      positional = true;
      while (text[index + 1] >= "0" && text[index + 1] <= "9") index += 1;
      continue;
    }
    if (char === "@" || char === ":" || char === "$") {
      let end = index + 1;
      while (end < text.length && isNameChar(text[end])) end += 1;
      if (end > index + 1) {
        const name = text.slice(index + 1, end);
        if (!named.has(name)) named.set(name, char + name);
        index = end - 1;
      }
    }
  }

  return { named, positional };
}

function isNamedObject(value) {
  if (value === null || typeof value !== "object") return false;
  if (Array.isArray(value) || Buffer.isBuffer(value) || value instanceof Uint8Array) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function bindValue(value) {
  if (typeof value === "boolean") return value ? 1 : 0;
  if (value === undefined) return null;
  return value;
}

function toNumber(value) {
  const number = Number(value);
  return Number.isFinite(number) ? number : 0;
}

/** A prepared statement that accepts bare or prefixed named parameters. */
class SqliteStatement {
  #statement;
  #sql;
  #parameters;

  constructor(statement, sql) {
    this.#statement = statement;
    this.#sql = String(sql);
    this.#parameters = scanParameters(this.#sql);
  }

  #bind(args) {
    const { named, positional } = this.#parameters;

    if (args.length === 1 && isNamedObject(args[0])) {
      if (named.size === 0) {
        throw new TypeError(`SQLite statement has no named parameters: ${snippet(this.#sql)}`);
      }
      if (positional) {
        throw new TypeError(
          `SQLite statement mixes named and positional parameters: ${snippet(this.#sql)}`
        );
      }

      const values = {};
      const supplied = new Set();
      for (const [rawKey, rawValue] of Object.entries(args[0])) {
        const name =
          rawKey.startsWith("@") || rawKey.startsWith(":") || rawKey.startsWith("$")
            ? rawKey.slice(1)
            : rawKey;
        const token = named.get(name);
        if (!token) {
          throw new TypeError(
            `Unknown named parameter "${rawKey}" for SQLite statement: ${snippet(this.#sql)}`
          );
        }
        if (supplied.has(name)) continue;
        supplied.add(name);
        // bun:sqlite matches named parameters by their prefixed token.
        values[token] = bindValue(rawValue);
      }

      for (const name of named.keys()) {
        if (!supplied.has(name)) {
          throw new TypeError(
            `Missing named parameter "${name}" for SQLite statement: ${snippet(this.#sql)}`
          );
        }
      }
      return [values];
    }

    if (named.size > 0 && !positional) {
      throw new TypeError(
        `SQLite statement expects named parameters (${[...named.keys()].join(", ")}): ${snippet(this.#sql)}`
      );
    }

    return args.map(bindValue);
  }

  run(...args) {
    const result = this.#statement.run(...this.#bind(args));
    return {
      changes: toNumber(result?.changes),
      lastInsertRowid: toNumber(result?.lastInsertRowid)
    };
  }

  get(...args) {
    const row = this.#statement.get(...this.#bind(args));
    return row == null ? undefined : { ...row };
  }

  all(...args) {
    return this.#statement.all(...this.#bind(args)).map((row) => ({ ...row }));
  }

  *iterate(...args) {
    for (const row of this.#statement.iterate(...this.#bind(args))) {
      yield { ...row };
    }
  }
}

/**
 * Database handle with the call surface used by the store and the metrics
 * probe. Statements are created lazily from `prepare()`, exactly like SQLite
 * itself does, so instrumentation can wrap the methods before hot statements
 * are prepared.
 */
export class SqliteDatabase {
  #db;
  #file;
  #readonly;
  #open = true;
  #transactionDepth = 0;

  constructor(file, { readonly = false } = {}) {
    this.#file = file;
    this.#readonly = readonly;
    this.#db = readonly
      ? new Database(file, { readonly: true, create: false })
      : new Database(file);
  }

  get driver() {
    return DB_DRIVER;
  }

  get name() {
    return this.#file;
  }

  get open() {
    return this.#open;
  }

  get readonly() {
    return this.#readonly;
  }

  get inTransaction() {
    return this.#transactionDepth > 0 || Boolean(this.#db.inTransaction);
  }

  #assertOpen() {
    if (!this.#open) {
      throw new Error(`SQLite database at ${this.#file} is already closed`);
    }
  }

  prepare(sql) {
    this.#assertOpen();
    return new SqliteStatement(this.#db.prepare(String(sql)), sql);
  }

  exec(sql) {
    this.#assertOpen();
    this.#db.exec(String(sql));
    return this;
  }

  /**
   * Runs `PRAGMA <source>`. With `{ simple: true }` the first column of the
   * first row is returned (matching the old driver's helper); otherwise the
   * rows are returned.
   */
  pragma(source, options = {}) {
    this.#assertOpen();
    const text = String(source).replace(/^PRAGMA\s+/i, "").replace(/;+\s*$/, "");
    const rows = this.#db.prepare(`PRAGMA ${text}`).all();
    if (options?.simple) {
      if (rows.length === 0) return undefined;
      const key = Object.keys(rows[0])[0];
      return key === undefined ? undefined : rows[0][key];
    }
    return rows.map((row) => ({ ...row }));
  }

  /**
   * Wraps `fn` in a transaction and returns a callable. Nested calls use
   * savepoints, so an inner failure rolls back only the inner scope and the
   * outer transaction can still commit.
   */
  transaction(fn) {
    if (typeof fn !== "function") throw new TypeError("transaction() requires a function");
    const db = this;

    return function runTransaction(...args) {
      db.#assertOpen();
      const depth = db.#transactionDepth;
      const savepoint = `rc_tx_${depth}`;
      db.exec(depth === 0 ? "BEGIN" : `SAVEPOINT ${savepoint}`);
      db.#transactionDepth = depth + 1;

      let result;
      try {
        result = fn.apply(null, args);
        if (result && typeof result.then === "function") {
          throw new TypeError("SQLite transactions must be synchronous");
        }
      } catch (error) {
        db.#transactionDepth = depth;
        try {
          if (depth === 0) {
            db.exec("ROLLBACK");
          } else {
            db.exec(`ROLLBACK TO ${savepoint}`);
            db.exec(`RELEASE ${savepoint}`);
          }
        } catch {
          /* the original error is more useful than a rollback failure */
        }
        throw error;
      }

      db.#transactionDepth = depth;
      try {
        db.exec(depth === 0 ? "COMMIT" : `RELEASE ${savepoint}`);
      } catch (error) {
        try {
          if (depth === 0) db.exec("ROLLBACK");
        } catch {
          /* already rolled back */
        }
        throw error;
      }
      return result;
    };
  }

  close() {
    if (!this.#open) return this;
    this.#open = false;
    this.#db.close();
    return this;
  }
}

/** Opens a database handle without touching pragmas or the schema. */
export function openSqlite(file, { readonly = false } = {}) {
  try {
    return new SqliteDatabase(file, { readonly });
  } catch (error) {
    throw new Error(`could not open SQLite database at ${file}: ${error.message}`);
  }
}

/**
 * Forward-only schema upgrades. Each entry runs when the database is older
 * than SCHEMA_VERSION; `addColumnIfMissing` keeps them safe to re-run even if
 * an older build managed to create some of the columns already.
 *
 * v2 shipped once without its migration, so a database could be stamped
 * `user_version = 2` while `targets` still lacked the columns. Migration 2
 * exists purely to repair that state; v1 gets the same repair through 1.
 */
function addAutoTargetColumns(db) {
  addColumnIfMissing(db, "targets", "auto_target", "INTEGER NOT NULL DEFAULT 0");
  addColumnIfMissing(db, "targets", "auto_target_margin_pct", "REAL NOT NULL DEFAULT 0");
}

const MIGRATIONS = {
  1: addAutoTargetColumns,
  2: addAutoTargetColumns
};

function addColumnIfMissing(db, table, column, definition) {
  const columns = db.pragma(`table_info(${table})`);
  if (columns.some((row) => row.name === column)) return;
  db.exec(`ALTER TABLE ${table} ADD COLUMN ${column} ${definition}`);
}

function migrate(db, fromVersion) {
  for (let version = fromVersion; version < SCHEMA_VERSION; version += 1) {
    const migration = MIGRATIONS[version];
    if (!migration) continue;
    db.transaction(() => migration(db))();
  }
}

/**
 * Opens (and bootstraps) the SQLite database that replaces state.json.
 */
export function openDatabase({ dataDir, logger, filename = DB_FILENAME }) {
  mkdirSync(dataDir, { recursive: true, mode: 0o700 });
  const file = join(dataDir, filename);

  const db = openSqlite(file);

  db.pragma("journal_mode = WAL");
  db.pragma("synchronous = NORMAL");
  db.pragma("busy_timeout = 5000");
  db.pragma("foreign_keys = ON");

  const version = Number(db.pragma("user_version", { simple: true })) || 0;
  if (version > SCHEMA_VERSION) {
    db.close();
    throw new Error(
      `database schema v${version} is newer than this build (v${SCHEMA_VERSION}); upgrade the worker instead of downgrading the data`
    );
  }

  db.exec(SCHEMA_SQL);
  migrate(db, version);
  if (version !== SCHEMA_VERSION) db.pragma(`user_version = ${SCHEMA_VERSION}`);

  secureFile(file);
  logger?.debug?.("db", `opened ${file} via ${DB_DRIVER} (schema v${SCHEMA_VERSION})`);
  return db;
}

/** Checkpoints the WAL so a copied .db file is self-contained, then closes. */
export function closeDatabase(db) {
  if (!db || !db.open) return;
  try {
    db.pragma("wal_checkpoint(TRUNCATE)");
  } catch {
    /* best effort: a busy checkpoint must not block shutdown */
  }
  try {
    db.close();
  } catch {
    /* already closed */
  }
}
