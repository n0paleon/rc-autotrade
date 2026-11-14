//! SQLite bootstrap, ported from `src/worker/db.js` + `src/worker/schema.js`.
//!
//! The Rust worker opens the *same* database format as the Bun worker: same
//! tables, same `user_version`, same WAL settings. That is what makes the
//! side-by-side comparison (and eventually the cutover) possible.

use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::logger::Logger;

pub const SCHEMA_VERSION: i64 = 8;
pub const DB_FILENAME: &str = "rollercoin.db";

/// The single trading switch. `dry_run` and `paused` stay in the table as
/// read-only mirrors so a pre-v8 build (or the JS worker during cutover) keeps
/// seeing a coherent pair of flags; see `Mode::legacy_flags`.
pub const MODE_OFF: &str = "off";
pub const MODE_SHADOW: &str = "shadow";
pub const MODE_LIVE: &str = "live";

pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS session (
  id              INTEGER PRIMARY KEY CHECK (id = 1),
  access_token    TEXT NOT NULL DEFAULT '',
  refresh_token   TEXT NOT NULL DEFAULT '',
  cookie          TEXT NOT NULL DEFAULT '',
  source          TEXT,
  updated_at      INTEGER,
  last_refresh_at INTEGER,
  refresh_count   INTEGER NOT NULL DEFAULT 0,
  last_error      TEXT
);

CREATE TABLE IF NOT EXISTS settings (
  id                    INTEGER PRIMARY KEY CHECK (id = 1),
  delay_per_order       INTEGER NOT NULL,
  max_orders_per_minute INTEGER NOT NULL,
  dry_run               INTEGER NOT NULL DEFAULT 0,
  auto_refresh          INTEGER NOT NULL DEFAULT 1,
  paused                INTEGER NOT NULL DEFAULT 0,
  -- Added in schema v8: the only field that decides behaviour. The two columns
  -- above are kept in sync as mirrors for pre-v8 readers.
  mode                  TEXT NOT NULL DEFAULT 'live',
  -- Added after schema v8: percentage of headroom added to the purchase
  -- request's totalPrice so the marketplace can fill the whole order instead of
  -- a partial slice.
  total_price_markup_pct REAL NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS targets (
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
);

CREATE TABLE IF NOT EXISTS stats (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  since       INTEGER NOT NULL,
  profit_rlt  REAL NOT NULL DEFAULT 0,
  spent_rlt   REAL NOT NULL DEFAULT 0,
  bought      INTEGER NOT NULL DEFAULT 0,
  filled      INTEGER NOT NULL DEFAULT 0,
  failed      INTEGER NOT NULL DEFAULT 0,
  orders_sent INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS orders (
  id           TEXT PRIMARY KEY,
  order_id     TEXT,
  item_id      TEXT NOT NULL DEFAULT '',
  label        TEXT NOT NULL DEFAULT '',
  qty          INTEGER NOT NULL DEFAULT 0,
  requested_qty INTEGER,
  stock_at_trigger INTEGER,
  unit_price   REAL NOT NULL DEFAULT 0,
  total_price  REAL NOT NULL DEFAULT 0,
  target_price REAL NOT NULL DEFAULT 0,
  profit_rlt   REAL NOT NULL DEFAULT 0,
  status       TEXT NOT NULL,
  at           INTEGER NOT NULL,
  latency_ms   INTEGER,
  timings      TEXT,
  raw_events   TEXT,
  note         TEXT,
  message      TEXT
);

CREATE INDEX IF NOT EXISTS orders_at ON orders (at DESC);
CREATE INDEX IF NOT EXISTS orders_item_at ON orders (item_id, at DESC);
-- The loot log pages on `at` with `id` as the tiebreak. Without both columns in
-- the index SQLite sorts every page through a temp B-tree.
CREATE INDEX IF NOT EXISTS orders_at_id ON orders (at DESC, id DESC);

CREATE TABLE IF NOT EXISTS report_products (
  product_key TEXT PRIMARY KEY,
  item_id     TEXT NOT NULL DEFAULT '',
  label       TEXT NOT NULL DEFAULT '',
  fills       INTEGER NOT NULL DEFAULT 0,
  failed      INTEGER NOT NULL DEFAULT 0,
  qty         INTEGER NOT NULL DEFAULT 0,
  spent_rlt   REAL NOT NULL DEFAULT 0,
  profit_rlt  REAL NOT NULL DEFAULT 0,
  first_at    INTEGER NOT NULL,
  last_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS market_snapshot (
  item_id           TEXT PRIMARY KEY REFERENCES targets (item_id) ON DELETE CASCADE,
  last_price        REAL,
  last_stock        INTEGER,
  allowed_price_min REAL,
  allowed_price_max REAL,
  allowed_at        INTEGER,
  item_type         TEXT,
  seen_at           INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS price_points (
  item_id    TEXT NOT NULL,
  bucket_at  INTEGER NOT NULL,
  price      REAL NOT NULL,
  low_price  REAL,
  high_price REAL,
  raw_low_price  REAL,
  raw_high_price REAL,
  fill_price REAL,
  fill_qty   REAL,
  fill_approx INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (item_id, bucket_at)
) WITHOUT ROWID;

-- Retention deletes by time alone, which the (item_id, bucket_at) primary key
-- cannot serve: without this index every prune scans the whole table.
CREATE INDEX IF NOT EXISTS price_points_bucket_at ON price_points (bucket_at);

-- Market-frame activity, one row per 5-minute bucket. Additive: older builds
-- that know schema v7 simply ignore the table, so `user_version` stays put.
CREATE TABLE IF NOT EXISTS event_points (
  bucket_at INTEGER PRIMARY KEY,
  events    INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

-- Per-item market-frame activity, one row per item per hour. Keyed by item id
-- instead of a single counter so the item monitor can rank the busiest ids in
-- any date range, including ids that were never added to the target fleet.
-- Additive like `event_points`; `user_version` does not move.
CREATE TABLE IF NOT EXISTS item_event_points (
  item_id     TEXT NOT NULL,
  bucket_at   INTEGER NOT NULL,
  item_type   TEXT,
  events      INTEGER NOT NULL DEFAULT 0,
  last_seen_at INTEGER,
  PRIMARY KEY (item_id, bucket_at)
) WITHOUT ROWID;

-- Retention deletes by time alone, which the (item_id, bucket_at) primary key
-- cannot serve. Mirrors `price_points_bucket_at`.
CREATE INDEX IF NOT EXISTS item_event_points_bucket_at ON item_event_points (bucket_at);

-- Last-seen metadata for every marketplace item the feed has shown, including
-- ids that never entered the target fleet. The name is resolved lazily from
-- item-info and cached here so the monitor can print a product name instead of
-- a 24-character id without calling upstream on every render.
CREATE TABLE IF NOT EXISTS item_catalog (
  item_id      TEXT PRIMARY KEY,
  item_type    TEXT NOT NULL DEFAULT 'mutation_component',
  label        TEXT NOT NULL DEFAULT '',
  rarity       TEXT,
  rarity_color TEXT,
  first_seen_at INTEGER,
  last_seen_at  INTEGER,
  resolved_at   INTEGER,
  attempts      INTEGER NOT NULL DEFAULT 0,
  last_attempt_at INTEGER
);
"#;

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, rusqlite::Error> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<bool, rusqlite::Error> {
    if column_exists(conn, table, column)? {
        return Ok(false);
    }
    conn.execute_batch(&format!(
        "ALTER TABLE {table} ADD COLUMN {column} {definition}"
    ))?;
    Ok(true)
}

/// Forward-only upgrades for databases created by older JavaScript builds.
fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    add_column_if_missing(conn, "price_points", "fill_price", "REAL")?;
    add_column_if_missing(conn, "price_points", "fill_qty", "REAL")?;
    add_column_if_missing(
        conn,
        "price_points",
        "fill_approx",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "price_points", "low_price", "REAL")?;
    add_column_if_missing(conn, "price_points", "high_price", "REAL")?;
    add_column_if_missing(conn, "price_points", "raw_low_price", "REAL")?;
    add_column_if_missing(conn, "price_points", "raw_high_price", "REAL")?;
    add_column_if_missing(conn, "orders", "timings", "TEXT")?;
    add_column_if_missing(conn, "orders", "raw_events", "TEXT")?;
    // Separate the sent quantity from the quantity actually obtained. Old
    // filled rows only recorded the fill count in `qty`, so the sent quantity
    // is unrecoverable there; those rows keep NULL and the UI falls back to
    // `qty`. Failed rows already stored the attempted quantity in `qty` and
    // obtained nothing, so they can be backfilled exactly: preserve `qty`
    // into `requested_qty`, then zero `qty` because the fulfillment result
    // event count for a failure is 0.
    add_column_if_missing(conn, "orders", "requested_qty", "INTEGER")?;
    // Stock available when the signal triggered. Rows written before this
    // column existed keep NULL and the loot log renders "—".
    add_column_if_missing(conn, "orders", "stock_at_trigger", "INTEGER")?;
    conn.execute_batch(
        "UPDATE orders SET requested_qty = qty WHERE requested_qty IS NULL AND status = 'failed'",
    )?;
    // Correct the legacy double-meaning: old failed rows stored the attempted
    // count in qty. That count now lives in requested_qty, so zero qty for
    // failed rows to match the fulfillment event count (0 obtained).
    conn.execute_batch("UPDATE orders SET qty = 0 WHERE status = 'failed'")?;

    // The auto-target feature is gone; drop its columns if a legacy DB has them.
    for column in ["auto_target", "auto_target_margin_pct"] {
        if column_exists(conn, "targets", column)? {
            conn.execute_batch(&format!("ALTER TABLE targets DROP COLUMN {column}"))?;
        }
    }

    // The item monitor rotates its name-resolution batch by attempt count, so a
    // few unnameable ids cannot starve the rest of the table. A database created
    // before these columns existed gets them here.
    add_column_if_missing(
        conn,
        "item_catalog",
        "attempts",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "item_catalog", "last_attempt_at", "INTEGER")?;

    // Schema v8 unifies `paused` + `dry_run` into `settings.mode`. When the
    // upgrade has to do the mapping, the two legacy columns are still the
    // source of truth, so translate them once and rewrite the mirrors from the
    // resulting mode. Otherwise the old flags would contradict `mode`.
    if add_column_if_missing(
        conn,
        "settings",
        "mode",
        &format!("TEXT NOT NULL DEFAULT '{MODE_LIVE}'"),
    )? {
        conn.execute_batch(&format!(
            "UPDATE settings SET mode = CASE
                 WHEN paused <> 0 THEN '{MODE_OFF}'
                 WHEN dry_run <> 0 THEN '{MODE_SHADOW}'
                 ELSE '{MODE_LIVE}'
             END WHERE id = 1"
        ))?;
    }
    conn.execute_batch(&format!(
        "UPDATE settings SET
             dry_run = CASE mode WHEN '{MODE_LIVE}' THEN 0 ELSE 1 END,
             paused  = CASE mode WHEN '{MODE_OFF}' THEN 1 ELSE 0 END
         WHERE id = 1"
    ))?;

    // Fulfillment headroom. Existing databases start at 0 (off), so an
    // upgrade never raises the price ceiling of a running worker.
    add_column_if_missing(
        conn,
        "settings",
        "total_price_markup_pct",
        "REAL NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

#[cfg(unix)]
fn secure(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn secure(_path: &Path) {}

/// Takes an exclusive advisory lock on the data directory.
///
/// Two workers against one database would each keep their own pending-order
/// state and could submit the same purchase twice, so the second process is
/// refused instead of being allowed to trade. The lock is held by the returned
/// handle and released when `Store` is dropped; the OS releases it even if the
/// process crashes.
pub fn lock_data_dir(data_dir: &Path) -> Result<fs::File, String> {
    fs::create_dir_all(data_dir)
        .map_err(|error| format!("could not create {}: {error}", data_dir.display()))?;
    let path = data_dir.join(".worker.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(format!(
            "another worker already owns {}; only one process may use a data directory",
            data_dir.display()
        )),
        Err(fs::TryLockError::Error(error)) => {
            Err(format!("could not lock {}: {error}", path.display()))
        }
    }
}

/// Opens (and bootstraps) the SQLite database that replaces `state.json`.
pub fn open(
    data_dir: &Path,
    logger: &Logger,
    metrics: Option<std::sync::Arc<crate::db_metrics::DbMetrics>>,
) -> Result<Connection, String> {
    fs::create_dir_all(data_dir)
        .map_err(|error| format!("could not create {}: {error}", data_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700));
    }

    let file = data_dir.join(DB_FILENAME);
    let mut conn = Connection::open(&file).map_err(|error| {
        format!(
            "could not open SQLite database at {}: {error}",
            file.display()
        )
    })?;

    conn.busy_timeout(Duration::from_millis(5_000))
        .map_err(|error| format!("could not set busy_timeout: {error}"))?;
    // `journal_mode` returns a row, so it cannot go through `pragma_update`.
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|error| format!("could not enable WAL: {error}"))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|error| format!("could not set synchronous: {error}"))?;
    // WAL auto-checkpoints every ~1000 pages, but the file itself is only
    // truncated when a checkpoint finishes. Capping the journal keeps the
    // on-disk footprint predictable without changing any stored row.
    conn.pragma_update(None, "journal_size_limit", 32 * 1024 * 1024_i64)
        .map_err(|error| format!("could not set journal_size_limit: {error}"))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|error| format!("could not enable foreign keys: {error}"))?;

    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);
    if version > SCHEMA_VERSION {
        return Err(format!(
            "database schema v{version} is newer than this build (v{SCHEMA_VERSION}); upgrade the worker instead of downgrading the data"
        ));
    }

    conn.execute_batch(SCHEMA_SQL)
        .map_err(|error| format!("could not apply schema: {error}"))?;
    migrate(&conn).map_err(|error| format!("could not migrate schema: {error}"))?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|error| format!("could not stamp user_version: {error}"))?;

    for suffix in ["", "-wal", "-shm"] {
        secure(Path::new(&format!("{}{suffix}", file.display())));
    }

    if let Some(metrics) = metrics {
        use rusqlite::trace::{TraceEvent, TraceEventCodes};
        conn.trace_v2(
            TraceEventCodes::SQLITE_TRACE_PROFILE,
            Some(move |event: TraceEvent<'_>| {
                if let TraceEvent::Profile(statement, duration) = event {
                    metrics.record(&statement.sql(), duration);
                }
            }),
        )
        .map_err(|error| format!("could not install SQLite metrics: {error}"))?;
    }

    logger.debug(
        "db",
        format!(
            "opened {} via rusqlite (schema v{SCHEMA_VERSION}, journal {mode})",
            file.display()
        ),
    );
    Ok(conn)
}

/// Checkpoints the WAL so a copied `.db` file is self-contained.
pub fn checkpoint_and_close(conn: &Connection) {
    let _: Result<String, _> =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger::Logger;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rc-rust-{tag}-{nanos}"))
    }

    #[test]
    fn creates_the_full_schema_and_stamps_the_version() {
        let dir = temp_dir("schema");
        let logger = Logger::new("error");
        let conn = open(&dir, &logger, None).expect("open");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        let tables: Vec<String> = {
            let mut statement = conn
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        for expected in [
            "meta",
            "session",
            "settings",
            "stats",
            "targets",
            "orders",
            "report_products",
            "market_snapshot",
            "price_points",
            "event_points",
            "item_event_points",
            "item_catalog",
        ] {
            assert!(
                tables.iter().any(|name| name == expected),
                "missing {expected}"
            );
        }

        let indexes: Vec<String> = {
            let mut statement = conn
                .prepare(
                    "SELECT name FROM sqlite_master
                     WHERE type='index' AND name NOT LIKE 'sqlite_autoindex%'
                     ORDER BY name",
                )
                .unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        for expected in [
            "orders_at_id",
            "price_points_bucket_at",
            "item_event_points_bucket_at",
        ] {
            assert!(
                indexes.iter().any(|name| name == expected),
                "missing index {expected}"
            );
        }

        // The whole point of the two indexes: the loot page must not need a
        // sort, and the hourly prune must not scan the whole price table.
        let order_plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT id, order_id, item_id, label, qty, requested_qty, stock_at_trigger, unit_price, total_price,
                        target_price, profit_rlt, status, at, latency_ms, note, message
                 FROM orders ORDER BY at DESC, id DESC LIMIT 50",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            !order_plan.to_ascii_uppercase().contains("TEMP B-TREE"),
            "loot page should not sort, got {order_plan}"
        );
        let prune_plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN DELETE FROM price_points WHERE bucket_at < 1",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            prune_plan.contains("price_points_bucket_at"),
            "prune should use the time index, got {prune_plan}"
        );

        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal.to_ascii_lowercase(), "wal");

        // `db::open` only builds the schema; the settings row is seeded later by
        // `Store::ensure_rows`. Assert the v8 column exists here instead.
        assert!(
            column_exists(&conn, "settings", "mode").unwrap(),
            "schema v8 must add settings.mode"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_newer_schema_version() {
        let dir = temp_dir("newer");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(DB_FILENAME);
        {
            let conn = Connection::open(&file).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let logger = Logger::new("error");
        let error = open(&dir, &logger, None).unwrap_err();
        assert!(error.contains("newer than this build"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A v7 database only has the two booleans. The upgrade has to pick the one
    /// mode they described and then rewrite the mirrors so they agree.
    #[test]
    fn migrates_a_v7_database_and_backfills_mode() {
        for (dry_run, paused, expected) in [
            (0, 0, MODE_LIVE),
            (1, 0, MODE_SHADOW),
            (0, 1, MODE_OFF),
            (1, 1, MODE_OFF), // the dead cell collapses to off
        ] {
            let dir = temp_dir(&format!("v7-{dry_run}-{paused}"));
            fs::create_dir_all(&dir).unwrap();
            let file = dir.join(DB_FILENAME);
            {
                let conn = Connection::open(&file).unwrap();
                conn.execute_batch(
                    "CREATE TABLE settings (
                         id INTEGER PRIMARY KEY CHECK (id = 1),
                         delay_per_order INTEGER NOT NULL,
                         max_orders_per_minute INTEGER NOT NULL,
                         dry_run INTEGER NOT NULL DEFAULT 0,
                         auto_refresh INTEGER NOT NULL DEFAULT 1,
                         paused INTEGER NOT NULL DEFAULT 0
                     );",
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO settings
                     (id, delay_per_order, max_orders_per_minute, dry_run, auto_refresh, paused)
                     VALUES (1, 200, 60, ?1, 1, ?2)",
                    [dry_run, paused],
                )
                .unwrap();
                conn.pragma_update(None, "user_version", 7).unwrap();
            }

            let logger = Logger::new("error");
            let conn = open(&dir, &logger, None).expect("open");
            let (mode, dry, paused_after): (String, i64, i64) = conn
                .query_row(
                    "SELECT mode, dry_run, paused FROM settings WHERE id = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(mode, expected, "dry_run={dry_run} paused={paused}");
            assert_eq!(dry, i64::from(expected != MODE_LIVE));
            assert_eq!(paused_after, i64::from(expected == MODE_OFF));
            let markup: f64 = conn
                .query_row(
                    "SELECT total_price_markup_pct FROM settings WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                markup, 0.0,
                "an upgraded database must not start with price headroom"
            );
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION);
            drop(conn);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
