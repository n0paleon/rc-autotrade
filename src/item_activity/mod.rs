//! Item activity monitor: which marketplace item ids moved the most.
//!
//! Unlike the global activity chart (`event_points`), this data is keyed by item
//! id, so it answers "what was busy" for an arbitrary date range and for ids
//! that were never added to the target fleet. Counting stays in memory in the
//! engine frame path ([`Store::note_item_event`]); this module owns the two
//! pieces that are not tied to SQLite:
//!
//! * [`ActivityFilters`] parsing/validation for `GET /api/item-activity`, so a
//!   malformed date range is rejected instead of silently returning everything;
//! * [`resolve_pending`], the slow half: it asks `item-info` for the products
//!   the catalogue has not named yet, with a strict per-response budget so a
//!   cold catalogue cannot stall the deck.
//!
//! The ranking itself lives in [`Store::item_activity`] because it reads the
//! in-memory accumulator that only the store owns.

use serde_json::Value;

use crate::constants::{ITEM_EVENT_BUCKET_MS, ITEM_EVENT_HISTORY_DAYS};
use crate::store::{ItemActivityQuery, Store, now_ms};

mod names;

#[cfg(test)]
mod tests;

pub use names::{
    PendingName, Resolved, ResolvedName, apply_resolved, pending_names, resolve_pending,
};

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
    /// Frames and ids per item type, summed over the whole range before the
    /// category/search filters. The header summary uses this, so it does not
    /// shrink when the reader pages forward or selects a category.
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
