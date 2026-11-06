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

pub const SCHEMA_VERSION: i64 = 7;
pub const DB_FILENAME: &str = "rollercoin.db";

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
  paused                INTEGER NOT NULL DEFAULT 0
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
  fill_price REAL,
  fill_qty   REAL,
  fill_approx INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (item_id, bucket_at)
) WITHOUT ROWID;
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
) -> Result<(), rusqlite::Error> {
    if !column_exists(conn, table, column)? {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
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
    add_column_if_missing(conn, "orders", "timings", "TEXT")?;
    add_column_if_missing(conn, "orders", "raw_events", "TEXT")?;

    // The auto-target feature is gone; drop its columns if a legacy DB has them.
    for column in ["auto_target", "auto_target_margin_pct"] {
        if column_exists(conn, "targets", column)? {
            conn.execute_batch(&format!("ALTER TABLE targets DROP COLUMN {column}"))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn secure(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn secure(_path: &Path) {}

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
        ] {
            assert!(
                tables.iter().any(|name| name == expected),
                "missing {expected}"
            );
        }

        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal.to_ascii_lowercase(), "wal");

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
}
