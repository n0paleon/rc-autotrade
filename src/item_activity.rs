//! Item activity monitor: which marketplace item ids moved the most.
//!
//! Unlike the global activity chart (`event_points`), this data is keyed by item
//! id, so it answers "what was busy" for an arbitrary date range and for ids
//! that were never added to the target fleet. Counting stays in memory in the
//! engine frame path ([`Store::note_item_event`]); this module owns the two
//! pieces that are not tied to SQLite:
//!
//! * [`ActivityQuery`] parsing/validation for `GET /api/item-activity`, so a
//!   malformed date range is rejected instead of silently returning everything;
//! * [`resolve_names`], the slow half: it asks `item-info` for the products the
//!   catalogue has not named yet, with a strict per-response budget so a cold
//!   catalogue cannot stall the deck.
//!
//! The ranking itself lives in [`Store::item_activity`] because it reads the
//! in-memory accumulator that only the store owns.

use std::sync::Arc;

use serde_json::Value;

use crate::api::Credentials;
use crate::constants::{ITEM_EVENT_BUCKET_MS, ITEM_EVENT_HISTORY_DAYS};
use crate::item_info::ItemInfoService;
use crate::logger::Logger;
use crate::store::{ItemActivityQuery, Store, now_ms};

/// A page of the monitor plus the window and totals it was computed for.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityView {
    pub since: i64,
    pub until: i64,
    pub bucket_ms: i64,
    pub retention_days: i64,
    pub history_days: i64,
    /// Frames counted across every id in the range, before pagination. The deck
    /// divides each row by this to draw both the share number and its bar.
    pub total_events: i64,
    /// Ids that matched the filters, before the page was cut.
    pub total_rows: usize,
    pub limit: usize,
    pub offset: usize,
    pub page: usize,
    pub pages: usize,
    pub has_more: bool,
    /// Frames and ids per item type, summed over the whole range. The header
    /// summary uses this, so it does not shrink when the reader pages forward.
    pub type_totals: Vec<crate::model::ItemTypeTotal>,
    pub rows: Vec<crate::model::ItemActivityRow>,
    /// Ids in the response whose name still has to be looked up.
    pub unresolved: usize,
}

/// Validated filters for one monitor request.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityFilters {
    pub since: i64,
    pub until: i64,
    pub item_type: String,
    pub q: String,
    pub limit: usize,
    pub offset: usize,
}

impl ActivityFilters {
    /// Turns the raw query string into a safe window.
    ///
    /// Rules, in order:
    /// * `days` (1..=`ITEM_EVENT_HISTORY_DAYS`) is the primary control; an
    ///   explicit `since`/`until` wins over it when both are present.
    /// * `since` older than retention is clamped to the retention start, so the
    ///   table never claims a range the store already pruned.
    /// * `until` defaults to now and is clamped to `[since + 1 bucket, now]`.
    /// * a non-positive or inverted direct range falls back to `days`, keeping
    ///   a bad timestamp from returning an empty table.
    pub fn parse(params: &std::collections::HashMap<String, String>, now: i64) -> Self {
        let retention_start = now - ITEM_EVENT_HISTORY_DAYS * 86_400_000;
        let days = params
            .get("days")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(7)
            .clamp(1, ITEM_EVENT_HISTORY_DAYS);

        let direct_since = params
            .get("since")
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|value| *value > 0);
        let direct_until = params
            .get("until")
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|value| *value > 0);

        let (mut since, mut until) = match (direct_since, direct_until) {
            (Some(since), Some(until)) if until > since => (since, until),
            (Some(since), None) => (since, now),
            (None, Some(until)) => (until - days * 86_400_000, until),
            _ => (now - days * 86_400_000, now),
        };

        since = since.max(retention_start);
        // Clamping `since` forward can invert an old explicit range (both ends
        // older than retention). Falling back to the `days` window keeps the
        // deck showing real data instead of an empty table for a range that was
        // pruned long ago.
        if until <= since {
            since = now - days * 86_400_000;
            until = now;
        }
        // A range narrower than one stored bucket still gets a full bucket, so
        // the table is not empty merely because the clock was tight.
        if until - since < ITEM_EVENT_BUCKET_MS {
            since = (until - ITEM_EVENT_BUCKET_MS).max(retention_start);
        }
        until = until.clamp(since + 1, now);

        let item_type = params
            .get("itemType")
            .or_else(|| params.get("item_type"))
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty() && value.len() <= 40)
            .unwrap_or_default();
        let q = params
            .get("q")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty() && value.chars().count() <= 64)
            .unwrap_or_default();
        let limit = params
            .get("limit")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .map(|value| value.min(crate::constants::ITEM_ACTIVITY_LIMIT))
            .unwrap_or(crate::constants::ITEM_ACTIVITY_PAGE_SIZE);
        // An offset past the end simply yields an empty page; the response
        // reports `totalRows` so the deck can correct the page number.
        let offset = params
            .get("offset")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);

        Self {
            since,
            until,
            item_type,
            q,
            limit,
            offset,
        }
    }

    /// The store-facing query for these filters.
    pub fn query(&self) -> ItemActivityQuery {
        ItemActivityQuery {
            since: self.since,
            until: self.until,
            item_type: self.item_type.clone(),
            q: self.q.clone(),
            limit: self.limit,
            offset: self.offset,
        }
    }
}

/// Builds the monitor payload for one request.
pub fn activity_view(store: &Store, filters: &ActivityFilters) -> ActivityView {
    let report = store.item_activity(&filters.query(), filters.until.max(now_ms()));
    let limit = filters.limit.max(1);
    let pages = report.total_rows.div_ceil(limit).max(1);
    let page = (filters.offset / limit) + 1;
    let unresolved = report
        .rows
        .iter()
        .filter(|row| row.label.is_empty())
        .count();
    ActivityView {
        since: filters.since,
        until: filters.until,
        bucket_ms: ITEM_EVENT_BUCKET_MS,
        retention_days: crate::constants::ITEM_EVENT_RETENTION_DAYS,
        history_days: ITEM_EVENT_HISTORY_DAYS,
        total_events: report.total_events,
        total_rows: report.total_rows,
        limit,
        offset: filters.offset,
        page,
        pages,
        has_more: filters.offset + report.rows.len() < report.total_rows,
        type_totals: report.type_totals,
        rows: report.rows,
        unresolved,
    }
}

/// How long one monitor response may spend resolving names upstream.
const RESOLVE_BUDGET_MS: u64 = 2_500;
/// Names resolved per response. A cold catalogue fills over a few refreshes
/// instead of turning the first render into one request per row.
const RESOLVE_BATCH: usize = 8;

/// One id the response still has to name.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingName {
    pub item_id: String,
    pub item_type: String,
}

/// Which rows on this page still need an `item-info` lookup.
///
/// Pure and synchronous so it runs under the store lock: the caller then does
/// the slow part ([`resolve_pending`]) with no lock held and applies the results
/// in one more short critical section.
///
/// Rows are ordered by fewest attempts first. Without that ordering a cold
/// catalogue with more unnamed rows than [`RESOLVE_BATCH`] would retry the same
/// first eight on every refresh forever — exactly what a batch of unnameable ids
/// (a delisted product, or the simulator's random ids) would cause while they
/// kept a transport error from being cached.
pub fn pending_names(store: &Store, rows: &[crate::model::ItemActivityRow]) -> Vec<PendingName> {
    let mut pending: Vec<(i64, i64, PendingName)> = Vec::new();
    for row in rows {
        if !row.label.is_empty() {
            continue;
        }
        let key = row.item_id.to_ascii_lowercase();
        let entry = store.catalog_entry(&key);
        if let Some(entry) = entry
            && !entry.needs_resolution()
        {
            // Already asked for, and upstream did not know it (or it has no
            // name). Keep the fallback label instead of retrying forever.
            continue;
        }
        let item_type = if row.item_type.is_empty() {
            "mutation_component".to_string()
        } else {
            row.item_type.clone()
        };
        // Fewest attempts first, then the busiest row; the tiebreak keeps the
        // order stable between refreshes.
        let attempts = entry.map(|entry| entry.attempts).unwrap_or(0);
        let events = row.events;
        pending.push((
            attempts,
            -events,
            PendingName {
                item_id: key,
                item_type,
            },
        ));
    }
    pending.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    pending
        .into_iter()
        .take(RESOLVE_BATCH)
        .map(|(_, _, entry)| entry)
        .collect()
}

/// One lookup outcome, ready to be applied to the store.
#[derive(Debug, Clone)]
pub struct ResolvedName {
    pub item_id: String,
    pub resolved_at: i64,
    /// `Ok` carries the resolved name/rarity; `Err` carries the upstream
    /// message and whether it was a definitive miss (delisted / unknown type)
    /// rather than a transport failure.
    pub outcome: Result<Resolved, String>,
    pub definitive_miss: bool,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub label: String,
    pub item_type: String,
    pub rarity: Option<String>,
    pub rarity_color: Option<String>,
}

/// Names the pending ids, best-effort and with a strict budget.
///
/// Never holds a lock: the caller passes the ids in and applies the returned
/// outcomes afterwards. A timeout, a rejected session or an item the marketplace
/// no longer lists is reported, not raised — the monitor keeps rendering the id
/// and the next refresh retries a transport failure.
pub async fn resolve_pending(
    item_info: &Arc<ItemInfoService>,
    credentials: &Credentials,
    logger: &Arc<Logger>,
    pending: Vec<PendingName>,
) -> Vec<ResolvedName> {
    let mut resolved = Vec::with_capacity(pending.len());
    let started = std::time::Instant::now();
    for (index, entry) in pending.into_iter().enumerate() {
        if started.elapsed().as_millis() as u64 > RESOLVE_BUDGET_MS {
            logger.debug(
                "item-activity",
                format!(
                    "name resolution stopped after {index} item(s); the rest waits for the next refresh"
                ),
            );
            break;
        }
        let outcome = item_info
            .preview(credentials, &entry.item_id, &entry.item_type)
            .await;
        let resolved_at = now_ms();
        match outcome {
            Ok(item) => resolved.push(ResolvedName {
                item_id: entry.item_id,
                resolved_at,
                outcome: Ok(Resolved {
                    label: item.label,
                    item_type: item.item_type,
                    rarity: item.rarity,
                    rarity_color: item.rarity_color,
                }),
                definitive_miss: false,
            }),
            Err(error) => {
                let message = error.to_ascii_lowercase();
                let definitive_miss = message.contains("not found") || message.contains("invalid");
                if definitive_miss {
                    logger.debug(
                        "item-activity",
                        format!("item-info has no name for {}: {error}", entry.item_id),
                    );
                } else {
                    logger.warn(
                        "item-activity",
                        format!("could not name {}: {error}", entry.item_id),
                    );
                }
                resolved.push(ResolvedName {
                    item_id: entry.item_id,
                    resolved_at,
                    outcome: Err(error),
                    definitive_miss,
                });
            }
        }
    }
    resolved
}

/// Writes resolved names to the catalogue and back onto the page.
///
/// Returns how many rows are still nameless, so the response can say so without
/// a second scan of the catalogue.
pub fn apply_resolved(
    store: &mut Store,
    rows: &mut [crate::model::ItemActivityRow],
    resolved: Vec<ResolvedName>,
) -> usize {
    for entry in resolved {
        match entry.outcome {
            Ok(item) => {
                store.apply_catalog_item(
                    &entry.item_id,
                    &item.label,
                    &item.item_type,
                    item.rarity.as_deref(),
                    item.rarity_color.as_deref(),
                    entry.resolved_at,
                );
                for row in rows.iter_mut() {
                    if row.item_id.eq_ignore_ascii_case(&entry.item_id) {
                        row.label = item.label.clone();
                        row.item_type = item.item_type.clone();
                        row.rarity = item.rarity.clone();
                        row.rarity_color = item.rarity_color.clone();
                    }
                }
            }
            Err(_) if entry.definitive_miss => {
                // A miss is remembered so a delisted id is not retried on every
                // refresh; a transport failure is not, so a flaky network
                // recovers on its own.
                store.mark_catalog_resolved(&entry.item_id, entry.resolved_at);
            }
            Err(_) => {
                // Not final, but it still gets an attempt so the next batch
                // rotates to ids that have not been tried yet.
                store.note_catalog_attempts(&entry.item_id, entry.resolved_at);
            }
        }
    }
    rows.iter().filter(|row| row.label.is_empty()).count()
}

/// Strictest shape the deck may send for a single frame: used by tests.
pub fn parse_json_query(body: &Value) -> Option<std::collections::HashMap<String, String>> {
    let object = body.as_object()?;
    let mut params = std::collections::HashMap::new();
    for (key, value) in object {
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Null => continue,
            _ => return None,
        };
        params.insert(key.clone(), text);
    }
    Some(params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

        let future =
            ActivityFilters::parse(&params(&[("until", &(NOW + 5 * DAY).to_string())]), NOW);
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
}
