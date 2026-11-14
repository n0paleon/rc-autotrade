//! SQLite-backed runtime state.
//!
//! The in-memory [`State`] is the read path for the HTTP/WS layer; SQLite is
//! the durable log.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use rusqlite::{Connection, params};

use crate::constants::PRICE_RETENTION_DAYS;
use crate::db;
use crate::logger::Logger;
use crate::model::CatalogItem;

pub use crate::clock::now_ms;

mod benchmark;
mod catalog;
mod events;
mod hydrate;
mod item_activity;
mod market;
mod orders;
mod rows;
mod session;
mod settings;
mod state;
mod stats;
mod targets;
mod tokens;

#[cfg(test)]
mod tests;

use state::ItemEventAccum;

pub use rows::make_entry;
pub use state::{
    HistoryPage, HistoryQuery, ItemActivityQuery, State, event_bucket_at, item_event_bucket_at,
};
pub use tokens::{create_worker_token, hash_token, preview_token};

pub struct Store {
    pub state: State,
    pub db: Connection,
    pub data_dir: PathBuf,
    /// Held for the lifetime of the process so a second worker cannot open the
    /// same data directory and duplicate orders.
    _data_lock: std::fs::File,
    logger: Arc<Logger>,
    metrics: Arc<crate::db_metrics::DbMetrics>,
    /// Marketplace commands counted per 5-minute bucket (listing/price
    /// updates and fulfillment results). The window covers
    /// [`EVENT_RETENTION_DAYS`], so the chart can serve a week without a query
    /// per render; SQLite only receives the batches the flusher writes.
    event_points: BTreeMap<i64, i64>,
    /// Buckets changed since the last flush (absolute counts, not deltas).
    event_dirty: BTreeSet<i64>,
    event_pruned_at: i64,
    /// First instant owned by the marketplace-only counter. Older
    /// `event_points` rows were written before the scope was narrowed, cannot
    /// be split by command, and are hidden from the chart.
    event_scope_at: i64,
    /// Per-item frame counts, keyed by `(hour bucket, lowercase item id)`.
    /// The market feed can show hundreds of distinct ids an hour, so the map is
    /// capped and only the newest buckets survive a prune.
    item_events: BTreeMap<(i64, String), ItemEventAccum>,
    /// Item ids changed since the last flush.
    item_event_dirty: BTreeSet<(i64, String)>,
    item_event_pruned_at: i64,
    /// Every id the feed has shown, with the name resolved from item-info.
    /// `None` for an id means "looked up, not found", so a dead id is not
    /// retried on every response.
    catalog: HashMap<String, CatalogItem>,
    /// Lowercase item id → position in `state.configs`. The engine resolves a
    /// target on every frame, and the scan used to lowercase (and allocate for)
    /// every configured item on every lookup.
    config_index: HashMap<String, usize>,
    /// Last time a swallowed write error was logged, so a failing disk cannot
    /// turn into a log flood on the frame path.
    last_write_error_at: AtomicI64,
}

impl Store {
    /// Opens the database and hydrates the in-memory state.
    pub fn open(data_dir: PathBuf, logger: Arc<Logger>) -> Result<Self, String> {
        let data_lock = db::lock_data_dir(&data_dir)?;
        let metrics = crate::db_metrics::DbMetrics::new(data_dir.clone());
        let conn = db::open(&data_dir, &logger, Some(metrics.clone()))?;
        let mut store = Self {
            state: State::fresh(),
            db: conn,
            data_dir,
            _data_lock: data_lock,
            logger: logger.clone(),
            metrics,
            event_points: BTreeMap::new(),
            event_dirty: BTreeSet::new(),
            event_pruned_at: now_ms(),
            event_scope_at: 0,
            item_events: BTreeMap::new(),
            item_event_dirty: BTreeSet::new(),
            item_event_pruned_at: now_ms(),
            catalog: HashMap::new(),
            config_index: HashMap::new(),
            last_write_error_at: AtomicI64::new(0),
        };
        store.ensure_rows()?;
        store.hydrate()?;
        store.hydrate_events();
        store.hydrate_item_events();
        store.hydrate_catalog();
        // Reclaim anything older than the documented retention window. The API
        // only ever serves the last `PRICE_HISTORY_DAYS`, so this is not visible
        // in any chart; it just stops the table from growing across restarts.
        match store.prune_price_history() {
            Ok(removed) if removed > 0 => logger.info(
                "store",
                format!("pruned {removed} price point(s) older than {PRICE_RETENTION_DAYS}d"),
            ),
            Ok(_) => {}
            Err(error) => logger.warn("store", error),
        }
        Ok(store)
    }

    /// Statement metrics collected through SQLite's trace interface.
    pub fn db_metrics(&self) -> Arc<crate::db_metrics::DbMetrics> {
        self.metrics.clone()
    }

    /// Logs a swallowed write error at most once a minute.
    ///
    /// The in-memory row stays authoritative, so a failed write is a durability
    /// problem rather than a reason to fail the caller — but it must never be
    /// silent. A full disk used to look exactly like a healthy worker.
    pub(super) fn report_write_error(&self, what: &str, error: &dyn std::fmt::Display) {
        let now = now_ms();
        let previous = self.last_write_error_at.load(Ordering::Relaxed);
        if now - previous < 60_000 {
            return;
        }
        if self
            .last_write_error_at
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.logger.warn("store", format!("{what}: {error}"));
    }

    /// Writes settings, stats and every target in one transaction.
    pub fn flush(&mut self) -> Result<(), String> {
        self.state.updated_at = now_ms();
        let settings = self.state.settings;
        let stats = self.state.stats;
        let configs = self.state.configs.clone();

        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|e| format!("could not start transaction: {e}"))?;
        let (dry_run, paused) = settings.legacy_flags();
        tx.execute(
            "UPDATE settings SET delay_per_order = ?1, max_orders_per_minute = ?2,
                    dry_run = ?3, auto_refresh = ?4, paused = ?5, mode = ?6,
                    total_price_markup_pct = ?7 WHERE id = 1",
            params![
                settings.delay_per_order,
                settings.max_orders_per_minute,
                dry_run as i64,
                settings.auto_refresh as i64,
                paused as i64,
                settings.mode.as_str(),
                settings.total_price_markup_pct
            ],
        )
        .map_err(|e| format!("could not write settings: {e}"))?;
        tx.execute(
            "UPDATE stats SET since = ?1, profit_rlt = ?2, spent_rlt = ?3, bought = ?4,
                    filled = ?5, failed = ?6, orders_sent = ?7 WHERE id = 1",
            params![
                stats.since,
                stats.profit_rlt,
                stats.spent_rlt,
                stats.bought,
                stats.filled,
                stats.failed,
                stats.orders_sent
            ],
        )
        .map_err(|e| format!("could not write stats: {e}"))?;
        for config in &configs {
            tx.execute(
                "INSERT INTO targets
                 (item_id, label, item_type, rarity_color, target_price, desired_qty,
                  high_stock_threshold, min_acceptable_qty, allow_partial_order, active,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(item_id) DO UPDATE SET
                   label = excluded.label,
                   item_type = excluded.item_type,
                   rarity_color = excluded.rarity_color,
                   target_price = excluded.target_price,
                   desired_qty = excluded.desired_qty,
                   high_stock_threshold = excluded.high_stock_threshold,
                   min_acceptable_qty = excluded.min_acceptable_qty,
                   allow_partial_order = excluded.allow_partial_order,
                   active = excluded.active,
                   updated_at = excluded.updated_at",
                params![
                    config.item_id,
                    config.label,
                    config.item_type,
                    config.rarity_color,
                    config.target_price,
                    config.desired_qty,
                    config.high_stock_threshold,
                    config.min_acceptable_qty,
                    config.allow_partial_order as i64,
                    config.active as i64,
                    config.created_at,
                    config.updated_at
                ],
            )
            .map_err(|e| format!("could not write target: {e}"))?;
        }
        tx.commit()
            .map_err(|e| format!("could not commit store: {e}"))?;
        self.set_meta("updated_at", &self.state.updated_at.to_string())?;
        Ok(())
    }

    pub fn close(&mut self) {
        let _ = self.flush();
        let _ = self.flush_event_points();
        let _ = self.flush_item_events();
        db::checkpoint_and_close(&self.db);
    }

    pub fn logger(&self) -> &Arc<Logger> {
        &self.logger
    }
}
