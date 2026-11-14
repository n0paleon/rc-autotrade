use std::sync::Arc;

use rusqlite::params;

use crate::constants::{EVENT_BUCKET_MS, PRICE_RETENTION_DAYS};
use crate::logger::Logger;
use crate::model::{MarketSnapshot, PriceBand, PriceSeriesPoint};
use crate::store::state::MARKET_EVENT_SCOPE_KEY;
use crate::store::{Store, event_bucket_at, now_ms};

use super::{store, target, temp_dir};

#[test]
fn stores_market_snapshots_and_price_points() {
    let mut store = store("market");
    let item_id = "6319f7baa8ce530569ed16b9";
    store.upsert_config(target(item_id), None);

    store
        .save_market_snapshot(&MarketSnapshot {
            item_id: item_id.into(),
            last_price: Some(0.09),
            last_stock: Some(12),
            allowed_price_min: Some(0.05),
            allowed_price_max: Some(0.9),
            allowed_at: Some(now_ms()),
            item_type: Some("miner".into()),
            seen_at: now_ms(),
        })
        .unwrap();
    assert_eq!(store.load_market_snapshots().len(), 1);

    store
        .record_price_point(
            item_id,
            1_700_000_000_000,
            0.09,
            PriceBand {
                low: 0.085,
                high: 0.095,
                raw_low: 0.07,
                raw_high: 0.11,
            },
            Some((0.0905, 10.0, false)),
        )
        .unwrap();
    store
        .record_price_point(
            item_id,
            1_700_000_060_000,
            0.095,
            PriceBand {
                low: 0.093,
                high: 0.097,
                raw_low: 0.092,
                raw_high: 0.098,
            },
            None,
        )
        .unwrap();
    let points = store.load_price_history(item_id, 10, 0);
    assert_eq!(points.len(), 2);
    assert_eq!(points[0].bucket_at, 1_700_000_000_000);
    assert_eq!(points[0].low_price, Some(0.085));
    assert_eq!(points[0].high_price, Some(0.095));
    assert_eq!(points[0].raw_low_price, Some(0.07));
    assert_eq!(points[0].raw_high_price, Some(0.11));
    assert_eq!(points[0].fill_price, Some(0.0905));
    assert_eq!(points[1].fill_price, None);

    // The long-range series skips the live window cap and reads the whole
    // retained range for the chart's 3d/7d periods.
    assert_eq!(
        store.price_series(item_id, 1_700_000_000_000),
        vec![
            PriceSeriesPoint {
                bucket_at: 1_700_000_000_000,
                price: 0.09,
                low: Some(0.085),
                high: Some(0.095),
                raw_low: Some(0.07),
                raw_high: Some(0.11),
            },
            PriceSeriesPoint {
                bucket_at: 1_700_000_060_000,
                price: 0.095,
                low: Some(0.093),
                high: Some(0.097),
                raw_low: Some(0.092),
                raw_high: Some(0.098),
            },
        ]
    );
    assert_eq!(
        store.price_series(item_id, 1_700_000_030_000),
        vec![PriceSeriesPoint {
            bucket_at: 1_700_000_060_000,
            price: 0.095,
            low: Some(0.093),
            high: Some(0.097),
            raw_low: Some(0.092),
            raw_high: Some(0.098),
        }]
    );
    assert!(store.price_series("other-item", 0).is_empty());
}

#[test]
fn prunes_price_points_outside_the_retention_window() {
    let store = store("prune-price");
    let item_id = "6319f7baa8ce530569ed16b9";
    let old = now_ms() - (PRICE_RETENTION_DAYS + 1) * 86_400_000;
    let recent = now_ms() - 86_400_000;
    let flat = |price: f64| PriceBand {
        low: price,
        high: price,
        raw_low: price,
        raw_high: price,
    };
    store
        .record_price_point(item_id, old, 0.1, flat(0.1), None)
        .unwrap();
    store
        .record_price_point(item_id, recent, 0.2, flat(0.2), None)
        .unwrap();

    assert_eq!(store.prune_price_history().unwrap(), 1);
    let points = store.load_price_history(item_id, 10, 0);
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].bucket_at, recent);
    // A passive checkpoint is always safe to run on a live database.
    store.checkpoint_wal().unwrap();
}

#[test]
fn reads_a_legacy_price_row_as_a_zero_width_band() {
    let store = store("legacy-band");
    let item_id = "6319f7baa8ce530569ed16b9";
    // Rows written before the band columns existed have NULL edges.
    store
        .db
        .execute(
            "INSERT INTO price_points (item_id, bucket_at, price) VALUES (?1, ?2, ?3)",
            params![item_id, 1_700_000_000_000_i64, 0.25],
        )
        .unwrap();

    assert_eq!(
        store.price_series(item_id, 0),
        vec![PriceSeriesPoint {
            bucket_at: 1_700_000_000_000,
            price: 0.25,
            low: Some(0.25),
            high: Some(0.25),
            raw_low: Some(0.25),
            raw_high: Some(0.25),
        }],
        "an old close must read as low = high = price, never a hole"
    );
}

#[test]
fn counts_market_events_into_five_minute_buckets_across_a_reopen() {
    let dir = temp_dir("events");
    let logger = Arc::new(Logger::new("error"));
    let now = now_ms();
    let current = event_bucket_at(now);
    let previous = current - EVENT_BUCKET_MS;

    {
        let mut store = Store::open(dir.clone(), logger.clone()).expect("store");
        store.note_market_event(previous + 1_000);
        store.note_market_event(previous + 4 * 60_000);
        store.note_market_event(current + 10);
        // Older than the retention window: the flush must drop it instead
        // of letting the chart query a stale row.
        store.note_market_event(now - 9 * 86_400_000);

        assert_eq!(
            store.event_series(previous),
            vec![(previous, 2), (current, 1)]
        );
        assert_eq!(store.flush_event_points().unwrap(), 3);
        assert_eq!(
            store.event_series(previous),
            vec![(previous, 2), (current, 1)]
        );
        let stored: i64 = store
            .db
            .query_row("SELECT COUNT(*) FROM event_points", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, 2);
    }

    // The flusher's rows come back on boot, so the chart survives restarts.
    let store = Store::open(dir, logger).expect("reopen");
    assert_eq!(
        store.event_series(previous),
        vec![(previous, 2), (current, 1)]
    );
}

#[test]
fn marketplace_event_scope_is_stamped_once_and_survives_a_reopen() {
    let dir = temp_dir("event-scope");
    let logger = Arc::new(Logger::new("error"));
    let first = {
        let store = Store::open(dir.clone(), logger.clone()).expect("store");
        let scope = store.event_scope_at();
        assert_eq!(
            scope,
            event_bucket_at(scope),
            "scope starts on a bucket edge"
        );
        let stored: String = store
            .db
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![MARKET_EVENT_SCOPE_KEY],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, scope.to_string());
        scope
    };

    std::thread::sleep(std::time::Duration::from_millis(2));
    let store = Store::open(dir, logger).expect("reopen");
    assert_eq!(
        store.event_scope_at(),
        first,
        "a restart must not move the scope and hide already-correct buckets"
    );
}

#[test]
fn event_buckets_align_to_the_five_minute_grid() {
    let at = 1_700_000_007_999;
    let bucket = event_bucket_at(at);
    assert_eq!(bucket % EVENT_BUCKET_MS, 0);
    assert!(bucket <= at && at < bucket + EVENT_BUCKET_MS);
    assert_eq!(event_bucket_at(bucket), bucket);
    assert_eq!(
        event_bucket_at(bucket + EVENT_BUCKET_MS),
        bucket + EVENT_BUCKET_MS
    );
}
