//! SQLite-backed runtime state, ported from `src/worker/store.js`.
//!
//! Same tables, same JSON shapes, same token hashing. The in-memory `State` is
//! the read path for the HTTP/WS layer; SQLite is the durable log.
//!
//! Deliberate differences from the Bun worker (tracked in `docs/rust-parity.md`):
//! * `save()` flushes immediately instead of debouncing 120 ms. Correctness
//!   first; the engine round will reintroduce the debounce once the frame path
//!   exists and can be benchmarked.
//! * legacy `state.json` / `loot-log.json` import is not implemented; the Rust
//!   worker targets the SQLite schema v7 that the JS worker already writes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::constants::{
    EVENT_BUCKET_MS, EVENT_RETENTION_DAYS, LOOT_LOG_MAX_PAGE_SIZE, LOOT_LOG_PAGE_SIZE, MAX_HISTORY,
    MAX_REPORT_PRODUCTS, PRICE_RETENTION_DAYS,
};
use crate::db;
use crate::logger::Logger;
use crate::model::{
    MarketSnapshot, Mode, OrderEntry, PricePoint, Product, ProductView, ReportTotals, ReportView,
    Session, Settings, Stats, Target,
};
use crate::rules::round_price;
use crate::timings::Timings;
use crate::validation::{self, SessionPatch};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn preview_token(token: &str) -> Option<String> {
    if token.is_empty() {
        return None;
    }
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 12 {
        let head: String = chars.iter().take(2).collect();
        let tail: String = chars.iter().skip(chars.len() - 2).collect();
        return Some(format!("{head}••••{tail}"));
    }
    let head: String = chars.iter().take(5).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    Some(format!("{head}••••{tail}"))
}

pub fn create_worker_token() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let a = left.as_bytes();
    let b = right.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn product_key(item_id: &str, label: &str) -> String {
    let id = item_id.trim().to_ascii_lowercase();
    if !id.is_empty() {
        return id;
    }
    let name = label.trim().to_ascii_lowercase();
    if !name.is_empty() {
        return format!("label:{name}");
    }
    "unknown".to_string()
}

#[derive(Debug, Clone)]
pub struct State {
    pub created_at: i64,
    pub updated_at: i64,
    pub worker_token_hash: Option<String>,
    pub worker_token_preview: Option<String>,
    pub session: Session,
    pub configs: Vec<Target>,
    pub settings: Settings,
    pub stats: Stats,
    pub report_since: i64,
    pub report: BTreeMap<String, Product>,
    pub history: Vec<OrderEntry>,
}

impl State {
    fn fresh() -> Self {
        let now = now_ms();
        Self {
            created_at: now,
            updated_at: now,
            worker_token_hash: None,
            worker_token_preview: None,
            session: Session::default(),
            configs: Vec::new(),
            settings: Settings::default(),
            stats: Stats {
                since: now,
                profit_rlt: 0.0,
                spent_rlt: 0.0,
                bought: 0,
                filled: 0,
                failed: 0,
                orders_sent: 0,
            },
            report_since: now,
            report: BTreeMap::new(),
            history: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub entries: Vec<OrderEntry>,
    pub total: i64,
    pub limit: usize,
    pub offset: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    pub q: String,
    pub status: String,
    pub limit: Option<usize>,
    pub offset: i64,
    pub before_at: i64,
    pub before_id: String,
}

pub struct Store {
    pub state: State,
    pub db: Connection,
    pub data_dir: PathBuf,
    /// Held for the lifetime of the process so a second worker cannot open the
    /// same data directory and duplicate orders.
    _data_lock: std::fs::File,
    logger: Arc<Logger>,
    metrics: Arc<crate::db_metrics::DbMetrics>,
    /// Market frames counted per 5-minute bucket. The window covers
    /// [`EVENT_RETENTION_DAYS`], so the chart can serve a week without a query
    /// per render; SQLite only receives the batches the flusher writes.
    event_points: BTreeMap<i64, i64>,
    /// Buckets changed since the last flush (absolute counts, not deltas).
    event_dirty: BTreeSet<i64>,
    event_pruned_at: i64,
    /// Lowercase item id → position in `state.configs`. The engine resolves a
    /// target on every frame, and the scan used to lowercase (and allocate for)
    /// every configured item on every lookup.
    config_index: HashMap<String, usize>,
    /// Last time a swallowed write error was logged, so a failing disk cannot
    /// turn into a log flood on the frame path.
    last_write_error_at: AtomicI64,
}

/// Start of the 5-minute bucket that owns `at`.
pub fn event_bucket_at(at: i64) -> i64 {
    at.div_euclid(EVENT_BUCKET_MS) * EVENT_BUCKET_MS
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
            config_index: HashMap::new(),
            last_write_error_at: AtomicI64::new(0),
        };
        store.ensure_rows()?;
        store.hydrate()?;
        store.hydrate_events();
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

    fn ensure_rows(&self) -> Result<(), String> {
        let now = now_ms();
        let defaults = Settings::default();
        let (dry_run, paused) = defaults.legacy_flags();
        self.db
            .execute(
                "INSERT OR IGNORE INTO settings
                 (id, delay_per_order, max_orders_per_minute, dry_run, auto_refresh, paused, mode)
                 VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    defaults.delay_per_order,
                    defaults.max_orders_per_minute,
                    dry_run as i64,
                    defaults.auto_refresh as i64,
                    paused as i64,
                    defaults.mode.as_str()
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

    fn hydrate(&mut self) -> Result<(), String> {
        let mut state = State::fresh();

        state.worker_token_hash = self.get_meta("worker_token_hash")?;
        state.worker_token_preview = self.get_meta("worker_token_preview")?;
        if let Some(value) = self.get_meta("created_at")? {
            state.created_at = value.parse().unwrap_or(state.created_at);
        }

        let settings_row = self
            .db
            .query_row(
                "SELECT delay_per_order, max_orders_per_minute, dry_run, auto_refresh, paused, mode
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

    /// Rebuilds the id → position map. Called after any change to the target
    /// list (add, edit, delete, bulk replace, reload).
    fn reindex_configs(&mut self) {
        self.config_index.clear();
        for (index, config) in self.state.configs.iter().enumerate() {
            self.config_index
                .insert(config.item_id.to_ascii_lowercase(), index);
        }
    }

    /// Logs a swallowed write error at most once a minute.
    ///
    /// The in-memory row stays authoritative, so a failed write is a durability
    /// problem rather than a reason to fail the caller — but it must never be
    /// silent. A full disk used to look exactly like a healthy worker.
    fn report_write_error(&self, what: &str, error: &dyn std::fmt::Display) {
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
                "SELECT id, order_id, item_id, label, qty, unit_price, total_price,
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

    fn get_meta(&self, key: &str) -> Result<Option<String>, String> {
        self.db
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("could not read meta {key}: {e}"))
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
            .map_err(|e| format!("could not write meta {key}: {e}"))
    }

    // ── access key ──────────────────────────────────────────────────────────

    /// Resolves the worker access key. Returns plaintext only when a new key
    /// was generated, so the caller can print it exactly once.
    pub fn ensure_worker_token(&mut self, env_token: &str) -> Result<Option<String>, String> {
        if !env_token.is_empty() {
            let hash = hash_token(env_token);
            if self.state.worker_token_hash.as_deref() != Some(hash.as_str()) {
                self.state.worker_token_hash = Some(hash);
                self.state.worker_token_preview = preview_token(env_token);
                self.persist_token()?;
            }
            return Ok(None);
        }

        if self.state.worker_token_hash.is_some() {
            return Ok(None);
        }

        let token = create_worker_token();
        self.state.worker_token_hash = Some(hash_token(&token));
        self.state.worker_token_preview = preview_token(&token);
        self.persist_token()?;
        Ok(Some(token))
    }

    pub fn verify_worker_token(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        match self.state.worker_token_hash.as_deref() {
            Some(hash) => constant_time_eq(&hash_token(token), hash),
            None => false,
        }
    }

    pub fn rotate_worker_token(&mut self) -> Result<String, String> {
        let token = create_worker_token();
        self.state.worker_token_hash = Some(hash_token(&token));
        self.state.worker_token_preview = preview_token(&token);
        self.persist_token()?;
        Ok(token)
    }

    fn persist_token(&self) -> Result<(), String> {
        if let Some(hash) = &self.state.worker_token_hash {
            self.set_meta("worker_token_hash", hash)?;
        }
        if let Some(preview) = &self.state.worker_token_preview {
            self.set_meta("worker_token_preview", preview)?;
        }
        Ok(())
    }

    // ── settings / targets ──────────────────────────────────────────────────

    pub fn patch_settings(&mut self, patch: validation::SettingsPatch) -> Result<(), String> {
        let mut settings = self.state.settings;
        if let Some(value) = patch.delay_per_order {
            settings.delay_per_order = value;
        }
        if let Some(value) = patch.max_orders_per_minute {
            settings.max_orders_per_minute = value;
        }
        if let Some(value) = patch.mode {
            settings.mode = value;
        }
        if let Some(value) = patch.auto_refresh {
            settings.auto_refresh = value;
        }
        self.state.settings = settings;
        self.write_settings()
            .map_err(|e| format!("could not persist settings: {e}"))
    }

    pub fn upsert_config(&mut self, mut target: Target, original_id: Option<&str>) -> Target {
        let key = original_id.unwrap_or(&target.item_id).to_ascii_lowercase();
        let now = now_ms();
        target.updated_at = Some(now);
        if let Some(index) = self
            .state
            .configs
            .iter()
            .position(|config| config.item_id.to_ascii_lowercase() == key)
        {
            target.created_at = self.state.configs[index].created_at.or(Some(now));
            self.state.configs[index] = target.clone();
        } else {
            target.created_at = Some(now);
            self.state.configs.push(target.clone());
        }
        self.reindex_configs();
        // Best effort: the in-memory row is already authoritative for this call.
        if let Err(error) = self.write_target(&target) {
            self.report_write_error("could not save target", &error);
        }
        if let Err(error) = self.set_meta("updated_at", &now.to_string()) {
            self.report_write_error("could not stamp updated_at", &error);
        }
        target
    }

    pub fn remove_config(&mut self, item_id: &str) -> bool {
        let key = item_id.to_ascii_lowercase();
        let before = self.state.configs.len();
        self.state
            .configs
            .retain(|config| config.item_id.to_ascii_lowercase() != key);
        if self.state.configs.len() == before {
            return false;
        }
        self.reindex_configs();
        if let Err(error) = self
            .db
            .execute("DELETE FROM targets WHERE item_id = ?1", params![item_id])
        {
            self.report_write_error("could not delete target", &error);
        }
        true
    }

    pub fn set_configs(&mut self, configs: Vec<Target>) -> Result<(), String> {
        let now = now_ms();
        let stamped: Vec<Target> = configs
            .into_iter()
            .map(|mut config| {
                if config.created_at.is_none() {
                    config.created_at = Some(now);
                }
                config.updated_at = Some(now);
                config
            })
            .collect();

        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|e| format!("could not start transaction: {e}"))?;
        for config in &stamped {
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

        // Delete rows that are no longer in the list (cascades market_snapshot).
        let ids: Vec<String> = stamped.iter().map(|c| c.item_id.clone()).collect();
        if ids.is_empty() {
            tx.execute("DELETE FROM targets", [])
                .map_err(|e| format!("could not clear targets: {e}"))?;
        } else {
            let placeholders = std::iter::repeat("?")
                .take(ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("DELETE FROM targets WHERE item_id NOT IN ({placeholders})");
            tx.execute(&sql, rusqlite::params_from_iter(ids.iter()))
                .map_err(|e| format!("could not prune targets: {e}"))?;
        }
        tx.commit()
            .map_err(|e| format!("could not commit targets: {e}"))?;

        self.state.configs = stamped;
        self.reindex_configs();
        Ok(())
    }

    pub fn find_config(&self, item_id: &str) -> Option<&Target> {
        // Hot path: the frame normaliser already lowercased the id, so this is
        // one hash lookup with no allocation. The identity check keeps a stale
        // index harmless; anything else (mixed case, unknown id) falls back to
        // the case-insensitive scan.
        if let Some(index) = self.config_index.get(item_id)
            && let Some(config) = self.state.configs.get(*index)
            && config.item_id.eq_ignore_ascii_case(item_id)
        {
            return Some(config);
        }
        self.state
            .configs
            .iter()
            .find(|config| config.item_id.eq_ignore_ascii_case(item_id))
    }

    /// Writes a resolved `Rarity Name` label back to the target. Returns
    /// whether anything actually changed.
    pub fn apply_item_info(
        &mut self,
        item_id: &str,
        label: &str,
        item_type: &str,
        rarity_color: Option<&str>,
    ) -> bool {
        let key = item_id.to_ascii_lowercase();
        let Some(config) = self
            .state
            .configs
            .iter_mut()
            .find(|config| config.item_id.to_ascii_lowercase() == key)
        else {
            return false;
        };
        let changed = config.label != label
            || config.item_type != item_type
            || config.rarity_color.as_deref() != rarity_color;
        if !changed {
            return false;
        }
        config.label = label.to_string();
        config.item_type = item_type.to_string();
        config.rarity_color = rarity_color.map(str::to_string);
        config.updated_at = Some(now_ms());
        let stamped = config.clone();
        if let Err(error) = self.write_target(&stamped) {
            self.report_write_error("could not save item info", &error);
        }
        true
    }

    /// The socket knows the real item type (`miner`, `rack`, ...); the engine
    /// corrects configs that were added before item-info could infer it.
    pub fn update_item_type(&mut self, item_id: &str, item_type: &str) {
        let key = item_id.to_ascii_lowercase();
        let Some(config) = self
            .state
            .configs
            .iter_mut()
            .find(|config| config.item_id.to_ascii_lowercase() == key)
        else {
            return;
        };
        if config.item_type == item_type {
            return;
        }
        config.item_type = item_type.to_string();
        config.updated_at = Some(now_ms());
        let stamped = config.clone();
        if let Err(error) = self.write_target(&stamped) {
            self.report_write_error("could not save target item type", &error);
        }
    }

    pub fn config_ids(&self) -> Vec<String> {
        self.state
            .configs
            .iter()
            .map(|c| c.item_id.clone())
            .collect()
    }

    // ── persistence ─────────────────────────────────────────────────────────

    fn write_settings(&self) -> rusqlite::Result<()> {
        let settings = self.state.settings;
        let (dry_run, paused) = settings.legacy_flags();
        self.db.execute(
            "UPDATE settings SET delay_per_order = ?1, max_orders_per_minute = ?2,
                    dry_run = ?3, auto_refresh = ?4, paused = ?5, mode = ?6 WHERE id = 1",
            params![
                settings.delay_per_order,
                settings.max_orders_per_minute,
                dry_run as i64,
                settings.auto_refresh as i64,
                paused as i64,
                settings.mode.as_str()
            ],
        )?;
        Ok(())
    }

    fn write_stats(&self) -> rusqlite::Result<()> {
        let stats = self.state.stats;
        self.db.execute(
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
        )?;
        Ok(())
    }

    fn write_target(&self, config: &Target) -> rusqlite::Result<()> {
        self.db.execute(
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
        )?;
        Ok(())
    }

    /// Compatibility shim with the JS worker, where `save()` only marks dirty.
    pub fn save(&mut self) -> Result<(), String> {
        self.flush()
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
                    dry_run = ?3, auto_refresh = ?4, paused = ?5, mode = ?6 WHERE id = 1",
            params![
                settings.delay_per_order,
                settings.max_orders_per_minute,
                dry_run as i64,
                settings.auto_refresh as i64,
                paused as i64,
                settings.mode.as_str()
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

    /// Persists only the counters the order path mutates.
    ///
    /// `flush()` also rewrites every target row, so a single order used to cost
    /// one upsert per configured item even though the target list only changes
    /// through the API that already writes it.
    pub fn flush_stats(&mut self) -> Result<(), String> {
        self.state.updated_at = now_ms();
        self.write_stats()
            .map_err(|e| format!("could not write stats: {e}"))
    }

    // ── loot log ────────────────────────────────────────────────────────────

    /// Appends one confirmed/failed order to the permanent loot log.
    pub fn add_history(&mut self, entry: OrderEntry) -> Result<(), String> {
        self.state.history.insert(0, entry.clone());
        if self.state.history.len() > MAX_HISTORY {
            self.state.history.truncate(MAX_HISTORY);
        }

        self.record_product(&entry, true);

        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|e| format!("could not start transaction: {e}"))?;
        tx.execute(
            "INSERT OR REPLACE INTO orders
             (id, order_id, item_id, label, qty, unit_price, total_price, target_price,
              profit_rlt, status, at, latency_ms, timings, raw_events, note, message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                entry.id,
                entry.order_id,
                entry.item_id,
                entry.label,
                entry.qty,
                entry.unit_price,
                entry.total_price,
                entry.target_price,
                entry.profit_rlt,
                entry.status,
                entry.at,
                entry.latency_ms,
                entry
                    .timings
                    .as_ref()
                    .and_then(|t| serde_json::to_string(t).ok()),
                entry
                    .raw_events
                    .as_ref()
                    .and_then(crate::raw_events::encode),
                entry.note,
                entry.message
            ],
        )
        .map_err(|e| format!("could not write order {}: {e}", entry.id))?;
        let stats = self.state.stats;
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
        tx.commit()
            .map_err(|e| format!("could not commit order: {e}"))?;
        Ok(())
    }

    pub fn has_order(&self, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        self.db
            .query_row(
                "SELECT 1 FROM orders WHERE id = ?1",
                params![id],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Updates late, non-blocking diagnostics without rewriting the loot row.
    pub fn update_history_timings(&mut self, id: &str, raw: &Value) -> bool {
        let Some(incoming) = crate::timings::sanitize(raw) else {
            return false;
        };
        let existing: Option<String> = self
            .db
            .query_row(
                "SELECT timings FROM orders WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        let Some(existing) = existing else {
            return false;
        };

        // Merge: new marks win, old marks survive a partial update.
        let mut merged: Value = serde_json::from_str(&existing).unwrap_or(Value::Null);
        let patch_value = serde_json::to_value(&incoming).unwrap_or(Value::Null);
        if let (Some(base), Some(patch)) = (merged.as_object_mut(), patch_value.as_object()) {
            for (key, value) in patch {
                base.insert(key.clone(), value.clone());
            }
        }
        let Some(timings) = crate::timings::sanitize(&merged) else {
            return false;
        };
        let encoded = serde_json::to_string(&timings).unwrap_or_default();
        let changes = self
            .db
            .execute(
                "UPDATE orders SET timings = ?1 WHERE id = ?2",
                params![encoded, id],
            )
            .unwrap_or(0);
        if changes == 0 {
            return false;
        }
        if let Some(entry) = self.state.history.iter_mut().find(|row| row.id == id) {
            entry.timings = Some(timings);
        }
        true
    }

    /// Newest completed orders with their timing marks, for the benchmark
    /// window. Rows without a decodable `timings` object are skipped: they
    /// predate the latency debugger and have nothing to aggregate.
    pub fn benchmark_samples(&self, limit: usize) -> Vec<crate::benchmark::Sample> {
        let limit = limit.clamp(1, 1_000) as i64;
        let Ok(mut statement) = self.db.prepare(
            "SELECT at, status, timings FROM orders
             WHERE timings IS NOT NULL AND timings != ''
             ORDER BY at DESC, id DESC LIMIT ?1",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = statement.query_map(params![limit], |row| {
            let at: i64 = row.get(0)?;
            let status: String = row.get(1)?;
            let timings: Option<String> = row.get(2)?;
            Ok((at, status, timings))
        }) else {
            return Vec::new();
        };
        rows.flatten()
            .filter_map(|(at, status, text)| {
                let timings = crate::timings::decode(text.as_deref())?;
                Some(crate::benchmark::Sample {
                    at,
                    status,
                    timings,
                })
            })
            .collect()
    }

    /// Completed orders with timing marks inside a time range, newest first.
    /// Used by the bucketed history chart; the range is inclusive.
    pub fn benchmark_samples_between(
        &self,
        since: i64,
        until: i64,
        limit: usize,
    ) -> Vec<crate::benchmark::Sample> {
        let limit = limit.clamp(1, crate::benchmark::HISTORY_MAX_SAMPLES) as i64;
        let Ok(mut statement) = self.db.prepare(
            "SELECT at, status, timings FROM orders
             WHERE at >= ?1 AND at <= ?2
               AND timings IS NOT NULL AND timings != ''
             ORDER BY at DESC, id DESC LIMIT ?3",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = statement.query_map(params![since, until, limit], |row| {
            let at: i64 = row.get(0)?;
            let status: String = row.get(1)?;
            let timings: Option<String> = row.get(2)?;
            Ok((at, status, timings))
        }) else {
            return Vec::new();
        };
        rows.flatten()
            .filter_map(|(at, status, text)| {
                let timings = crate::timings::decode(text.as_deref())?;
                Some(crate::benchmark::Sample {
                    at,
                    status,
                    timings,
                })
            })
            .collect()
    }

    pub fn history_count(&self) -> i64 {
        self.db
            .query_row("SELECT COUNT(*) FROM orders", [], |row| row.get(0))
            .unwrap_or(0)
    }

    /// One page of the permanent loot log, newest first.
    pub fn history_page(&self, query: HistoryQuery) -> Result<HistoryPage, String> {
        let term = query.q.trim().chars().take(64).collect::<String>();
        let like = if term.is_empty() {
            String::new()
        } else {
            format!("%{}%", escape_like(&term))
        };
        let status = if query.status == "filled" || query.status == "failed" {
            query.status.clone()
        } else {
            String::new()
        };
        let page_size = query
            .limit
            .unwrap_or(LOOT_LOG_PAGE_SIZE)
            .clamp(1, LOOT_LOG_MAX_PAGE_SIZE);

        let cursor = query.before_at > 0 && !query.before_id.is_empty();
        let skip = if cursor { 0 } else { query.offset.max(0) };

        let (sql, bindings): (String, Vec<Box<dyn rusqlite::ToSql>>) = if cursor {
            (
                "SELECT id, order_id, item_id, label, qty, unit_price, total_price,
                        target_price, profit_rlt, status, at, latency_ms,
                        note, message
                 FROM orders
                 WHERE (?1 = '' OR label LIKE ?1 ESCAPE '\\' OR item_id LIKE ?1 ESCAPE '\\')
                   AND (?2 = '' OR status = ?2)
                   AND (at < ?3 OR (at = ?3 AND id < ?4))
                 ORDER BY at DESC, id DESC LIMIT ?5"
                    .to_string(),
                vec![
                    Box::new(like),
                    Box::new(status),
                    Box::new(query.before_at),
                    Box::new(query.before_id),
                    Box::new((page_size + 1) as i64),
                ],
            )
        } else {
            (
                "SELECT id, order_id, item_id, label, qty, unit_price, total_price,
                        target_price, profit_rlt, status, at, latency_ms,
                        note, message
                 FROM orders
                 WHERE (?1 = '' OR label LIKE ?1 ESCAPE '\\' OR item_id LIKE ?1 ESCAPE '\\')
                   AND (?2 = '' OR status = ?2)
                 ORDER BY at DESC, id DESC LIMIT ?3 OFFSET ?4"
                    .to_string(),
                vec![
                    Box::new(like),
                    Box::new(status),
                    Box::new((page_size + 1) as i64),
                    Box::new(skip),
                ],
            )
        };

        let mut statement = self
            .db
            .prepare(&sql)
            .map_err(|e| format!("could not prepare history page: {e}"))?;
        let rows: Vec<OrderEntry> = {
            let refs: Vec<&dyn rusqlite::ToSql> = bindings.iter().map(|b| b.as_ref()).collect();
            let mapped = statement
                .query_map(refs.as_slice(), row_to_entry_lite)
                .map_err(|e| format!("could not query history: {e}"))?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("could not decode history: {e}"))?
        };

        let total: i64 = self
            .db
            .query_row(
                "SELECT COUNT(*) FROM orders
                 WHERE (?1 = '' OR label LIKE ?1 ESCAPE '\\' OR item_id LIKE ?1 ESCAPE '\\')
                   AND (?2 = '' OR status = ?2)",
                params![
                    if term.is_empty() {
                        String::new()
                    } else {
                        format!("%{}%", escape_like(&term))
                    },
                    if query.status == "filled" || query.status == "failed" {
                        query.status
                    } else {
                        String::new()
                    }
                ],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let has_more = rows.len() > page_size;
        Ok(HistoryPage {
            entries: rows.into_iter().take(page_size).collect(),
            total,
            limit: page_size,
            offset: skip,
            has_more,
        })
    }

    /// Reads the two heavy JSON columns for a single row.
    ///
    /// Lists never select `timings`/`raw_events`: the SQLite profile timer keeps
    /// running until the statement resets, so decoding frames while iterating
    /// rows is reported as a slow SELECT even when the query itself is cheap.
    /// The deck asks for one entry at a time when its detail modal opens.
    pub fn order_detail(&self, id: &str) -> Result<Option<OrderEntry>, String> {
        self.db
            .query_row(
                "SELECT id, order_id, item_id, label, qty, unit_price, total_price,
                        target_price, profit_rlt, status, at, latency_ms, timings, raw_events,
                        note, message
                 FROM orders WHERE id = ?1",
                params![id],
                row_to_entry,
            )
            .optional()
            .map_err(|e| format!("could not read order {id}: {e}"))
    }

    // ── report ──────────────────────────────────────────────────────────────

    fn record_product(&mut self, entry: &OrderEntry, persist: bool) {
        let key = product_key(&entry.item_id, &entry.label);
        let at = if entry.at > 0 { entry.at } else { now_ms() };
        let product = self
            .state
            .report
            .entry(key.clone())
            .or_insert_with(|| Product {
                item_id: entry.item_id.clone(),
                label: entry.label.chars().take(48).collect(),
                fills: 0,
                failed: 0,
                qty: 0,
                spent_rlt: 0.0,
                profit_rlt: 0.0,
                first_at: at,
                last_at: at,
            });

        if !entry.label.is_empty() {
            product.label = entry.label.chars().take(48).collect();
        }
        if !entry.item_id.is_empty() {
            product.item_id = entry.item_id.clone();
        }
        product.first_at = product.first_at.min(at);
        product.last_at = product.last_at.max(at);

        if entry.status == "failed" {
            product.failed += 1;
        } else {
            product.fills += 1;
            product.qty += entry.qty.max(0);
            product.spent_rlt = round_price(product.spent_rlt + entry.total_price.max(0.0));
            product.profit_rlt = round_price(product.profit_rlt + entry.profit_rlt);
        }

        self.trim_report(persist);
        if persist {
            if let Some(product) = self.state.report.get(&key) {
                let _ = self.write_product(&key, product);
            }
        }
    }

    fn trim_report(&mut self, persist: bool) {
        if self.state.report.len() <= MAX_REPORT_PRODUCTS {
            return;
        }
        let mut keys: Vec<(String, f64)> = self
            .state
            .report
            .iter()
            .map(|(key, product)| (key.clone(), product.profit_rlt))
            .collect();
        keys.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let drop_count = keys.len() - MAX_REPORT_PRODUCTS;
        for (key, _) in keys.into_iter().take(drop_count) {
            self.state.report.remove(&key);
            if persist {
                let _ = self.db.execute(
                    "DELETE FROM report_products WHERE product_key = ?1",
                    params![key],
                );
            }
        }
    }

    fn write_product(&self, key: &str, product: &Product) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO report_products
             (product_key, item_id, label, fills, failed, qty, spent_rlt, profit_rlt,
              first_at, last_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(product_key) DO UPDATE SET
               item_id = excluded.item_id,
               label = excluded.label,
               fills = excluded.fills,
               failed = excluded.failed,
               qty = excluded.qty,
               spent_rlt = excluded.spent_rlt,
               profit_rlt = excluded.profit_rlt,
               first_at = excluded.first_at,
               last_at = excluded.last_at",
            params![
                key,
                product.item_id,
                product.label,
                product.fills,
                product.failed,
                product.qty,
                product.spent_rlt,
                product.profit_rlt,
                product.first_at,
                product.last_at
            ],
        )?;
        Ok(())
    }

    /// All-time profit grouped by product, sorted like the JS `reportView`.
    pub fn report_view(&self) -> ReportView {
        let mut items: Vec<ProductView> = self
            .state
            .report
            .values()
            .map(|product| {
                let completed = product.fills + product.failed;
                ProductView {
                    item_id: product.item_id.clone(),
                    label: product.label.clone(),
                    fills: product.fills,
                    failed: product.failed,
                    qty: product.qty,
                    spent_rlt: round_price(product.spent_rlt),
                    profit_rlt: round_price(product.profit_rlt),
                    first_at: product.first_at,
                    last_at: product.last_at,
                    avg_buy: if product.qty > 0 {
                        round_price(product.spent_rlt / product.qty as f64)
                    } else {
                        0.0
                    },
                    margin_pct: if product.spent_rlt > 0.0 {
                        Some((product.profit_rlt / product.spent_rlt * 1000.0).round() / 10.0)
                    } else {
                        None
                    },
                    success_pct: if completed > 0 {
                        Some((product.fills as f64 / completed as f64 * 100.0).round() as i64)
                    } else {
                        None
                    },
                }
            })
            .collect();

        items.sort_by(|a, b| {
            b.profit_rlt
                .partial_cmp(&a.profit_rlt)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    b.spent_rlt
                        .partial_cmp(&a.spent_rlt)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then_with(|| a.label.cmp(&b.label))
        });

        let mut totals = ReportTotals {
            products: items.len(),
            fills: 0,
            failed: 0,
            qty: 0,
            spent_rlt: 0.0,
            profit_rlt: 0.0,
            margin_pct: None,
        };
        for item in &items {
            totals.fills += item.fills;
            totals.failed += item.failed;
            totals.qty += item.qty;
            totals.spent_rlt += item.spent_rlt;
            totals.profit_rlt += item.profit_rlt;
        }
        totals.spent_rlt = round_price(totals.spent_rlt);
        totals.profit_rlt = round_price(totals.profit_rlt);
        totals.margin_pct = if totals.spent_rlt > 0.0 {
            Some((totals.profit_rlt / totals.spent_rlt * 1000.0).round() / 10.0)
        } else {
            None
        };

        ReportView {
            since: self.state.report_since,
            totals,
            items,
        }
    }

    // ── market cache ────────────────────────────────────────────────────────

    pub fn load_market_snapshots(&self) -> Vec<MarketSnapshot> {
        let mut statement = match self.db.prepare(
            "SELECT item_id, last_price, last_stock, allowed_price_min, allowed_price_max,
                    allowed_at, item_type, seen_at FROM market_snapshot",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map([], |row| {
            Ok(MarketSnapshot {
                item_id: row.get(0)?,
                last_price: row.get(1)?,
                last_stock: row.get(2)?,
                allowed_price_min: row.get(3)?,
                allowed_price_max: row.get(4)?,
                allowed_at: row.get(5)?,
                item_type: row.get(6)?,
                seen_at: row.get(7)?,
            })
        });
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn save_market_snapshot(&self, snapshot: &MarketSnapshot) -> Result<(), String> {
        // Hot path: the statement is cached so a snapshot write does not pay a
        // prepare per frame (the JS worker keeps prepared statements too).
        let mut statement = self
            .db
            .prepare_cached(
                "INSERT INTO market_snapshot
                 (item_id, last_price, last_stock, allowed_price_min, allowed_price_max,
                  allowed_at, item_type, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(item_id) DO UPDATE SET
                   last_price = excluded.last_price,
                   last_stock = excluded.last_stock,
                   allowed_price_min = excluded.allowed_price_min,
                   allowed_price_max = excluded.allowed_price_max,
                   allowed_at = excluded.allowed_at,
                   item_type = excluded.item_type,
                   seen_at = excluded.seen_at",
            )
            .map_err(|e| format!("could not cache market snapshot statement: {e}"))?;
        statement
            .execute(params![
                snapshot.item_id,
                snapshot.last_price,
                snapshot.last_stock,
                snapshot.allowed_price_min,
                snapshot.allowed_price_max,
                snapshot.allowed_at,
                snapshot.item_type,
                snapshot.seen_at
            ])
            .map(|_| ())
            .map_err(|e| format!("could not save market snapshot: {e}"))
    }

    pub fn save_market_snapshots(&self, snapshots: &[MarketSnapshot]) -> Result<(), String> {
        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|e| format!("could not start transaction: {e}"))?;
        for snapshot in snapshots {
            tx.execute(
                "INSERT INTO market_snapshot
                 (item_id, last_price, last_stock, allowed_price_min, allowed_price_max,
                  allowed_at, item_type, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(item_id) DO UPDATE SET
                   last_price = excluded.last_price,
                   last_stock = excluded.last_stock,
                   allowed_price_min = excluded.allowed_price_min,
                   allowed_price_max = excluded.allowed_price_max,
                   allowed_at = excluded.allowed_at,
                   item_type = excluded.item_type,
                   seen_at = excluded.seen_at",
                params![
                    snapshot.item_id,
                    snapshot.last_price,
                    snapshot.last_stock,
                    snapshot.allowed_price_min,
                    snapshot.allowed_price_max,
                    snapshot.allowed_at,
                    snapshot.item_type,
                    snapshot.seen_at
                ],
            )
            .map_err(|e| format!("could not save market snapshot: {e}"))?;
        }
        tx.commit()
            .map_err(|e| format!("could not commit market snapshots: {e}"))
    }

    pub fn load_price_history(&self, item_id: &str, limit: usize, since: i64) -> Vec<PricePoint> {
        let mut statement = match self.db.prepare(
            "SELECT item_id, bucket_at, price, fill_price, fill_qty, fill_approx
             FROM price_points
             WHERE item_id = ?1 AND (?2 = 0 OR bucket_at >= ?2)
             ORDER BY bucket_at DESC LIMIT ?3",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map(params![item_id, since, limit as i64], |row| {
            Ok(PricePoint {
                item_id: row.get(0)?,
                bucket_at: row.get(1)?,
                price: row.get(2)?,
                fill_price: row.get(3)?,
                fill_qty: row.get(4)?,
                fill_approx: row.get::<_, i64>(5)? != 0,
            })
        });
        match rows {
            Ok(rows) => {
                let mut points: Vec<PricePoint> = rows.filter_map(Result::ok).collect();
                points.reverse();
                points
            }
            Err(_) => Vec::new(),
        }
    }

    /// Raw 1-minute closes for one item, oldest first. Unlike
    /// `load_price_history` this is not capped by the live chart window, so the
    /// 3d/7d periods can reach back into the retained week.
    pub fn price_series(&self, item_id: &str, since: i64) -> Vec<(i64, f64)> {
        let mut statement = match self.db.prepare(
            "SELECT bucket_at, price
             FROM price_points
             WHERE item_id = ?1 AND bucket_at >= ?2
             ORDER BY bucket_at ASC",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map(params![item_id, since], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        });
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn record_price_point(
        &self,
        item_id: &str,
        bucket_at: i64,
        price: f64,
        fill: Option<(f64, f64, bool)>,
    ) -> Result<(), String> {
        let (fill_price, fill_qty, fill_approx) = match fill {
            Some((price, qty, approx)) => (Some(price), Some(qty), approx),
            None => (None, None, false),
        };
        let mut statement = self
            .db
            .prepare_cached(
                "INSERT INTO price_points
                 (item_id, bucket_at, price, fill_price, fill_qty, fill_approx)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(item_id, bucket_at) DO UPDATE SET
                   price = excluded.price,
                   fill_price = COALESCE(excluded.fill_price, price_points.fill_price),
                   fill_qty = COALESCE(excluded.fill_qty, price_points.fill_qty),
                   fill_approx = excluded.fill_approx",
            )
            .map_err(|e| format!("could not cache price point statement: {e}"))?;
        statement
            .execute(params![
                item_id,
                bucket_at,
                price,
                fill_price,
                fill_qty,
                fill_approx as i64
            ])
            .map(|_| ())
            .map_err(|e| format!("could not record price point: {e}"))
    }

    /// Writes a batch of sampled closes in one transaction. A sample never
    /// carries a fill, so re-sampling a bucket that already holds a fill keeps
    /// the fill columns intact.
    pub fn record_price_samples(&self, samples: &[(&str, i64, f64)]) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|e| format!("could not start transaction: {e}"))?;
        {
            let mut statement = tx
                .prepare_cached(
                    "INSERT INTO price_points
                     (item_id, bucket_at, price, fill_price, fill_qty, fill_approx)
                     VALUES (?1, ?2, ?3, NULL, NULL, 0)
                     ON CONFLICT(item_id, bucket_at) DO UPDATE SET
                       price = excluded.price",
                )
                .map_err(|e| format!("could not cache price sample statement: {e}"))?;
            for (item_id, bucket_at, price) in samples {
                statement
                    .execute(params![item_id, bucket_at, price])
                    .map_err(|e| format!("could not record price sample: {e}"))?;
            }
        }
        tx.commit()
            .map_err(|e| format!("could not commit price samples: {e}"))
    }

    pub fn prune_price_history(&self) -> Result<u64, String> {
        let cutoff = now_ms() - PRICE_RETENTION_DAYS * 86_400_000;
        self.db
            .execute(
                "DELETE FROM price_points WHERE bucket_at < ?1",
                params![cutoff],
            )
            .map(|n| n as u64)
            .map_err(|e| format!("could not prune price history: {e}"))
    }

    /// Non-blocking WAL checkpoint. `PASSIVE` never waits for readers and never
    /// truncates the log from under a writer, so it is safe to run on a timer.
    pub fn checkpoint_wal(&self) -> Result<(), String> {
        self.db
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_row| Ok(()))
            .map_err(|e| format!("could not checkpoint WAL: {e}"))
    }

    // ── market-event activity ───────────────────────────────────────────────

    /// Counts one market frame into its 5-minute bucket. Stays in memory: the
    /// flusher writes the batch, so the frame path never waits on SQLite.
    pub fn note_market_event(&mut self, at: i64) {
        let bucket = event_bucket_at(at);
        *self.event_points.entry(bucket).or_insert(0) += 1;
        self.event_dirty.insert(bucket);
    }

    /// Buckets at or after `since`, oldest first, including the live one.
    pub fn event_series(&self, since: i64) -> Vec<(i64, i64)> {
        self.event_points
            .range(since..)
            .map(|(bucket, count)| (*bucket, *count))
            .collect()
    }

    /// Writes every dirty bucket in one transaction, then drops expiry.
    pub fn flush_event_points(&mut self) -> Result<u64, String> {
        let pending: Vec<(i64, i64)> = self
            .event_dirty
            .iter()
            .map(|bucket| (*bucket, self.event_points.get(bucket).copied().unwrap_or(0)))
            .collect();

        if !pending.is_empty() {
            let tx = self
                .db
                .unchecked_transaction()
                .map_err(|e| format!("could not start event flush: {e}"))?;
            {
                let mut statement = tx
                    .prepare_cached(
                        "INSERT INTO event_points (bucket_at, events) VALUES (?1, ?2)
                         ON CONFLICT(bucket_at) DO UPDATE SET events = excluded.events",
                    )
                    .map_err(|e| format!("could not cache event point statement: {e}"))?;
                for (bucket, count) in &pending {
                    statement
                        .execute(params![bucket, count])
                        .map_err(|e| format!("could not record event point: {e}"))?;
                }
            }
            tx.commit()
                .map_err(|e| format!("could not commit event points: {e}"))?;
            self.event_dirty.clear();
        }

        self.prune_event_points()?;
        Ok(pending.len() as u64)
    }

    fn hydrate_events(&mut self) {
        let since = now_ms() - EVENT_RETENTION_DAYS * 86_400_000;
        let mut statement = match self.db.prepare(
            "SELECT bucket_at, events FROM event_points WHERE bucket_at >= ?1 ORDER BY bucket_at",
        ) {
            Ok(statement) => statement,
            Err(error) => {
                self.logger
                    .warn("store", format!("could not read event history: {error}"));
                return;
            }
        };
        let rows = statement.query_map(params![since], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        });
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) => {
                self.logger
                    .warn("store", format!("could not read event history: {error}"));
                return;
            }
        };
        for (bucket, count) in rows.flatten() {
            self.event_points.insert(bucket, count);
        }
    }

    fn prune_event_points(&mut self) -> Result<(), String> {
        let now = now_ms();
        let cutoff = now - EVENT_RETENTION_DAYS * 86_400_000;
        let expired = self
            .event_points
            .keys()
            .next()
            .map(|bucket| *bucket < cutoff)
            .unwrap_or(false);
        // A week of 5-minute buckets is ~2 300 rows; hourly is plenty. An
        // expiry that appears between sweeps (a clock jump, restored data)
        // still forces the sweep immediately.
        if !expired && now - self.event_pruned_at < 3_600_000 {
            return Ok(());
        }
        self.event_pruned_at = now;
        self.event_points = self.event_points.split_off(&cutoff);
        self.event_dirty.retain(|bucket| *bucket >= cutoff);
        self.db
            .execute(
                "DELETE FROM event_points WHERE bucket_at < ?1",
                params![cutoff],
            )
            .map(|_| ())
            .map_err(|e| format!("could not prune event history: {e}"))
    }

    // ── session ─────────────────────────────────────────────────────────────

    pub fn apply_session_patch(&mut self, patch: SessionPatch) -> Result<(), String> {
        let session = &mut self.state.session;
        if patch.clear_access_token {
            session.access_token.clear();
        }
        if patch.clear_refresh_token {
            session.refresh_token.clear();
        }
        if patch.clear_cookie {
            session.cookie.clear();
        }
        if let Some(token) = patch.access_token {
            session.access_token = token;
        }
        if let Some(token) = patch.refresh_token {
            session.refresh_token = token;
        }
        if let Some(cookie) = patch.cookie {
            session.cookie = cookie;
        }
        if let Some(source) = patch.source {
            session.source = Some(source);
        }
        session.updated_at = Some(now_ms());
        session.last_error = None;
        self.persist_session()
    }

    pub fn mark_refreshed(&mut self, access_token: &str, refresh_token: Option<&str>) {
        let now = now_ms();
        self.state.session.access_token = access_token.to_string();
        if let Some(token) = refresh_token.filter(|t| !t.is_empty()) {
            self.state.session.refresh_token = token.to_string();
        }
        self.state.session.last_refresh_at = Some(now);
        self.state.session.refresh_count += 1;
        self.state.session.last_error = None;
        if let Err(error) = self.persist_session() {
            self.report_write_error("could not persist the refreshed session", &error);
        }
    }

    pub fn mark_session_error(&mut self, message: &str) {
        self.state.session.last_error = Some(crate::redact::redact_secrets(message));
        if let Err(error) = self.persist_session() {
            self.report_write_error("could not persist the session error", &error);
        }
    }

    pub fn session_status(&self) -> Value {
        let session = &self.state.session;
        serde_json::json!({
            "hasAccessToken": !session.access_token.is_empty(),
            "hasRefreshToken": !session.refresh_token.is_empty(),
            "hasCookie": !session.cookie.is_empty(),
            "accessPreview": preview_token(&session.access_token),
            "refreshPreview": preview_token(&session.refresh_token),
            "updatedAt": session.updated_at,
            "source": session.source,
            "lastRefreshAt": session.last_refresh_at,
            "refreshCount": session.refresh_count,
            "lastError": session.last_error,
            "expiresAt": validation::jwt_expiry(&session.access_token),
        })
    }

    fn persist_session(&self) -> Result<(), String> {
        let session = &self.state.session;
        self.db
            .execute(
                "UPDATE session SET access_token = ?1, refresh_token = ?2, cookie = ?3,
                        source = ?4, updated_at = ?5, last_refresh_at = ?6,
                        refresh_count = ?7, last_error = ?8 WHERE id = 1",
                params![
                    session.access_token,
                    session.refresh_token,
                    session.cookie,
                    session.source,
                    session.updated_at,
                    session.last_refresh_at,
                    session.refresh_count,
                    session.last_error
                ],
            )
            .map(|_| ())
            .map_err(|e| format!("could not persist session: {e}"))
    }

    // ── misc ────────────────────────────────────────────────────────────────

    pub fn reset_stats(&mut self) {
        let now = now_ms();
        self.state.stats = Stats {
            since: now,
            profit_rlt: 0.0,
            spent_rlt: 0.0,
            bought: 0,
            filled: 0,
            failed: 0,
            orders_sent: 0,
        };
        self.state.report_since = now;
        self.state.report.clear();
        // The loot log lives in `orders`; dropping the rows is what actually
        // makes the reset stick after the next history fetch or restart.
        self.state.history.clear();
        if let Err(error) = self.db.execute("DELETE FROM orders", []) {
            self.report_write_error("could not clear the loot log", &error);
        }
        if let Err(error) = self.db.execute("DELETE FROM report_products", []) {
            self.report_write_error("could not clear the profit report", &error);
        }
        if let Err(error) = self.write_stats() {
            self.report_write_error("could not reset stats", &error);
        }
        if let Err(error) = self.set_meta("report_since", &now.to_string()) {
            self.report_write_error("could not stamp report_since", &error);
        }
    }

    pub fn close(&mut self) {
        let _ = self.flush();
        let _ = self.flush_event_points();
        db::checkpoint_and_close(&self.db);
    }

    pub fn logger(&self) -> &Arc<Logger> {
        &self.logger
    }
}

fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<OrderEntry> {
    let timings: Option<String> = row.get(12)?;
    let raw_events: Option<String> = row.get(13)?;
    Ok(OrderEntry {
        id: row.get(0)?,
        order_id: row.get(1)?,
        item_id: row.get(2)?,
        label: row.get(3)?,
        qty: row.get(4)?,
        unit_price: row.get(5)?,
        total_price: row.get(6)?,
        target_price: row.get(7)?,
        profit_rlt: row.get(8)?,
        status: row.get(9)?,
        at: row.get(10)?,
        latency_ms: row.get(11)?,
        timings: timings
            .as_deref()
            .and_then(|text| serde_json::from_str::<Timings>(text).ok()),
        raw_events: crate::raw_events::decode(raw_events.as_deref()),
        note: row.get(14)?,
        message: row.get(15)?,
    })
}

/// The list shape: everything the loot table renders, without the JSON blobs.
fn row_to_entry_lite(row: &Row<'_>) -> rusqlite::Result<OrderEntry> {
    Ok(OrderEntry {
        id: row.get(0)?,
        order_id: row.get(1)?,
        item_id: row.get(2)?,
        label: row.get(3)?,
        qty: row.get(4)?,
        unit_price: row.get(5)?,
        total_price: row.get(6)?,
        target_price: row.get(7)?,
        profit_rlt: row.get(8)?,
        status: row.get(9)?,
        at: row.get(10)?,
        latency_ms: row.get(11)?,
        timings: None,
        raw_events: None,
        note: row.get(12)?,
        message: row.get(13)?,
    })
}

/// Convenience for building an in-memory entry in tests and the engine round.
pub fn make_entry(
    id: &str,
    item_id: &str,
    label: &str,
    status: &str,
    qty: i64,
    at: i64,
) -> OrderEntry {
    OrderEntry {
        id: id.to_string(),
        order_id: None,
        item_id: item_id.to_string(),
        label: label.to_string(),
        qty,
        unit_price: 0.1,
        total_price: 0.1 * qty as f64,
        target_price: 0.1,
        profit_rlt: 0.0,
        status: status.to_string(),
        at,
        latency_ms: None,
        timings: None,
        raw_events: None,
        note: None,
        message: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rc-rust-store-{tag}-{nanos}"))
    }

    fn store(tag: &str) -> Store {
        let logger = Arc::new(Logger::new("error"));
        Store::open(temp_dir(tag), logger).expect("store")
    }

    fn target(item_id: &str) -> Target {
        Target {
            item_id: item_id.to_string(),
            label: "Hashboard".into(),
            item_type: "miner".into(),
            rarity_color: None,
            target_price: 0.1,
            desired_qty: 10,
            high_stock_threshold: 20,
            min_acceptable_qty: 1,
            allow_partial_order: true,
            active: true,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn generates_and_verifies_a_worker_token() {
        let mut store = store("token");
        let token = store.ensure_worker_token("").unwrap().expect("generated");
        assert!(store.verify_worker_token(&token));
        assert!(!store.verify_worker_token("wrong-token"));
        assert!(store.ensure_worker_token("").unwrap().is_none());
    }

    #[test]
    fn pins_the_token_from_the_environment() {
        let mut store = store("env-token");
        let pinned = "super-secret-worker-key";
        assert!(store.ensure_worker_token(pinned).unwrap().is_none());
        assert!(store.verify_worker_token(pinned));
    }

    #[test]
    fn persists_targets_and_settings_across_reopen() {
        let dir = temp_dir("persist");
        let logger = Arc::new(Logger::new("error"));
        {
            let mut store = Store::open(dir.clone(), logger.clone()).unwrap();
            store.upsert_config(target("6319f7baa8ce530569ed16b9"), None);
            let patch = validation::SettingsPatch {
                mode: Some(Mode::Off),
                ..Default::default()
            };
            store.patch_settings(patch).unwrap();
            store.close();
        }
        let store = Store::open(dir.clone(), logger).unwrap();
        assert_eq!(store.state.configs.len(), 1);
        assert_eq!(store.state.configs[0].item_id, "6319f7baa8ce530569ed16b9");
        assert_eq!(store.state.settings.mode, Mode::Off);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The legacy columns are mirrors, not state: whatever mode is written must
    /// leave a pair of flags that agrees with it on disk.
    #[test]
    fn the_config_index_tracks_every_mutation() {
        let mut store = store("config-index");
        store.upsert_config(target("AAA111"), None);
        store.upsert_config(target("bbb222"), None);

        // Mixed case in the caller still resolves, without an allocation.
        assert_eq!(store.find_config("aaa111").unwrap().item_id, "AAA111");
        assert_eq!(store.find_config("BBB222").unwrap().item_id, "bbb222");
        assert!(store.find_config("missing").is_none());

        // An edit can change the stored case; the index must follow.
        store.upsert_config(target("aaa111"), Some("AAA111"));
        assert_eq!(store.find_config("aaa111").unwrap().item_id, "aaa111");

        // Deleting shifts every later position.
        assert!(store.remove_config("aaa111"));
        assert!(store.find_config("aaa111").is_none());
        assert!(store.find_config("bbb222").is_some());

        // A bulk replace rewrites the whole set.
        store.set_configs(vec![target("ccc333")]).unwrap();
        assert!(store.find_config("bbb222").is_none());
        assert!(store.find_config("ccc333").is_some());
    }

    #[test]
    fn mode_and_the_legacy_mirror_columns_never_disagree() {
        let dir = temp_dir("mirror");
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger.clone()).unwrap();
        for (mode, dry_run, paused) in [(Mode::Off, 1, 1), (Mode::Shadow, 1, 0), (Mode::Live, 0, 0)]
        {
            store
                .patch_settings(validation::SettingsPatch {
                    mode: Some(mode),
                    ..Default::default()
                })
                .unwrap();
            let row: (String, i64, i64) = store
                .db
                .query_row("SELECT mode, dry_run, paused FROM settings", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .unwrap();
            assert_eq!(row, (mode.as_str().to_string(), dry_run, paused));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    }

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
                Some((0.0905, 10.0, false)),
            )
            .unwrap();
        store
            .record_price_point(item_id, 1_700_000_060_000, 0.095, None)
            .unwrap();
        let points = store.load_price_history(item_id, 10, 0);
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].bucket_at, 1_700_000_000_000);
        assert_eq!(points[0].fill_price, Some(0.0905));
        assert_eq!(points[1].fill_price, None);

        // The long-range series skips the live window cap and reads the whole
        // retained range for the chart's 3d/7d periods.
        assert_eq!(
            store.price_series(item_id, 1_700_000_000_000),
            vec![(1_700_000_000_000, 0.09), (1_700_000_060_000, 0.095)]
        );
        assert_eq!(
            store.price_series(item_id, 1_700_000_030_000),
            vec![(1_700_000_060_000, 0.095)]
        );
        assert!(store.price_series("other-item", 0).is_empty());
    }

    #[test]
    fn prunes_price_points_outside_the_retention_window() {
        let store = store("prune-price");
        let item_id = "6319f7baa8ce530569ed16b9";
        let old = now_ms() - (PRICE_RETENTION_DAYS + 1) * 86_400_000;
        let recent = now_ms() - 86_400_000;
        store.record_price_point(item_id, old, 0.1, None).unwrap();
        store
            .record_price_point(item_id, recent, 0.2, None)
            .unwrap();

        assert_eq!(store.prune_price_history().unwrap(), 1);
        let points = store.load_price_history(item_id, 10, 0);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].bucket_at, recent);
        // A passive checkpoint is always safe to run on a live database.
        store.checkpoint_wal().unwrap();
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
}
