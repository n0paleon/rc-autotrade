use super::names::RESOLVE_BATCH;
use super::*;
use std::collections::HashMap;
use std::sync::Arc;

use crate::constants::{ITEM_EVENT_BUCKET_MS, ITEM_EVENT_HISTORY_DAYS};
use crate::logger::Logger;

#[test]
fn the_resolution_batch_rotates_instead_of_retrying_the_same_rows() {
    // A cold catalogue with more unnamed rows than one batch can hold. The
    // bug this guards: retrying the same first eight forever while the rest
    // never get a turn, which is what a batch of transport failures caused.
    let dir = std::env::temp_dir().join(format!(
        "rc-rust-item-activity-batch-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let logger = Arc::new(Logger::new("error"));
    let mut store = Store::open(dir.clone(), logger).expect("store");
    for index in 0..40 {
        store.note_item_event(&format!("item-{index:02}"), Some("miner"), now_ms());
    }
    let store_query = ItemActivityQuery {
        // The rotation works on whatever rows the page carries, so ask for
        // all 40 in one page instead of the default page size.
        limit: crate::constants::ITEM_ACTIVITY_LIMIT,
        ..Default::default()
    };
    let rows = store.item_activity(&store_query, now_ms()).rows;
    assert_eq!(rows.len(), 40);

    let first: Vec<String> = pending_names(&store, &rows)
        .into_iter()
        .map(|entry| entry.item_id)
        .collect();
    assert_eq!(first.len(), RESOLVE_BATCH);
    assert_eq!(first[0], "item-00");
    assert_eq!(first[RESOLVE_BATCH - 1], "item-07");

    // Record a transport failure for the first batch, then ask again: the
    // next batch must name different rows.
    for item_id in &first {
        store.note_catalog_attempts(item_id, now_ms());
    }
    let second: Vec<String> = pending_names(&store, &rows)
        .into_iter()
        .map(|entry| entry.item_id)
        .collect();
    assert_eq!(second.len(), RESOLVE_BATCH);
    assert!(
        second.iter().all(|item_id| !first.contains(item_id)),
        "the second batch must move on, got {second:?}"
    );
    assert_eq!(second[0], "item-08");

    // A definitive miss is final: the row is never retried even though it
    // has no name to show.
    store.mark_catalog_resolved("item-08", now_ms());
    let third: Vec<String> = pending_names(&store, &rows)
        .into_iter()
        .map(|entry| entry.item_id)
        .collect();
    assert!(
        !third.iter().any(|item_id| item_id == "item-08"),
        "a cached miss must not be retried, got {third:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

const NOW: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

#[test]
fn defaults_to_the_last_week_and_one_page() {
    let filters = ActivityFilters::parse(&HashMap::new(), NOW);
    assert_eq!(filters.until, NOW);
    assert_eq!(filters.since, NOW - 7 * DAY);
    assert_eq!(filters.item_type, "");
    assert_eq!(filters.q, "");
    assert_eq!(filters.limit, crate::constants::ITEM_ACTIVITY_PAGE_SIZE);
    assert_eq!(filters.offset, 0);
}

#[test]
fn days_controls_the_window_and_is_clamped_to_retention() {
    let filters = ActivityFilters::parse(&params(&[("days", "2")]), NOW);
    assert_eq!(filters.since, NOW - 2 * DAY);

    let too_wide = ActivityFilters::parse(&params(&[("days", "999")]), NOW);
    assert_eq!(too_wide.since, NOW - ITEM_EVENT_HISTORY_DAYS * DAY);

    let zero = ActivityFilters::parse(&params(&[("days", "0")]), NOW);
    assert_eq!(zero.since, NOW - DAY);
}

#[test]
fn an_explicit_range_wins_and_is_clamped_to_retention() {
    let filters = ActivityFilters::parse(&params(&[("since", "1000"), ("until", "5000")]), NOW);
    // Both ends predate retention, so the stored window cannot answer the
    // request; the fallback is the `days` window, not an empty table.
    assert_eq!(filters.since, NOW - 7 * DAY);
    assert_eq!(filters.until, NOW);

    // A range that only *starts* too early keeps the recent end it asked
    // for and is clamped forward to the oldest stored hour.
    let recent = ActivityFilters::parse(
        &params(&[("since", "1000"), ("until", &(NOW - 3 * HOUR).to_string())]),
        NOW,
    );
    assert_eq!(recent.since, NOW - ITEM_EVENT_HISTORY_DAYS * DAY);
    assert_eq!(recent.until, NOW - 3 * HOUR);

    let today = ActivityFilters::parse(
        &params(&[
            ("since", &(NOW - 3 * HOUR).to_string()),
            ("until", &NOW.to_string()),
        ]),
        NOW,
    );
    assert_eq!(today.since, NOW - 3 * HOUR);
    assert_eq!(today.until, NOW);
}

#[test]
fn an_inverted_or_future_range_falls_back_instead_of_returning_nothing() {
    let inverted = ActivityFilters::parse(
        &params(&[
            ("since", &NOW.to_string()),
            ("until", &(NOW - DAY).to_string()),
        ]),
        NOW,
    );
    assert_eq!(inverted.since, NOW - 7 * DAY);
    assert_eq!(inverted.until, NOW);

    let future = ActivityFilters::parse(&params(&[("until", &(NOW + 5 * DAY).to_string())]), NOW);
    assert_eq!(future.until, NOW, "the future is never served");

    // A 10 ms range is widened to at least one stored bucket, so the table
    // can never be empty merely because the clock was tight.
    let tiny = ActivityFilters::parse(
        &params(&[
            ("since", &(NOW - HOUR).to_string()),
            ("until", &(NOW - HOUR + 10).to_string()),
        ]),
        NOW,
    );
    assert!(tiny.until > tiny.since);
    assert!(tiny.until - tiny.since >= ITEM_EVENT_BUCKET_MS);
}

#[test]
fn filters_are_trimmed_and_bounded() {
    let filters = ActivityFilters::parse(
        &params(&[
            ("itemType", " Miner "),
            ("q", "  hash  "),
            ("limit", "9999"),
        ]),
        NOW,
    );
    assert_eq!(filters.item_type, "miner");
    assert_eq!(filters.q, "hash");
    assert_eq!(filters.limit, crate::constants::ITEM_ACTIVITY_LIMIT);

    let junk = ActivityFilters::parse(
        &params(&[
            ("itemType", &"x".repeat(41)),
            ("q", &"y".repeat(65)),
            ("limit", "0"),
        ]),
        NOW,
    );
    assert_eq!(junk.item_type, "");
    assert_eq!(junk.q, "");
    // An explicit zero is not a page size; fall back to the default.
    assert_eq!(junk.limit, crate::constants::ITEM_ACTIVITY_PAGE_SIZE);
}

#[test]
fn pagination_params_are_parsed_and_bounded() {
    let paged = ActivityFilters::parse(&params(&[("limit", "10"), ("offset", "20")]), NOW);
    assert_eq!(paged.limit, 10);
    assert_eq!(paged.offset, 20);

    // A negative offset is not a number `usize` can hold, so it is ignored
    // rather than wrapping around to a huge skip.
    let negative = ActivityFilters::parse(&params(&[("offset", "-5")]), NOW);
    assert_eq!(negative.offset, 0);

    let garbage = ActivityFilters::parse(&params(&[("offset", "abc")]), NOW);
    assert_eq!(garbage.offset, 0);
}

#[test]
fn leading_zeros_in_numeric_filters_are_rejected_as_text() {
    let filters = ActivityFilters::parse(&params(&[("limit", "abc"), ("days", "-4")]), NOW);
    assert_eq!(filters.limit, crate::constants::ITEM_ACTIVITY_PAGE_SIZE);
    assert_eq!(filters.since, NOW - DAY);
}

#[test]
fn json_bodies_reject_nested_values() {
    use serde_json::json;
    let parsed = parse_json_query(&json!({ "days": 3, "q": null })).expect("object");
    assert_eq!(parsed.get("days").map(String::as_str), Some("3"));
    assert!(parse_json_query(&json!({ "days": [1] })).is_none());
    assert!(parse_json_query(&json!([1, 2])).is_none());
}
