use std::collections::{BTreeMap, HashMap};

use rusqlite::params;

use crate::clock::now_ms;
use crate::constants::{
    ITEM_ACTIVITY_LIMIT, ITEM_ACTIVITY_PAGE_SIZE, ITEM_EVENT_BUCKET_MS, ITEM_EVENT_HISTORY_DAYS,
    ITEM_EVENT_RETENTION_DAYS,
};
use crate::model::{ItemActivityReport, ItemActivityRow, ItemTypeTotal};

use super::Store;
use super::state::{ItemActivityQuery, ItemEventAccum, ItemEventRow, item_event_bucket_at};

impl Store {
    // ── per-item market activity ────────────────────────────────────────────

    /// Counts one market frame for `item_id` into its hourly bucket and stamps
    /// the catalogue entry, so an id that was never added to the fleet still
    /// appears (and gets a name) on the item monitor.
    ///
    /// Everything stays in memory on the frame path; the flusher below writes
    /// the batches in one transaction.
    pub fn note_item_event(&mut self, item_id: &str, item_type: Option<&str>, at: i64) {
        let key = item_id.to_ascii_lowercase();
        if key.is_empty() {
            return;
        }
        let bucket = item_event_bucket_at(at);
        {
            let accum = self.item_events.entry((bucket, key.clone())).or_default();
            accum.events += 1;
            accum.last_seen_at = Some(at);
            if let Some(item_type) = item_type.filter(|value| !value.is_empty()) {
                accum.item_type = Some(item_type.to_ascii_lowercase());
            }
        }
        self.item_event_dirty.insert((bucket, key.clone()));

        let entry = self.catalog.entry(key).or_default();
        if entry.first_seen_at.is_none() {
            entry.first_seen_at = Some(at);
            entry.item_id = item_id.to_string();
        }
        entry.last_seen_at = Some(at);
        if let Some(item_type) = item_type.filter(|value| !value.is_empty()) {
            let item_type = item_type.to_ascii_lowercase();
            if entry.item_type.is_empty() {
                entry.item_type = item_type;
            }
        }
    }

    /// The ranked item monitor for one range.
    ///
    /// Reads the in-memory accumulator, which holds the same retention window
    /// the flusher keeps in SQLite, so a row cannot be counted twice and a fresh
    /// frame shows up without waiting for a flush. Ids are matched
    /// case-insensitively; `events_24h` is computed against `now`, not against
    /// the requested range, so the heat column keeps meaning "busy right now".
    ///
    /// Pagination happens **after** ranking and after the per-type totals are
    /// summed, so page 2 is the next slice of the same ordering and the type
    /// summary always describes the whole range rather than the current page.
    /// The per-type totals are also summed before the category and search
    /// filters are applied: the deck keeps one badge per known type on screen,
    /// and those counts must not vanish while the table is narrowed.
    pub fn item_activity(&self, query: &ItemActivityQuery, now: i64) -> ItemActivityReport {
        let since = if query.since > 0 {
            query.since
        } else {
            now - ITEM_EVENT_HISTORY_DAYS * 86_400_000
        };
        let until = if query.until > 0 { query.until } else { now };
        let day_ago = now - 86_400_000;
        let wanted_type = query.item_type.trim().to_ascii_lowercase();
        let term = query.q.trim().to_ascii_lowercase();
        let limit = match query.limit {
            // 0 means "caller did not ask", which is the default page size; a
            // literal clamp would turn it into 1 and silently serve one row.
            0 => ITEM_ACTIVITY_PAGE_SIZE,
            requested => requested.min(ITEM_ACTIVITY_LIMIT),
        };
        let offset = query.offset;

        struct Aggregate {
            events: i64,
            events_24h: i64,
            first_at: i64,
            last_at: i64,
            item_type: String,
        }

        let mut aggregates: HashMap<&str, Aggregate> = HashMap::new();
        for ((bucket, item_id), accum) in &self.item_events {
            if *bucket > until {
                continue;
            }
            // A bucket is a full slice that may start before `since`; the row
            // still belongs to the range when part of it does.
            let bucket_end = bucket + ITEM_EVENT_BUCKET_MS - 1;
            let in_range = bucket_end >= since;
            let in_day = *bucket <= now && bucket_end >= day_ago;
            if !in_range && !in_day {
                continue;
            }
            let entry = aggregates
                .entry(item_id.as_str())
                .or_insert_with(|| Aggregate {
                    events: 0,
                    events_24h: 0,
                    first_at: i64::MAX,
                    last_at: i64::MIN,
                    item_type: String::new(),
                });
            if in_range {
                entry.events += accum.events;
                entry.first_at = entry.first_at.min((*bucket).max(since));
                let last_frame = accum.last_seen_at.unwrap_or(bucket_end);
                entry.last_at = entry.last_at.max(last_frame.clamp(since, until));
            }
            if in_day {
                entry.events_24h += accum.events;
            }
            if entry.item_type.is_empty()
                && let Some(item_type) = accum.item_type.as_deref()
            {
                entry.item_type = item_type.to_string();
            }
        }

        // Build every row first: the per-type totals are a summary of the whole
        // range, not of the current category or search filter. They have to be
        // summed before the filters narrow the table so the header badges and
        // the category menu keep the same shape (and the same counts) while the
        // reader is looking at one category.
        let mut rows: Vec<ItemActivityRow> = aggregates
            .into_iter()
            .filter(|(_, aggregate)| aggregate.events > 0)
            .map(|(item_id, aggregate)| {
                let catalog = self.catalog.get(item_id);
                // The type a frame was counted under wins over the catalogue.
                // Frames are grouped by the type the feed reported, so using
                // anything else here would let the per-type totals disagree
                // with the very counts they summarise — and a later `item-info`
                // lookup would silently move rows between categories.
                let item_type = if aggregate.item_type.is_empty() {
                    catalog
                        .map(|item| item.item_type.clone())
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| "mutation_component".to_string())
                } else {
                    aggregate.item_type.clone()
                };
                let label = catalog.map(|item| item.label.clone()).unwrap_or_default();
                let tracked = self.find_config(item_id).is_some();
                let first_at = (aggregate.first_at != i64::MAX).then_some(aggregate.first_at);
                let last_at = (aggregate.last_at != i64::MIN).then_some(aggregate.last_at);
                ItemActivityRow {
                    item_id: item_id.to_string(),
                    item_type,
                    label,
                    rarity: catalog.and_then(|item| item.rarity.clone()),
                    rarity_color: catalog.and_then(|item| item.rarity_color.clone()),
                    events: aggregate.events,
                    events_24h: aggregate.events_24h,
                    first_at,
                    last_at,
                    tracked,
                }
            })
            .collect();

        // Totals describe every row in the range, before the filters narrow the
        // table and before paging. The deck renders one badge per known type,
        // so these counts must not disappear when one category is selected.
        let mut type_totals: BTreeMap<String, ItemTypeTotal> = BTreeMap::new();
        for row in &rows {
            let entry = type_totals
                .entry(row.item_type.clone())
                .or_insert_with(|| ItemTypeTotal {
                    item_type: row.item_type.clone(),
                    events: 0,
                    items: 0,
                });
            entry.events += row.events;
            entry.items += 1;
        }

        rows.retain(|row| {
            if !wanted_type.is_empty() && row.item_type != wanted_type {
                return false;
            }
            if term.is_empty() {
                return true;
            }
            row.label.to_ascii_lowercase().contains(&term) || row.item_id.contains(&term)
        });

        rows.sort_by(|a, b| {
            b.events
                .cmp(&a.events)
                .then(b.events_24h.cmp(&a.events_24h))
                // A stable tiebreak keeps the table from reshuffling between
                // polls when two ids moved exactly as often.
                .then_with(|| a.item_id.cmp(&b.item_id))
        });

        // These totals describe every row that matched the filters, not just
        // the page being served.
        let total_events: i64 = rows.iter().map(|row| row.events).sum();
        let total_rows = rows.len();
        let page: Vec<ItemActivityRow> = rows.into_iter().skip(offset).take(limit).collect();

        ItemActivityReport {
            rows: page,
            total_rows,
            total_events,
            type_totals: type_totals.into_values().collect(),
        }
    }

    /// Total market frames the monitor knows about in a range, for the summary
    /// line. Counts every id, not just the rows the pagination cap returns.
    pub fn item_activity_total(&self, since: i64, until: i64) -> i64 {
        self.item_events
            .iter()
            .filter(|((bucket, _), _)| *bucket <= until && bucket + ITEM_EVENT_BUCKET_MS > since)
            .map(|(_, accum)| accum.events)
            .sum()
    }

    /// Writes every dirty item bucket in one transaction, then drops expiry.
    pub fn flush_item_events(&mut self) -> Result<u64, String> {
        let pending: Vec<((i64, String), ItemEventAccum)> = self
            .item_event_dirty
            .iter()
            .filter_map(|key| {
                self.item_events
                    .get(key)
                    .map(|accum| (key.clone(), accum.clone()))
            })
            .collect();

        if !pending.is_empty() {
            let tx = self
                .db
                .unchecked_transaction()
                .map_err(|e| format!("could not start item event flush: {e}"))?;
            {
                let mut statement = tx
                    .prepare_cached(
                        "INSERT INTO item_event_points
                             (item_id, bucket_at, item_type, events, last_seen_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(item_id, bucket_at) DO UPDATE SET
                           events = excluded.events,
                           item_type = COALESCE(excluded.item_type, item_type),
                           last_seen_at = excluded.last_seen_at",
                    )
                    .map_err(|e| format!("could not cache item event statement: {e}"))?;
                for ((bucket, item_id), accum) in &pending {
                    statement
                        .execute(params![
                            item_id,
                            bucket,
                            accum.item_type,
                            accum.events,
                            accum.last_seen_at
                        ])
                        .map_err(|e| format!("could not record item event point: {e}"))?;
                }
            }
            tx.commit()
                .map_err(|e| format!("could not commit item event points: {e}"))?;
            self.item_event_dirty.clear();
        }

        self.prune_item_events()?;
        Ok(pending.len() as u64)
    }

    pub(super) fn hydrate_item_events(&mut self) {
        let since = now_ms() - ITEM_EVENT_RETENTION_DAYS * 86_400_000;
        let result = (|| -> Result<Vec<ItemEventRow>, String> {
            let mut statement = self
                .db
                .prepare(
                    "SELECT item_id, bucket_at, item_type, events, last_seen_at
                     FROM item_event_points WHERE bucket_at >= ?1",
                )
                .map_err(|e| format!("could not prepare item event read: {e}"))?;
            let rows = statement
                .query_map(params![since], |row| {
                    let item_id: String = row.get(0)?;
                    let bucket: i64 = row.get(1)?;
                    Ok(ItemEventRow {
                        key: (bucket, item_id),
                        accum: ItemEventAccum {
                            events: row.get(3)?,
                            last_seen_at: row.get(4)?,
                            item_type: row.get(2)?,
                        },
                    })
                })
                .map_err(|e| format!("could not read item events: {e}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("could not decode item events: {e}"))
        })();
        match result {
            Ok(rows) => {
                for row in rows {
                    self.item_events.insert(row.key, row.accum);
                }
            }
            Err(error) => self.logger.warn("store", error),
        }
    }

    fn prune_item_events(&mut self) -> Result<(), String> {
        let now = now_ms();
        let cutoff = now - ITEM_EVENT_RETENTION_DAYS * 86_400_000;
        let expired = self
            .item_events
            .keys()
            .next()
            .map(|(bucket, _)| *bucket < cutoff)
            .unwrap_or(false);
        // A month of hourly rows for a busy feed is a few thousand entries; an
        // hourly sweep is plenty. An expiry between sweeps (clock jump, restored
        // data) still forces the sweep immediately.
        if !expired && now - self.item_event_pruned_at < 3_600_000 {
            return Ok(());
        }
        self.item_event_pruned_at = now;
        self.item_events.retain(|(bucket, _), _| *bucket >= cutoff);
        self.item_event_dirty
            .retain(|(bucket, _)| *bucket >= cutoff);
        self.db
            .execute(
                "DELETE FROM item_event_points WHERE bucket_at < ?1",
                params![cutoff],
            )
            .map(|_| ())
            .map_err(|e| format!("could not prune item activity: {e}"))
    }
}
