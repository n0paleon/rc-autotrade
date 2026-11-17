use super::schema::column_exists;
use super::{DB_FILENAME, MODE_LIVE, MODE_OFF, MODE_SHADOW, SCHEMA_VERSION, open};

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::logger::Logger;

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
    assert!(
        column_exists(&conn, "report_products", "failed_qty").unwrap(),
        "the profit report must track missed units, not just missed orders"
    );
    for column in ["min_price", "max_price"] {
        assert!(
            column_exists(&conn, "item_event_points", column).unwrap(),
            "the item monitor must store per-bucket price extremes ({column})"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A database from before `failed_qty` kept no missed-unit total, but the loot
/// log still holds every failed attempt. The upgrade must rebuild the column
/// from `requested_qty` instead of starting every product at 0. The same goes
/// for `wins`, which only counts non-negative-profit fills.
#[test]
fn backfills_missed_units_from_the_loot_log() {
    let dir = temp_dir("failed-qty");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(DB_FILENAME);
    {
        let conn = Connection::open(&file).unwrap();
        conn.execute_batch(
            "CREATE TABLE orders (
                 id TEXT PRIMARY KEY,
                 order_id TEXT,
                 item_id TEXT NOT NULL DEFAULT '',
                 label TEXT NOT NULL DEFAULT '',
                 qty INTEGER NOT NULL DEFAULT 0,
                 unit_price REAL NOT NULL DEFAULT 0,
                 total_price REAL NOT NULL DEFAULT 0,
                 target_price REAL NOT NULL DEFAULT 0,
                 profit_rlt REAL NOT NULL DEFAULT 0,
                 status TEXT NOT NULL,
                 at INTEGER NOT NULL,
                 latency_ms INTEGER,
                 note TEXT,
                 message TEXT
             );
             CREATE TABLE report_products (
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
             -- Legacy failed rows stored the attempted count in qty.
             INSERT INTO orders (id, item_id, label, qty, status, at, profit_rlt)
             VALUES ('o1', 'item-1', 'Hashboard', 4, 'failed', 1, 0),
                    ('o2', 'item-1', 'Hashboard', 6, 'failed', 2, 0),
                    ('o3', 'item-1', 'Hashboard', 2, 'filled', 3, 0.5),
                    ('o4', 'item-1', 'Hashboard', 2, 'filled', 4, -0.5);
             INSERT INTO report_products (product_key, item_id, label, fills, failed, qty, first_at, last_at)
             VALUES ('item-1', 'item-1', 'Hashboard', 2, 2, 4, 1, 4);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 7).unwrap();
    }

    let logger = Logger::new("error");
    let conn = open(&dir, &logger, None).expect("open");
    let failed_qty: i64 = conn
        .query_row(
            "SELECT failed_qty FROM report_products WHERE product_key = 'item-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(failed_qty, 10, "4 + 6 attempted units were never obtained");
    // The legacy `qty` column still holds the fulfilled total.
    let qty: i64 = conn
        .query_row(
            "SELECT qty FROM report_products WHERE product_key = 'item-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(qty, 4);
    // Only the profitable fill is a win; the loss is not.
    let wins: i64 = conn
        .query_row(
            "SELECT wins FROM report_products WHERE product_key = 'item-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(wins, 1);
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn backfills_session_wins_from_the_loot_log() {
    let dir = temp_dir("stats-wins");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(DB_FILENAME);
    {
        let conn = Connection::open(&file).unwrap();
        conn.execute_batch(
            "CREATE TABLE orders (
                 id TEXT PRIMARY KEY,
                 item_id TEXT NOT NULL DEFAULT '',
                 label TEXT NOT NULL DEFAULT '',
                 qty INTEGER NOT NULL DEFAULT 0,
                 unit_price REAL NOT NULL DEFAULT 0,
                 total_price REAL NOT NULL DEFAULT 0,
                 target_price REAL NOT NULL DEFAULT 0,
                 profit_rlt REAL NOT NULL DEFAULT 0,
                 status TEXT NOT NULL,
                 at INTEGER NOT NULL,
                 latency_ms INTEGER,
                 note TEXT,
                 message TEXT
             );
             INSERT INTO orders (id, item_id, label, qty, profit_rlt, status, at)
             VALUES ('o1', 'item-1', 'Hashboard', 1, 0.4, 'filled', 1),
                    ('o2', 'item-1', 'Hashboard', 1, 0, 'filled', 2),
                    ('o3', 'item-1', 'Hashboard', 1, -0.3, 'filled', 3),
                    ('o4', 'item-1', 'Hashboard', 0, 0, 'failed', 4);
             CREATE TABLE stats (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 since INTEGER NOT NULL,
                 profit_rlt REAL NOT NULL DEFAULT 0,
                 spent_rlt REAL NOT NULL DEFAULT 0,
                 bought INTEGER NOT NULL DEFAULT 0,
                 filled INTEGER NOT NULL DEFAULT 0,
                 failed INTEGER NOT NULL DEFAULT 0,
                 orders_sent INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO stats (id, since, filled, failed, orders_sent)
             VALUES (1, 1, 3, 1, 4);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 7).unwrap();
    }

    let logger = Logger::new("error");
    let conn = open(&dir, &logger, None).unwrap();
    let wins: i64 = conn
        .query_row("SELECT wins FROM stats WHERE id = 1", [], |row| row.get(0))
        .unwrap();
    // A profitable fill and a break-even fill count; the loss does not.
    assert_eq!(wins, 2);
    drop(conn);
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
