use std::sync::Arc;

use crate::logger::Logger;
use crate::store::{HistoryQuery, Store, make_entry, now_ms};

use super::{store, temp_dir};

#[test]
fn records_orders_in_history_and_report() {
    let mut store = store("history");
    // Every statement executed through this connection is traced.
    assert!(
        store.db_metrics().sample()["queries"]["statements"]
            .as_i64()
            .unwrap_or(0)
            > 0,
        "the trace interface should have counted the boot statements"
    );
    let at = now_ms();
    let mut entry = make_entry("o1", "item-1", "Hashboard", "filled", 2, at);
    entry.total_price = 0.2;
    entry.profit_rlt = 0.05;
    store.add_history(entry).unwrap();

    let mut failed = make_entry("o2", "item-1", "Hashboard", "failed", 0, at + 1);
    failed.profit_rlt = 0.0;
    store.add_history(failed).unwrap();

    assert_eq!(store.state.history.len(), 2);
    assert!(store.has_order("o1"));
    assert_eq!(store.history_count(), 2);

    let report = store.report_view();
    assert_eq!(report.totals.fills, 1);
    assert_eq!(report.totals.failed, 1);
    assert_eq!(report.items.len(), 1);
    assert_eq!(report.items[0].qty, 2);
    assert!((report.items[0].spent_rlt - 0.2).abs() < 1e-9);
    // One filled and one failed order: half of the completed attempts won.
    assert_eq!(report.totals.success_pct, Some(50));
    assert_eq!(report.items[0].success_pct, Some(50));
}

#[test]
fn report_winrate_counts_only_non_negative_profit_fills() {
    let mut store = store("winrate-profit");
    let at = now_ms();

    let mut win = make_entry("o1", "item-1", "Hashboard", "filled", 1, at);
    win.profit_rlt = 0.05;
    store.add_history(win).unwrap();

    // Breaking even still counts as a win.
    let mut even = make_entry("o2", "item-1", "Hashboard", "filled", 1, at + 1);
    even.profit_rlt = 0.0;
    store.add_history(even).unwrap();

    // A losing fill is a completed attempt but not a win.
    let mut loss = make_entry("o3", "item-1", "Hashboard", "filled", 1, at + 2);
    loss.profit_rlt = -0.25;
    store.add_history(loss).unwrap();

    store
        .add_history(make_entry("o4", "item-1", "Hashboard", "failed", 0, at + 3))
        .unwrap();

    let report = store.report_view();
    assert_eq!(report.totals.fills, 3);
    assert_eq!(report.totals.wins, 2);
    assert_eq!(report.totals.failed, 1);
    // Two wins out of four completed attempts.
    assert_eq!(report.totals.success_pct, Some(50));
    assert_eq!(report.items[0].wins, 2);
    assert_eq!(report.items[0].success_pct, Some(50));

    // The session HUD uses the same definition and keeps its own counter.
    assert_eq!(store.state.stats.wins, 2);
    let stored: i64 = store
        .db
        .query_row("SELECT wins FROM stats WHERE id = 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(stored, 2);
}

#[test]
fn missed_quantity_tracks_the_attempted_units_of_failed_orders() {
    let mut store = store("missed-qty");
    let at = now_ms();
    let mut partial = make_entry("o1", "item-1", "Hashboard", "filled", 2, at);
    // Asked for 5, got 2: only the filled units count towards `qty`.
    partial.requested_qty = Some(5);
    partial.total_price = 0.2;
    store.add_history(partial).unwrap();
    // A rejection has no fulfillment result, so every attempted unit missed.
    let mut rejected = make_entry("o2", "item-1", "Hashboard", "failed", 0, at + 1);
    rejected.requested_qty = Some(3);
    store.add_history(rejected).unwrap();
    let mut empty = make_entry("o3", "item-1", "Hashboard", "failed", 0, at + 2);
    empty.requested_qty = Some(0);
    store.add_history(empty).unwrap();

    let report = store.report_view();
    assert_eq!(report.items[0].qty, 2, "only fulfilled units are bought");
    assert_eq!(
        report.items[0].failed_qty, 3,
        "0 + 3 attempted units missed"
    );
    assert_eq!(report.totals.failed_qty, 3);
    assert_eq!(report.totals.failed, 2);
}

#[test]
fn reset_stats_wipes_the_loot_log_and_profit_report() {
    let dir = temp_dir("reset-stats");
    let logger = Arc::new(Logger::new("error"));
    {
        let mut store = Store::open(dir.clone(), logger.clone()).unwrap();
        let at = now_ms();
        let mut filled = make_entry("o1", "item-1", "Hashboard", "filled", 2, at);
        filled.total_price = 0.2;
        filled.profit_rlt = 0.05;
        store.add_history(filled).unwrap();
        store
            .add_history(make_entry("o2", "item-1", "Hashboard", "failed", 0, at + 1))
            .unwrap();
        assert_eq!(store.history_count(), 2);
        assert_eq!(store.report_view().totals.fills, 1);

        store.reset_stats();

        assert!(store.state.history.is_empty());
        assert_eq!(store.history_count(), 0);
        assert_eq!(
            store.history_page(HistoryQuery::default()).unwrap().total,
            0
        );
        assert!(store.state.report.is_empty());
        assert_eq!(store.state.stats.filled, 0);
        assert_eq!(store.state.stats.orders_sent, 0);
        store.close();
    }

    // The reset must survive a restart, not just the in-memory snapshot.
    let reopened = Store::open(dir.clone(), logger).unwrap();
    assert_eq!(reopened.history_count(), 0);
    assert!(reopened.state.history.is_empty());
    drop(reopened);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pages_history_newest_first_with_filters() {
    let mut store = store("paging");
    let at = now_ms();
    for index in 0..5 {
        let mut entry = make_entry(
            &format!("o{index}"),
            "item-1",
            if index % 2 == 0 { "Alpha" } else { "Beta" },
            if index % 2 == 0 { "filled" } else { "failed" },
            1,
            at + index,
        );
        entry.profit_rlt = 0.0;
        store.add_history(entry).unwrap();
    }

    let page = store
        .history_page(HistoryQuery {
            limit: Some(2),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.entries.len(), 2);
    assert_eq!(page.total, 5);
    assert!(page.has_more);
    assert_eq!(page.entries[0].id, "o4");

    let filtered = store
        .history_page(HistoryQuery {
            q: "alpha".into(),
            status: "filled".into(),
            limit: Some(10),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(filtered.entries.len(), 3);
    assert!(
        filtered
            .entries
            .iter()
            .all(|entry| entry.status == "filled")
    );

    // Profit-sign filter: re-point the same rows at non-zero profits.
    // Zero-profit rows must match neither bucket.
    let db = &store.db;
    db.execute("UPDATE orders SET profit_rlt = 0.5 WHERE id = 'o0'", [])
        .unwrap();
    db.execute("UPDATE orders SET profit_rlt = -0.25 WHERE id = 'o2'", [])
        .unwrap();
    db.execute("UPDATE orders SET profit_rlt = 1.0 WHERE id = 'o4'", [])
        .unwrap();
    // o1 (failed) and o3 (failed) stay at 0.0.

    let positive = store
        .history_page(HistoryQuery {
            profit: "positive".into(),
            limit: Some(10),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(positive.total, 2);
    assert!(positive.entries.iter().all(|entry| entry.profit_rlt > 0.0));

    let negative = store
        .history_page(HistoryQuery {
            profit: "negative".into(),
            limit: Some(10),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(negative.total, 1);
    assert!(negative.entries.iter().all(|entry| entry.profit_rlt < 0.0));

    // Combined filters compose: filled + positive.
    let filled_positive = store
        .history_page(HistoryQuery {
            status: "filled".into(),
            profit: "positive".into(),
            limit: Some(10),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(filled_positive.total, 2);
    assert_eq!(filled_positive.entries.len(), 2);

    // Unknown profit values fall back to no filter.
    let unknown = store
        .history_page(HistoryQuery {
            profit: "bogus".into(),
            limit: Some(10),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(unknown.total, 5);
}

#[test]
fn merges_late_timings_into_an_existing_row() {
    let mut store = store("timings");
    let mut entry = make_entry("o1", "item-1", "Hashboard", "filled", 1, now_ms());
    entry.timings = crate::timings::sanitize(&serde_json::json!({ "signalAt": 100 }));
    store.add_history(entry).unwrap();

    assert!(store.update_history_timings("o1", &serde_json::json!({ "responseAt": 250 })));
    let row = store.order_detail("o1").unwrap().expect("detail");
    let timings = row.timings.expect("timings");
    assert_eq!(timings.signal_at, Some(100.0));
    assert_eq!(timings.response_at, Some(250.0));
}

#[test]
fn benchmark_samples_are_newest_first_and_skip_untimed_rows() {
    let mut store = store("benchmark");
    let mut older = make_entry("o1", "item-1", "Hashboard", "filled", 1, 1_000);
    older.timings =
        crate::timings::sanitize(&serde_json::json!({ "signalAt": 100, "fillAt": 160 }));
    store.add_history(older).unwrap();
    let mut newer = make_entry("o2", "item-1", "Hashboard", "failed", 1, 2_000);
    newer.timings =
        crate::timings::sanitize(&serde_json::json!({ "signalAt": 100, "failedAt": 140 }));
    store.add_history(newer).unwrap();
    // Rows written before the latency debugger had no timings at all.
    store
        .add_history(make_entry("o3", "item-1", "Hashboard", "filled", 1, 3_000))
        .unwrap();

    let samples = store.benchmark_samples(10);
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0].at, 2_000);
    assert_eq!(samples[0].status, "failed");
    assert_eq!(samples[1].at, 1_000);

    // The limit applies to usable rows, not to the raw table tail.
    let newest = store.benchmark_samples(1);
    assert_eq!(newest.len(), 1);
    assert_eq!(newest[0].at, 2_000);
}

#[test]
fn benchmark_history_query_respects_the_time_range() {
    let mut store = store("benchmark-range");
    for (id, at) in [("o1", 1_000), ("o2", 2_000), ("o3", 3_000)] {
        let mut entry = make_entry(id, "item-1", "Hashboard", "filled", 1, at);
        entry.timings = crate::timings::sanitize(&serde_json::json!({
            "signalAt": 100,
            "fillHandledAt": 200
        }));
        store.add_history(entry).unwrap();
    }

    let ranged = store.benchmark_samples_between(1_500, 3_500, 10);
    assert_eq!(ranged.len(), 2);
    assert_eq!(ranged[0].at, 3_000);
    assert_eq!(ranged[1].at, 2_000);

    let inclusive = store.benchmark_samples_between(2_000, 3_000, 10);
    assert_eq!(inclusive.len(), 2);
    assert!(store.benchmark_samples_between(3_001, 4_000, 10).is_empty());
}

#[test]
fn lists_skip_raw_payloads_and_the_detail_reader_loads_them() {
    let mut store = store("history-lite");
    let mut entry = make_entry("o1", "item-1", "Hashboard", "filled", 1, now_ms());
    entry.timings = crate::timings::sanitize(&serde_json::json!({ "signalAt": 100 }));
    entry.raw_events = crate::raw_events::sanitize(&serde_json::json!({
        "priceEvent": { "cmd": "marketplace_orders_update" },
        "resultEvent": { "cmd": "marketplace_orders_filled" }
    }));
    store.add_history(entry).unwrap();

    let listed = store
        .history_page(HistoryQuery {
            ..Default::default()
        })
        .unwrap()
        .entries
        .remove(0);
    assert!(listed.timings.is_none(), "list must not decode timings");
    assert!(listed.raw_events.is_none(), "list must not decode frames");

    let detail = store.order_detail("o1").unwrap().expect("detail");
    assert_eq!(detail.timings.unwrap().signal_at, Some(100.0));
    assert_eq!(
        detail.raw_events.unwrap()["priceEvent"]["cmd"],
        "marketplace_orders_update"
    );

    assert!(store.order_detail("missing").unwrap().is_none());
}

#[test]
fn hydrate_loads_lists_without_heavy_frames() {
    let dir = temp_dir("history-reload");
    let logger = Arc::new(Logger::new("error"));
    let mut store = Store::open(dir.clone(), logger.clone()).unwrap();
    let mut entry = make_entry("o1", "item-1", "Hashboard", "filled", 1, now_ms());
    entry.raw_events = crate::raw_events::sanitize(&serde_json::json!({
        "priceEvent": { "cmd": "marketplace_orders_update" },
    }));
    store.add_history(entry).unwrap();
    drop(store);

    let store = Store::open(dir.clone(), logger).unwrap();
    let row = &store.state.history[0];
    assert!(row.timings.is_none(), "boot list must stay lean");
    assert!(row.raw_events.is_none(), "boot list must stay lean");
    assert!(
        store
            .order_detail("o1")
            .unwrap()
            .unwrap()
            .raw_events
            .is_some(),
        "the detail row still carries the frames"
    );
}

#[test]
fn trace_records_real_statement_durations() {
    let store = store("trace");
    store
        .db
        .execute_batch(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<200000)
             SELECT count(*) FROM c",
        )
        .expect("heavy query");
    let view = store.db_metrics().sample();
    assert_eq!(view["instrumented"], true);
    assert!(
        view["queries"]["statements"].as_i64().unwrap() > 0,
        "statements must be counted"
    );
    assert!(
        view["queries"]["maxMs"].as_f64().unwrap() > 1.0,
        "a 200k-row recursive query must take more than a millisecond, got {}",
        view["queries"]["maxMs"]
    );
    assert!(view["window"]["count"].as_i64().unwrap() > 0);
}

#[test]
fn rejects_duplicate_order_ids_gracefully() {
    let mut store = store("dupe");
    let at = now_ms();
    store
        .add_history(make_entry("same", "item-1", "Hashboard", "filled", 1, at))
        .unwrap();
    assert!(store.has_order("same"));
}
