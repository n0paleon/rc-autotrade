use std::sync::Arc;

use crate::constants::{ITEM_EVENT_BUCKET_MS, ITEM_EVENT_RETENTION_DAYS};
use crate::logger::Logger;
use crate::store::{ItemActivityQuery, Store, item_event_bucket_at, now_ms};

use super::{store, target, temp_dir};

#[test]
fn item_activity_ranks_every_id_by_frame_count() {
    let mut store = store("item-activity");
    let now = now_ms();
    // Two busy items and one quiet one, all outside the target fleet.
    for _ in 0..5 {
        store.note_item_event("busy-a", Some("miner"), now - 1_000);
    }
    for _ in 0..3 {
        store.note_item_event("busy-b", Some("rack"), now - 2_000);
    }
    store.note_item_event("quiet", Some("battery"), now - 3_000);

    let report = store.item_activity(&ItemActivityQuery::default(), now);
    let rows = &report.rows;
    assert_eq!(rows.len(), 3);
    assert_eq!(report.total_rows, 3);
    assert_eq!(rows[0].item_id, "busy-a");
    assert_eq!(rows[0].events, 5);
    assert_eq!(rows[0].item_type, "miner");
    assert!(!rows[0].tracked);
    assert_eq!(rows[1].item_id, "busy-b");
    assert_eq!(rows[2].item_id, "quiet");

    // The per-id counters sum to the same number the global chart counts.
    assert_eq!(report.total_events, 9);
    assert_eq!(store.item_activity_total(now - 86_400_000, now), 9);

    // Per-type totals describe the whole range, not the page. The map is
    // ordered by type key, which keeps the header summary stable between
    // polls instead of reshuffling as rows re-rank.
    let types: Vec<(&str, i64)> = report
        .type_totals
        .iter()
        .map(|total| (total.item_type.as_str(), total.events))
        .collect();
    assert_eq!(types, vec![("battery", 1), ("miner", 5), ("rack", 3)]);
}

#[test]
fn item_activity_filters_by_type_search_and_range() {
    let mut store = store("item-activity-filters");
    let now = now_ms();
    let target = target("miner-tracked");
    store.upsert_config(target, None);
    store.apply_catalog_item(
        "miner-tracked",
        "Epic Hashboard",
        "miner",
        Some("Epic"),
        Some("a52bff"),
        now,
    );

    for _ in 0..4 {
        store.note_item_event("miner-tracked", Some("miner"), now - 1_000);
    }
    for _ in 0..2 {
        store.note_item_event("rack-1", Some("rack"), now - 1_000);
    }
    // Old enough to fall outside a one-day window but inside retention.
    for _ in 0..7 {
        store.note_item_event("old-part", Some("mutation_component"), now - 4 * 86_400_000);
    }

    let all = store.item_activity(&ItemActivityQuery::default(), now);
    let all_rows = &all.rows;
    assert_eq!(all_rows.len(), 3);
    assert_eq!(
        all_rows[0].item_id, "old-part",
        "the busiest id wins the default range"
    );
    assert!(
        all_rows
            .iter()
            .any(|row| row.item_id == "miner-tracked" && row.tracked),
        "a fleet id must be flagged as tracked"
    );

    let by_type = store.item_activity(
        &ItemActivityQuery {
            item_type: "miner".into(),
            ..Default::default()
        },
        now,
    );
    assert_eq!(by_type.rows.len(), 1);
    assert_eq!(by_type.total_events, 4, "the type filter scopes the totals");
    // The header badges keep the whole range: narrowing the table to miners
    // must not make the rack and parts counts disappear.
    let types: Vec<(&str, i64)> = by_type
        .type_totals
        .iter()
        .map(|total| (total.item_type.as_str(), total.events))
        .collect();
    assert_eq!(
        types,
        vec![("miner", 4), ("mutation_component", 7), ("rack", 2)]
    );
    assert_eq!(by_type.rows[0].label, "Epic Hashboard");
    assert_eq!(by_type.rows[0].rarity.as_deref(), Some("Epic"));
    assert_eq!(by_type.rows[0].rarity_color.as_deref(), Some("a52bff"));
    assert_eq!(by_type.rows[0].events_24h, 4);

    let by_name = store.item_activity(
        &ItemActivityQuery {
            q: "HASH".into(),
            ..Default::default()
        },
        now,
    );
    assert_eq!(by_name.rows.len(), 1, "the resolved name is searchable");
    let by_id = store.item_activity(
        &ItemActivityQuery {
            q: "RACK-1".into(),
            ..Default::default()
        },
        now,
    );
    assert_eq!(by_id.rows.len(), 1, "the raw id stays searchable");

    let recent = store.item_activity(
        &ItemActivityQuery {
            since: now - 86_400_000,
            until: now,
            ..Default::default()
        },
        now,
    );
    assert_eq!(
        recent.rows.len(),
        2,
        "the four-day-old id leaves a one-day range"
    );
    assert!(recent.rows.iter().all(|row| row.item_id != "old-part"));
    // `events24h` stays anchored to "now", not to the selected window.
    assert_eq!(recent.rows[0].events_24h, recent.rows[0].events);
}

/// The table is paged, so a page must be a slice of one stable ranking and
/// the totals must keep describing the whole range, not the visible rows.
#[test]
fn item_activity_pages_a_stable_ranking_with_range_wide_totals() {
    let mut store = store("item-activity-paging");
    let now = now_ms();
    // Ten ids with strictly decreasing volume, so the expected order is
    // unambiguous: item-9 (10 frames) down to item-0 (1 frame).
    for index in 0..10 {
        let item_type = if index % 2 == 0 { "miner" } else { "rack" };
        for _ in 0..=index {
            store.note_item_event(&format!("item-{index}"), Some(item_type), now - 1_000);
        }
    }

    let first = store.item_activity(
        &ItemActivityQuery {
            limit: 4,
            offset: 0,
            ..Default::default()
        },
        now,
    );
    assert_eq!(first.rows.len(), 4);
    assert_eq!(first.total_rows, 10);
    assert_eq!(
        first
            .rows
            .iter()
            .map(|row| row.item_id.as_str())
            .collect::<Vec<_>>(),
        vec!["item-9", "item-8", "item-7", "item-6"]
    );
    // 10+9+…+1
    assert_eq!(first.total_events, 55);
    // The share column divides by this, so it has to describe the range and
    // not the page: 180 of 2 176 must read 8% on every page.
    assert_eq!(first.total_events, 55);
    let second = store.item_activity(
        &ItemActivityQuery {
            limit: 4,
            offset: 4,
            ..Default::default()
        },
        now,
    );
    assert_eq!(
        second
            .rows
            .iter()
            .map(|row| row.item_id.as_str())
            .collect::<Vec<_>>(),
        vec!["item-5", "item-4", "item-3", "item-2"]
    );
    // Totals are unchanged by paging: they describe the range.
    assert_eq!(second.total_events, first.total_events);
    assert_eq!(second.total_rows, first.total_rows);
    assert_eq!(second.type_totals, first.type_totals);

    // The last page is short and still reports the real total.
    let last = store.item_activity(
        &ItemActivityQuery {
            limit: 4,
            offset: 8,
            ..Default::default()
        },
        now,
    );
    assert_eq!(last.rows.len(), 2);
    assert_eq!(last.total_rows, 10);

    // An offset past the end is an empty page, not a panic or a wrap.
    let past = store.item_activity(
        &ItemActivityQuery {
            limit: 4,
            offset: 999,
            ..Default::default()
        },
        now,
    );
    assert!(past.rows.is_empty());
    assert_eq!(past.total_rows, 10);
}

#[test]
fn item_activity_pages_within_a_type_filter() {
    let mut store = store("item-activity-paging-type");
    let now = now_ms();
    for index in 0..6 {
        let item_type = if index < 4 { "miner" } else { "rack" };
        for _ in 0..(index + 1) {
            store.note_item_event(&format!("t-{index}"), Some(item_type), now - 1_000);
        }
    }

    let report = store.item_activity(
        &ItemActivityQuery {
            item_type: "miner".into(),
            limit: 2,
            offset: 0,
            ..Default::default()
        },
        now,
    );
    // Only the four miners count toward the ranking and the filtered
    // totals. The type summary still describes the whole range, so the
    // badges do not shrink while one category is selected.
    assert_eq!(report.total_rows, 4);
    assert_eq!(report.rows.len(), 2);
    assert_eq!(report.rows[0].item_id, "t-3");
    let types: Vec<(&str, i64, i64)> = report
        .type_totals
        .iter()
        .map(|total| (total.item_type.as_str(), total.events, total.items))
        .collect();
    assert_eq!(types, vec![("miner", 10, 4), ("rack", 11, 2)]);
    assert_eq!(report.total_events, 1 + 2 + 3 + 4);
}

#[test]
fn item_activity_summarizes_frames_per_type() {
    let mut store = store("item-activity-type-totals");
    let now = now_ms();
    for _ in 0..6 {
        store.note_item_event("miner-a", Some("miner"), now - 1_000);
        store.note_item_event("miner-b", Some("miner"), now - 1_000);
    }
    for _ in 0..3 {
        store.note_item_event("rack-a", Some("rack"), now - 1_000);
    }

    let report = store.item_activity(&ItemActivityQuery::default(), now);
    let miners = report
        .type_totals
        .iter()
        .find(|total| total.item_type == "miner")
        .expect("miner total");
    assert_eq!(miners.events, 12);
    assert_eq!(miners.items, 2);
    let racks = report
        .type_totals
        .iter()
        .find(|total| total.item_type == "rack")
        .expect("rack total");
    assert_eq!(racks.events, 3);
    assert_eq!(racks.items, 1);
    // The per-type sums equal the range total, so the header can show both.
    let summed: i64 = report.type_totals.iter().map(|total| total.events).sum();
    assert_eq!(summed, report.total_events);
}

#[test]
fn item_activity_survives_a_reopen_and_updates_in_place() {
    let dir = temp_dir("item-activity-reopen");
    let logger = Arc::new(Logger::new("error"));
    let now = now_ms();
    {
        let mut store = Store::open(dir.clone(), logger.clone()).expect("store");
        for _ in 0..6 {
            store.note_item_event("persisted", Some("miner"), now - 5_000);
        }
        store.flush_item_events().expect("flush");
        let stored: i64 = store
            .db
            .query_row("SELECT SUM(events) FROM item_event_points", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored, 6);
    }

    // Counting continues after boot; the second flush must replace the row
    // instead of inserting a duplicate for the same slice.
    let mut store = Store::open(dir, logger).expect("reopen");
    let rows = store.item_activity(&ItemActivityQuery::default(), now).rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].events, 6);
    for _ in 0..2 {
        store.note_item_event("persisted", Some("miner"), now - 4_000);
    }
    store.flush_item_events().expect("second flush");
    let rows = store.item_activity(&ItemActivityQuery::default(), now).rows;
    assert_eq!(rows[0].events, 8);
    let rows_in_table: i64 = store
        .db
        .query_row("SELECT COUNT(*) FROM item_event_points", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows_in_table, 1);
}

#[test]
fn catalog_records_names_misses_and_boot_state() {
    let dir = temp_dir("catalog");
    let logger = Arc::new(Logger::new("error"));
    let now = now_ms();
    {
        let mut store = Store::open(dir.clone(), logger.clone()).expect("store");
        store.note_item_event("named", Some("miner"), now);
        store.note_item_event("missing", Some("rack"), now);
        assert_eq!(store.catalog_unresolved(10).len(), 2);

        assert!(store.apply_catalog_item(
            "named",
            "Epic Hashboard",
            "miner",
            Some("Epic"),
            Some("a52bff"),
            now,
        ));
        store.mark_catalog_resolved("missing", now);
        assert!(store.catalog_unresolved(10).is_empty());

        let entry = store
            .catalog_entry("NAMED")
            .expect("case-insensitive lookup");
        assert_eq!(entry.label, "Epic Hashboard");
        assert_eq!(entry.rarity.as_deref(), Some("Epic"));
        assert!(!entry.needs_resolution());
    }

    // A lookup miss must stay a miss after a restart, so a delisted item is
    // not retried on every monitor refresh.
    let store = Store::open(dir, logger).expect("reopen");
    let missing = store.catalog_entry("missing").expect("catalogued");
    assert!(!missing.needs_resolution());
    assert_eq!(missing.label, "");
    assert!(store.catalog_unresolved(10).is_empty());
}

#[test]
fn item_event_buckets_align_to_the_five_minute_grid() {
    let at = 1_700_000_007_999;
    let bucket = item_event_bucket_at(at);
    assert_eq!(bucket % ITEM_EVENT_BUCKET_MS, 0);
    assert!(bucket <= at && at < bucket + ITEM_EVENT_BUCKET_MS);
    assert_eq!(item_event_bucket_at(bucket), bucket);
    // The monitor's finest range is an hour, which must divide evenly.
    assert_eq!(3_600_000 % ITEM_EVENT_BUCKET_MS, 0);
}

#[test]
fn item_events_are_pruned_outside_retention() {
    let mut store = store("item-activity-prune");
    let now = now_ms();
    store.note_item_event("fresh", Some("miner"), now - 1_000);
    store.note_item_event(
        "stale",
        Some("miner"),
        now - (ITEM_EVENT_RETENTION_DAYS + 1) * 86_400_000,
    );
    store.flush_item_events().expect("flush");

    let report = store.item_activity(
        &ItemActivityQuery {
            since: now - (ITEM_EVENT_RETENTION_DAYS + 5) * 86_400_000,
            until: now,
            ..Default::default()
        },
        now,
    );
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].item_id, "fresh");
    let stale_rows: i64 = store
        .db
        .query_row(
            "SELECT COUNT(*) FROM item_event_points WHERE item_id = 'stale'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stale_rows, 0);
}
