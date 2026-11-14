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
