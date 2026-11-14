use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, params};

use crate::clock::now_ms;
use crate::constants::MAX_HISTORY;
use crate::model::{Mode, OrderEntry, Product, Session, Settings, Stats, Target};

use super::Store;
use super::rows::row_to_entry_lite;
use super::state::State;

impl Store {
    pub(super) fn ensure_rows(&self) -> Result<(), String> {
        let now = now_ms();
        let defaults = Settings::default();
        let (dry_run, paused) = defaults.legacy_flags();
        self.db
            .execute(
                "INSERT OR IGNORE INTO settings
                 (id, delay_per_order, max_orders_per_minute, dry_run, auto_refresh, paused, mode,
                  total_price_markup_pct)
                 VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    defaults.delay_per_order,
                    defaults.max_orders_per_minute,
                    dry_run as i64,
                    defaults.auto_refresh as i64,
                    paused as i64,
                    defaults.mode.as_str(),
                    defaults.total_price_markup_pct
                ],
            )
            .map_err(|e| format!("could not seed settings: {e}"))?;

        self.db
            .execute(
                "INSERT OR IGNORE INTO stats
                 (id, since, profit_rlt, spent_rlt, bought, filled, failed, orders_sent)
                 VALUES (1, ?1, 0, 0, 0, 0, 0, 0)",
                params![now],
            )
            .map_err(|e| format!("could not seed stats: {e}"))?;

        self.db
            .execute(
                "INSERT OR IGNORE INTO session
                 (id, access_token, refresh_token, cookie, source, updated_at,
                  last_refresh_at, refresh_count, last_error)
                 VALUES (1, '', '', '', NULL, NULL, NULL, 0, NULL)",
                [],
            )
            .map_err(|e| format!("could not seed session: {e}"))?;
        Ok(())
    }

    /// Rebuilds the in-memory state from SQLite after something wrote rows
    /// behind the store's back — today only a backup restore.
    pub fn reload(&mut self) -> Result<(), String> {
        self.ensure_rows()?;
        self.hydrate()
    }

    pub(super) fn hydrate(&mut self) -> Result<(), String> {
        let mut state = State::fresh();

        state.worker_token_hash = self.get_meta("worker_token_hash")?;
        state.worker_token_preview = self.get_meta("worker_token_preview")?;
        if let Some(value) = self.get_meta("created_at")? {
            state.created_at = value.parse().unwrap_or(state.created_at);
        }

        let settings_row = self
            .db
            .query_row(
                "SELECT delay_per_order, max_orders_per_minute, dry_run, auto_refresh, paused, mode,
                        total_price_markup_pct
                 FROM settings WHERE id = 1",
                [],
                |row| {
                    let dry_run: i64 = row.get(2)?;
                    let paused: i64 = row.get(4)?;
                    let mode: String = row.get(5)?;
                    Ok(Settings {
                        delay_per_order: row.get(0)?,
                        max_orders_per_minute: row.get(1)?,
                        // An unknown string (a newer build wrote it) falls
                        // back to the mirrored flags rather than guessing.
                        mode: Mode::parse(&mode)
                            .unwrap_or_else(|| Mode::from_legacy(dry_run != 0, paused != 0)),
                        auto_refresh: row.get(3)?,
                        total_price_markup_pct: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(|e| format!("could not read settings: {e}"))?;
        if let Some(settings) = settings_row {
            state.settings = settings;
        }

        let stats_row = self
            .db
            .query_row(
                "SELECT since, profit_rlt, spent_rlt, bought, filled, failed, orders_sent
                 FROM stats WHERE id = 1",
                [],
                |row| {
                    Ok(Stats {
                        since: row.get(0)?,
                        profit_rlt: row.get(1)?,
                        spent_rlt: row.get(2)?,
                        bought: row.get(3)?,
                        filled: row.get(4)?,
                        failed: row.get(5)?,
                        orders_sent: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(|e| format!("could not read stats: {e}"))?;
        if let Some(stats) = stats_row {
            state.stats = stats;
        }

        let session_row = self
            .db
            .query_row(
                "SELECT access_token, refresh_token, cookie, source, updated_at,
                        last_refresh_at, refresh_count, last_error
                 FROM session WHERE id = 1",
                [],
                |row| {
                    Ok(Session {
                        access_token: row.get(0)?,
                        refresh_token: row.get(1)?,
                        cookie: row.get(2)?,
                        source: row.get(3)?,
                        updated_at: row.get(4)?,
                        last_refresh_at: row.get(5)?,
                        refresh_count: row.get(6)?,
                        last_error: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(|e| format!("could not read session: {e}"))?;
        if let Some(session) = session_row {
            state.session = session;
        }

        state.configs = self.read_targets()?;
        state.history = self.read_recent_history(MAX_HISTORY)?;
        state.report = self.read_report()?;
        if let Some(value) = self.get_meta("report_since")? {
            state.report_since = value.parse().unwrap_or(state.report_since);
        }

        state.updated_at = now_ms();
        self.state = state;
        self.reindex_configs();
        Ok(())
    }

    fn read_targets(&self) -> Result<Vec<Target>, String> {
        let mut statement = self
            .db
            .prepare(
                "SELECT item_id, label, item_type, rarity_color, target_price, desired_qty,
                        high_stock_threshold, min_acceptable_qty, allow_partial_order, active,
                        created_at, updated_at
                 FROM targets ORDER BY created_at ASC, item_id ASC",
            )
            .map_err(|e| format!("could not prepare target read: {e}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok(Target {
                    item_id: row.get(0)?,
                    label: row.get(1)?,
                    item_type: row.get(2)?,
                    rarity_color: row.get(3)?,
                    target_price: row.get(4)?,
                    desired_qty: row.get(5)?,
                    high_stock_threshold: row.get(6)?,
                    min_acceptable_qty: row.get(7)?,
                    allow_partial_order: row.get::<_, i64>(8)? != 0,
                    active: row.get::<_, i64>(9)? != 0,
                    created_at: row.get(10)?,
                    updated_at: row.get(11)?,
                })
            })
            .map_err(|e| format!("could not read targets: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("could not decode targets: {e}"))
    }

    fn read_recent_history(&self, limit: usize) -> Result<Vec<OrderEntry>, String> {
        let mut statement = self
            .db
            .prepare(
                "SELECT id, order_id, item_id, label, qty, requested_qty, stock_at_trigger, unit_price, total_price,
                        target_price, profit_rlt, status, at, latency_ms, note, message
                 FROM orders ORDER BY at DESC, id DESC LIMIT ?1",
            )
            .map_err(|e| format!("could not prepare history read: {e}"))?;
        let rows = statement
            .query_map(params![limit as i64], row_to_entry_lite)
            .map_err(|e| format!("could not read history: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("could not decode history: {e}"))
    }

    fn read_report(&self) -> Result<BTreeMap<String, Product>, String> {
        let mut statement = self
            .db
            .prepare(
                "SELECT product_key, item_id, label, fills, failed, qty, spent_rlt,
                        profit_rlt, first_at, last_at FROM report_products",
            )
            .map_err(|e| format!("could not prepare report read: {e}"))?;
        let rows = statement
            .query_map([], |row| {
                let key: String = row.get(0)?;
                let product = Product {
                    item_id: row.get(1)?,
                    label: row.get(2)?,
                    fills: row.get(3)?,
                    failed: row.get(4)?,
                    qty: row.get(5)?,
                    spent_rlt: row.get(6)?,
                    profit_rlt: row.get(7)?,
                    first_at: row.get(8)?,
                    last_at: row.get(9)?,
                };
                Ok((key, product))
            })
            .map_err(|e| format!("could not read report: {e}"))?;
        rows.collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(|e| format!("could not decode report: {e}"))
    }

    // ── meta ────────────────────────────────────────────────────────────────

    pub(super) fn get_meta(&self, key: &str) -> Result<Option<String>, String> {
        self.db
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("could not read meta {key}: {e}"))
    }

    pub(super) fn set_meta(&self, key: &str, value: &str) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
            .map_err(|e| format!("could not write meta {key}: {e}"))
    }
}
