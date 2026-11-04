import Database from "better-sqlite3";
import { chmodSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { SCHEMA_SQL, SCHEMA_VERSION } from "./schema.js";

export const DB_FILENAME = "rollercoin.db";

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

/**
 * Opens (and bootstraps) the SQLite database that replaces state.json.
 *
 * WAL keeps the control deck readable while the frame loop writes; NORMAL
 * sync trades an fsync per commit for durability across app crashes while
 * staying safe against power loss at the WAL level. A single process owns the
 * writer, which is exactly how the worker is deployed (PM2 fork, 1 instance).
 */
export function openDatabase({ dataDir, logger, filename = DB_FILENAME }) {
  mkdirSync(dataDir, { recursive: true, mode: 0o700 });
  const file = join(dataDir, filename);

  let db;
  try {
    db = new Database(file);
  } catch (error) {
    throw new Error(`could not open SQLite database at ${file}: ${error.message}`);
  }

  db.pragma("journal_mode = WAL");
  db.pragma("synchronous = NORMAL");
  db.pragma("busy_timeout = 5000");
  db.pragma("foreign_keys = ON");

  const version = db.pragma("user_version", { simple: true });
  if (version > SCHEMA_VERSION) {
    db.close();
    throw new Error(
      `database schema v${version} is newer than this build (v${SCHEMA_VERSION}); upgrade the worker instead of downgrading the data`
    );
  }

  db.exec(SCHEMA_SQL);
  if (version !== SCHEMA_VERSION) db.pragma(`user_version = ${SCHEMA_VERSION}`);

  secureFile(file);
  logger?.debug?.("db", `opened ${file} (schema v${SCHEMA_VERSION})`);
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
