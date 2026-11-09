//! Portable backups, ported from `src/worker/backup.js`.
//!
//! Two formats, both interchangeable with the Bun worker:
//! * a versioned JSON document (`kind: "turu-rc-backup"`, format v1) holding
//!   settings, targets, stats, the profit report, the loot log and — optionally
//!   — the session credentials;
//! * a consistent SQLite copy written with `VACUUM INTO` (safe while the
//!   worker is running, unlike copying the `.db` file).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rusqlite::params;
use serde_json::{Value, json};

use crate::constants::{APP_NAME, APP_VERSION, Limits, MAX_REPORT_PRODUCTS};
use crate::model::{OrderEntry, Product, Settings, Stats, Target};
use crate::rules::round_price;
use crate::store::Store;
use crate::timings;

pub const BACKUP_KIND: &str = "turu-rc-backup";
pub const BACKUP_FORMAT_VERSION: i64 = 1;
pub const BACKUP_MAX_BYTES: usize = 64 * 1024 * 1024;
pub const BACKUP_MAX_ORDERS: usize = 250_000;
pub const BACKUP_DIRNAME: &str = "backups";
const MAX_WARNINGS: usize = 20;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn round(value: f64, digits: i32) -> f64 {
    let factor = 10f64.powi(digits);
    (value * factor).round() / factor
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn text(value: Option<&Value>, max: usize) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(max)
        .collect()
}

fn optional_text(value: Option<&Value>, max: usize) -> Option<String> {
    let text = value.and_then(Value::as_str)?;
    if text.is_empty() {
        None
    } else {
        Some(text.chars().take(max).collect())
    }
}

fn positive_time(value: Option<&Value>) -> Option<i64> {
    let value = number(value)?;
    (value.is_finite() && value > 0.0).then_some(value as i64)
}

fn time_or(value: Option<&Value>, fallback: i64) -> i64 {
    positive_time(value).unwrap_or(fallback)
}

fn whole(value: Option<&Value>, fallback: i64, max: i64) -> i64 {
    match number(value) {
        Some(value) if value.is_finite() => (value.trunc() as i64).clamp(0, max),
        _ => fallback,
    }
}

fn signed(value: Option<&Value>, fallback: f64) -> f64 {
    number(value)
        .filter(|value| value.is_finite())
        .unwrap_or(fallback)
}

fn non_negative(value: Option<&Value>, fallback: f64) -> f64 {
    signed(value, fallback).max(0.0)
}

/// Compact ISO timestamp used in filenames: `20261106T224502Z`.
pub fn timestamp(now: i64) -> String {
    let seconds = now.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's civil-from-days algorithm (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn backup_filename(now: i64) -> String {
    format!("turu-rc-backup-{}.json", timestamp(now))
}

pub fn database_filename(now: i64) -> String {
    format!("turu-rc-db-{}.db", timestamp(now))
}

fn safe_prefix(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "snapshot".to_string()
    } else {
        trimmed
    }
}

fn warnings_finish(warnings: &mut Vec<String>) {
    while warnings.len() > MAX_WARNINGS {
        warnings.pop();
    }
}

// ── export ─────────────────────────────────────────────────────────────────

fn target_json(target: &Target) -> Value {
    let mut value = json!({
        "itemId": target.item_id,
        "label": target.label,
        "itemType": target.item_type,
        "targetPrice": target.target_price,
        "desiredQty": target.desired_qty,
        "highStockThreshold": target.high_stock_threshold,
        "minAcceptableQty": target.min_acceptable_qty,
        "allowPartialOrder": target.allow_partial_order,
        "active": target.active,
        "createdAt": target.created_at,
        "updatedAt": target.updated_at,
    });
    if let Some(color) = &target.rarity_color {
        value["rarityColor"] = json!(color);
    }
    value
}

fn order_json(entry: &OrderEntry) -> Value {
    json!({
        "id": entry.id,
        "orderId": entry.order_id,
        "itemId": entry.item_id,
        "label": entry.label,
        "qty": entry.qty,
        "requestedQty": entry.requested_qty,
        "unitPrice": entry.unit_price,
        "totalPrice": entry.total_price,
        "targetPrice": entry.target_price,
        "profitRlt": entry.profit_rlt,
        "status": entry.status,
        "at": entry.at,
        "latencyMs": entry.latency_ms,
        "timings": entry.timings,
        "rawEvents": entry.raw_events,
        "note": entry.note,
        "message": entry.message,
    })
}

/// Reads every portable row and wraps it in the versioned backup envelope.
pub fn build_backup(store: &Store, include_credentials: bool, now: i64) -> Result<Value, String> {
    let targets: Vec<Value> = store.state.configs.iter().map(target_json).collect();

    let orders: Vec<Value> = {
        let mut statement = store
            .db
            .prepare(
                "SELECT id, order_id, item_id, label, qty, requested_qty, unit_price, total_price,
                        target_price, profit_rlt, status, at, latency_ms, timings, raw_events,
                        note, message
                 FROM orders ORDER BY at ASC, rowid ASC",
            )
            .map_err(|error| format!("could not read orders: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                let timings_text: Option<String> = row.get(13)?;
                let raw_text: Option<String> = row.get(14)?;
                Ok(OrderEntry {
                    id: row.get(0)?,
                    order_id: row.get(1)?,
                    item_id: row.get(2)?,
                    label: row.get(3)?,
                    qty: row.get(4)?,
                    requested_qty: row.get::<_, Option<i64>>(5)?.or(Some(row.get::<_, i64>(4)?)),
                    unit_price: row.get(6)?,
                    total_price: row.get(7)?,
                    target_price: row.get(8)?,
                    profit_rlt: row.get(9)?,
                    status: row.get(10)?,
                    at: row.get(11)?,
                    latency_ms: row.get(12)?,
                    timings: timings::decode(timings_text.as_deref()),
                    raw_events: crate::raw_events::decode(raw_text.as_deref()),
                    note: row.get(15)?,
                    message: row.get(16)?,
                })
            })
            .map_err(|error| format!("could not read orders: {error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("could not decode orders: {error}"))?
            .iter()
            .map(order_json)
            .collect()
    };

    let report_products: Vec<Value> = store
        .state
        .report
        .iter()
        .map(|(key, product)| {
            json!({
                "productKey": key,
                "itemId": product.item_id,
                "label": product.label,
                "fills": product.fills,
                "failed": product.failed,
                "qty": product.qty,
                "spentRlt": product.spent_rlt,
                "profitRlt": product.profit_rlt,
                "firstAt": product.first_at,
                "lastAt": product.last_at,
            })
        })
        .collect();

    let stats = store.state.stats;
    let created_at: i64 = store.state.created_at;
    let report_since = store.state.report_since.max(created_at);

    let mut payload = json!({
        "kind": BACKUP_KIND,
        "formatVersion": BACKUP_FORMAT_VERSION,
        "schemaVersion": crate::db::SCHEMA_VERSION,
        "app": { "name": APP_NAME, "version": APP_VERSION },
        "exportedAt": now,
        "counts": {
            "targets": targets.len(),
            "orders": orders.len(),
            "reportProducts": report_products.len(),
        },
        "data": {
            "createdAt": created_at,
            "reportSince": report_since,
            "settings": store.state.settings,
            "targets": targets,
            "stats": stats,
            "reportProducts": report_products,
            "orders": orders,
        },
    });

    if include_credentials {
        let session = &store.state.session;
        payload["credentials"] = json!({
            "session": {
                "accessToken": session.access_token,
                "refreshToken": session.refresh_token,
                "cookie": session.cookie,
                "source": session.source,
                "updatedAt": session.updated_at,
                "lastRefreshAt": session.last_refresh_at,
                "refreshCount": session.refresh_count,
                "lastError": session.last_error,
            }
        });
    }

    Ok(payload)
}

// ── validation ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ReportProduct {
    pub product_key: String,
    pub item_id: String,
    pub label: String,
    pub fills: i64,
    pub failed: i64,
    pub qty: i64,
    pub spent_rlt: f64,
    pub profit_rlt: f64,
    pub first_at: i64,
    pub last_at: i64,
}

impl From<ReportProduct> for Product {
    fn from(value: ReportProduct) -> Self {
        Self {
            item_id: value.item_id,
            label: value.label,
            fills: value.fills,
            failed: value.failed,
            qty: value.qty,
            spent_rlt: value.spent_rlt,
            profit_rlt: value.profit_rlt,
            first_at: value.first_at,
            last_at: value.last_at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedBackup {
    pub format_version: i64,
    pub schema_version: i64,
    pub exported_at: i64,
    pub created_at: i64,
    pub report_since: i64,
    pub settings: Settings,
    pub targets: Vec<Target>,
    pub stats: Stats,
    pub report_products: Vec<ReportProduct>,
    pub orders: Vec<OrderEntry>,
    pub credentials: Option<crate::model::Session>,
}

#[derive(Debug, Clone)]
pub struct ParseOutcome {
    pub ok: bool,
    pub errors: BTreeMap<String, String>,
    pub warnings: Vec<String>,
    pub value: Option<ParsedBackup>,
}

fn normalize_order(entry: &Value, warnings: &mut Vec<String>, index: usize) -> Option<OrderEntry> {
    if !entry.is_object() {
        warnings.push(format!(
            "Skipped loot entry #{}: it is not an object.",
            index + 1
        ));
        return None;
    }
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if id.is_empty() || id.chars().count() > 200 {
        warnings.push(format!(
            "Skipped loot entry #{}: it has no usable id.",
            index + 1
        ));
        return None;
    }
    let Some(at) = positive_time(entry.get("at")) else {
        warnings.push(format!(
            "Skipped loot entry {id}: its timestamp is invalid."
        ));
        return None;
    };

    let status = if entry.get("status").and_then(Value::as_str) == Some("failed") {
        "failed".to_string()
    } else {
        "filled".to_string()
    };
    let note = optional_text(entry.get("note"), 160);
    let mut message = optional_text(entry.get("message"), 160);
    if message.as_deref().unwrap_or("").trim().is_empty() {
        message = Some(if status == "failed" {
            note.clone().unwrap_or_else(|| "failed".to_string())
        } else {
            "-".to_string()
        });
    }

    Some(OrderEntry {
        id: id.clone(),
        order_id: optional_text(entry.get("orderId"), 200),
        item_id: text(entry.get("itemId"), 64),
        label: text(entry.get("label"), 48),
        qty: whole(entry.get("qty"), 0, 1_000_000_000),
        requested_qty: entry
            .get("requestedQty")
            .filter(|value| !value.is_null())
            .map(|value| whole(Some(value), 0, 1_000_000_000)),
        unit_price: non_negative(entry.get("unitPrice"), 0.0),
        total_price: non_negative(entry.get("totalPrice"), 0.0),
        target_price: non_negative(entry.get("targetPrice"), 0.0),
        profit_rlt: signed(entry.get("profitRlt"), 0.0),
        status,
        at,
        latency_ms: entry
            .get("latencyMs")
            .filter(|value| !value.is_null())
            .map(|value| whole(Some(value), 0, 86_400_000)),
        timings: entry.get("timings").and_then(timings::sanitize),
        raw_events: entry.get("rawEvents").and_then(crate::raw_events::sanitize),
        note,
        message,
    })
}

fn derive_stats(orders: &[OrderEntry], now: i64) -> Stats {
    let mut since = now;
    let mut spent = 0.0;
    let mut profit = 0.0;
    let mut bought = 0i64;
    let mut filled = 0i64;
    for order in orders {
        if order.status == "filled" {
            filled += 1;
            bought += order.qty;
            spent += order.total_price;
            profit += order.profit_rlt;
        }
        if order.at > 0 && order.at < since {
            since = order.at;
        }
    }
    Stats {
        since,
        profit_rlt: round_price(profit),
        spent_rlt: round_price(spent),
        bought,
        filled,
        failed: orders.len() as i64 - filled,
        orders_sent: orders.len() as i64,
    }
}

fn normalize_product(
    entry: &Value,
    warnings: &mut Vec<String>,
    index: usize,
    now: i64,
) -> Option<ReportProduct> {
    if !entry.is_object() {
        warnings.push(format!(
            "Skipped profit row #{}: it is not an object.",
            index + 1
        ));
        return None;
    }
    let item_id = text(entry.get("itemId"), 64);
    let label = text(entry.get("label"), 48);
    let product_key = entry
        .get("productKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let id = item_id.trim().to_ascii_lowercase();
            if !id.is_empty() {
                id
            } else {
                let name = label.trim().to_ascii_lowercase();
                if name.is_empty() {
                    "unknown".to_string()
                } else {
                    format!("label:{name}")
                }
            }
        });
    Some(ReportProduct {
        product_key,
        item_id,
        label,
        fills: whole(entry.get("fills"), 0, 1_000_000_000),
        failed: whole(entry.get("failed"), 0, 1_000_000_000),
        qty: whole(entry.get("qty"), 0, 1_000_000_000),
        spent_rlt: non_negative(entry.get("spentRlt"), 0.0),
        profit_rlt: signed(entry.get("profitRlt"), 0.0),
        first_at: time_or(entry.get("firstAt"), now),
        last_at: time_or(entry.get("lastAt"), now),
    })
}

fn aggregate_products(orders: &[OrderEntry]) -> Vec<ReportProduct> {
    let mut products: BTreeMap<String, ReportProduct> = BTreeMap::new();
    for order in orders {
        let key = {
            let id = order.item_id.trim().to_ascii_lowercase();
            if !id.is_empty() {
                id
            } else {
                let name = order.label.trim().to_ascii_lowercase();
                if name.is_empty() {
                    "unknown".to_string()
                } else {
                    format!("label:{name}")
                }
            }
        };
        let entry = products.entry(key.clone()).or_insert(ReportProduct {
            product_key: key,
            item_id: order.item_id.clone(),
            label: order.label.chars().take(48).collect(),
            fills: 0,
            failed: 0,
            qty: 0,
            spent_rlt: 0.0,
            profit_rlt: 0.0,
            first_at: order.at,
            last_at: order.at,
        });
        entry.first_at = entry.first_at.min(order.at);
        entry.last_at = entry.last_at.max(order.at);
        if order.status == "failed" {
            entry.failed += 1;
        } else {
            entry.fills += 1;
            entry.qty += order.qty.max(0);
            entry.spent_rlt = round_price(entry.spent_rlt + order.total_price.max(0.0));
            entry.profit_rlt = round_price(entry.profit_rlt + order.profit_rlt);
        }
    }
    products.into_values().collect()
}

/// Validates a backup document. Mirrors `parseBackup`, including the warning list.
pub fn parse_backup(raw: &Value, now: i64) -> ParseOutcome {
    let mut warnings: Vec<String> = Vec::new();
    let mut errors: BTreeMap<String, String> = BTreeMap::new();

    let Some(object) = raw.as_object() else {
        errors.insert(
            "file".into(),
            "The uploaded value is not a JSON object.".into(),
        );
        return ParseOutcome {
            ok: false,
            errors,
            warnings,
            value: None,
        };
    };

    if object.get("kind").and_then(Value::as_str) != Some(BACKUP_KIND) {
        errors.insert(
            "kind".into(),
            format!("This is not a {APP_NAME} backup (expected kind \"{BACKUP_KIND}\")."),
        );
    }

    let format_version = number(object.get("formatVersion")).unwrap_or(0.0);
    if format_version < 1.0 || format_version.fract() != 0.0 {
        errors.insert(
            "formatVersion".into(),
            "The backup format version is missing or invalid.".into(),
        );
    } else if format_version as i64 > BACKUP_FORMAT_VERSION {
        errors.insert(
            "formatVersion".into(),
            format!(
                "Backup format v{} is newer than this build supports (v{BACKUP_FORMAT_VERSION}). Update the worker first.",
                format_version as i64
            ),
        );
    }

    let schema_version = number(object.get("schemaVersion")).unwrap_or(0.0);
    if schema_version < 1.0 || schema_version.fract() != 0.0 {
        errors.insert(
            "schemaVersion".into(),
            "The database schema version is missing or invalid.".into(),
        );
    } else if schema_version as i64 > crate::db::SCHEMA_VERSION {
        errors.insert(
            "schemaVersion".into(),
            format!(
                "The backup was written with schema v{}, but this build only understands up to v{}. Upgrade the worker instead of downgrading the data.",
                schema_version as i64,
                crate::db::SCHEMA_VERSION
            ),
        );
    }

    let data = object.get("data").and_then(Value::as_object);
    if data.is_none() {
        errors.insert("data".into(), "The backup has no data block.".into());
    }
    if !errors.is_empty() {
        return ParseOutcome {
            ok: false,
            errors,
            warnings,
            value: None,
        };
    }
    let data = data.expect("checked above");

    // Settings: merge over defaults, warn when anything was invalid.
    let settings_input = data
        .get("settings")
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !data.get("settings").map(Value::is_object).unwrap_or(false) {
        warnings.push("The backup has no settings block; worker defaults were used.".into());
    }
    let mut settings = Settings::default();
    match crate::validation::validate_settings(&settings_input) {
        Ok(patch) => {
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
            if let Some(value) = patch.max_fill_retries {
                settings.max_fill_retries = value;
            }
            if let Some(value) = patch.retry_delay_ms {
                settings.retry_delay_ms = value;
            }
        }
        Err(_) => {
            warnings
                .push("Some settings were invalid; valid values and defaults were used.".into());
        }
    }

    let mut raw_targets = data
        .get("targets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !data.get("targets").map(Value::is_array).unwrap_or(false) {
        warnings.push(
            "The backup has no target list; this worker will start with zero targets.".into(),
        );
    }
    if raw_targets.len() > Limits::CONFIGS_MAX {
        warnings.push(format!(
            "The backup has {} targets, more than the {} allowed; only the first {} were restored.",
            raw_targets.len(),
            Limits::CONFIGS_MAX,
            Limits::CONFIGS_MAX
        ));
        raw_targets.truncate(Limits::CONFIGS_MAX);
    }

    let mut targets: Vec<Target> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (index, entry) in raw_targets.iter().enumerate() {
        match crate::validation::validate_target(entry, &[], None) {
            Ok(mut target) => {
                let key = target.item_id.to_ascii_lowercase();
                if !seen.insert(key) {
                    warnings.push(format!("Skipped duplicate target {}.", target.item_id));
                    continue;
                }
                target.created_at = Some(time_or(entry.get("createdAt"), now));
                target.updated_at = Some(time_or(entry.get("updatedAt"), now));
                targets.push(target);
            }
            Err(fields) => {
                let first = fields
                    .values()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "invalid value".to_string());
                warnings.push(format!("Skipped target #{}: {first}", index + 1));
            }
        }
    }

    let raw_orders = data.get("orders").and_then(Value::as_array).cloned();
    if !data.get("orders").map(Value::is_array).unwrap_or(false) {
        warnings.push("The backup has no loot log; the permanent log will be empty.".into());
    }
    if let Some(orders) = &raw_orders {
        if orders.len() > BACKUP_MAX_ORDERS {
            errors.insert(
                "orders".into(),
                format!(
                    "The backup contains {} loot entries, more than the {} supported per import.",
                    orders.len(),
                    BACKUP_MAX_ORDERS
                ),
            );
            warnings_finish(&mut warnings);
            return ParseOutcome {
                ok: false,
                errors,
                warnings,
                value: None,
            };
        }
    }

    let mut orders: Vec<OrderEntry> = Vec::new();
    let mut order_ids: BTreeSet<String> = BTreeSet::new();
    for (index, entry) in raw_orders.unwrap_or_default().iter().enumerate() {
        let Some(order) = normalize_order(entry, &mut warnings, index) else {
            continue;
        };
        if !order_ids.insert(order.id.to_ascii_lowercase()) {
            warnings.push(format!("Skipped duplicate loot entry {}.", order.id));
            continue;
        }
        orders.push(order);
    }

    let derived = derive_stats(&orders, now);
    let stats = match data.get("stats").filter(|value| value.is_object()) {
        Some(stats) => Stats {
            since: time_or(stats.get("since"), derived.since),
            profit_rlt: signed(stats.get("profitRlt"), derived.profit_rlt),
            spent_rlt: non_negative(stats.get("spentRlt"), derived.spent_rlt),
            bought: whole(stats.get("bought"), derived.bought, 1_000_000_000),
            filled: whole(stats.get("filled"), derived.filled, 1_000_000_000),
            failed: whole(stats.get("failed"), derived.failed, 1_000_000_000),
            orders_sent: whole(stats.get("ordersSent"), derived.orders_sent, 1_000_000_000),
        },
        None => {
            warnings.push(
                "The backup has no stats block; totals were derived from the loot log.".into(),
            );
            derived
        }
    };

    let raw_products = data
        .get("reportProducts")
        .and_then(Value::as_array)
        .cloned();
    if raw_products.is_none() {
        warnings.push("The backup has no profit report block.".into());
    }
    let mut products: Vec<ReportProduct> = Vec::new();
    let mut product_keys: BTreeSet<String> = BTreeSet::new();
    for (index, entry) in raw_products.unwrap_or_default().iter().enumerate() {
        let Some(product) = normalize_product(entry, &mut warnings, index, now) else {
            continue;
        };
        if !product_keys.insert(product.product_key.clone()) {
            warnings.push(format!(
                "Skipped duplicate profit row {}.",
                product.product_key
            ));
            continue;
        }
        products.push(product);
    }
    if products.len() > MAX_REPORT_PRODUCTS {
        warnings.push(format!(
            "The backup has {} profit rows; only the {MAX_REPORT_PRODUCTS} most profitable were restored.",
            products.len()
        ));
        products.sort_by(|a, b| {
            b.profit_rlt
                .partial_cmp(&a.profit_rlt)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        products.truncate(MAX_REPORT_PRODUCTS);
    }
    if products.is_empty() && !orders.is_empty() {
        products = aggregate_products(&orders);
        warnings.push("The profit report was rebuilt from the loot log.".into());
    }

    let mut credentials = None;
    if let Some(session) = object
        .get("credentials")
        .and_then(|value| value.get("session"))
        .filter(|value| value.is_object())
    {
        let session = crate::model::Session {
            access_token: text(session.get("accessToken"), 4096),
            refresh_token: text(session.get("refreshToken"), 4096),
            cookie: text(session.get("cookie"), 8192),
            source: optional_text(session.get("source"), 40),
            updated_at: positive_time(session.get("updatedAt")),
            last_refresh_at: positive_time(session.get("lastRefreshAt")),
            refresh_count: whole(session.get("refreshCount"), 0, 1_000_000_000),
            last_error: optional_text(session.get("lastError"), 240),
        };
        if session.access_token.is_empty() && session.refresh_token.is_empty() {
            warnings.push(
                "The credentials block has no usable session; the current session will be kept."
                    .into(),
            );
        } else {
            if !session.access_token.is_empty() && session.access_token.len() < 8 {
                warnings.push(
                    "The stored access token looks unusually short; Rollercoin may reject it."
                        .into(),
                );
            }
            credentials = Some(session);
        }
    }

    let exported_at = time_or(object.get("exportedAt"), now);
    let created_at = time_or(data.get("createdAt"), now);
    warnings_finish(&mut warnings);

    ParseOutcome {
        ok: true,
        errors: BTreeMap::new(),
        warnings,
        value: Some(ParsedBackup {
            format_version: format_version as i64,
            schema_version: schema_version as i64,
            exported_at,
            created_at,
            report_since: time_or(data.get("reportSince"), stats.since),
            settings,
            targets,
            stats,
            report_products: products,
            orders,
            credentials,
        }),
    }
}

// ── snapshot + restore ─────────────────────────────────────────────────────

/// Writes a consistent copy of the live database with `VACUUM INTO`.
pub fn create_database_snapshot(store: &Store, prefix: &str, now: i64) -> Result<PathBuf, String> {
    let dir = store.data_dir.join(BACKUP_DIRNAME);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    let base = format!("{}-{}", safe_prefix(prefix), timestamp(now));
    let mut file = dir.join(format!("{base}.db"));
    for index in 2.. {
        if !file.exists() {
            break;
        }
        file = dir.join(format!("{base}-{index}.db"));
    }

    store
        .db
        .execute("VACUUM INTO ?1", params![file.to_string_lossy()])
        .map_err(|error| format!("Could not write the database snapshot: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

#[derive(Debug, Clone)]
pub struct RestoreResult {
    pub targets: i64,
    pub orders: i64,
    pub report_products: i64,
    pub credentials_restored: bool,
    pub safety_backup: Option<String>,
    pub safety_backup_path: Option<PathBuf>,
}

/// Replaces the portable tables with a validated backup inside one transaction.
pub fn restore_backup(
    store: &mut Store,
    backup: &ParsedBackup,
    restore_credentials: bool,
    snapshot: bool,
    now: i64,
) -> Result<RestoreResult, String> {
    let safety = if snapshot {
        Some(create_database_snapshot(store, "pre-import", now)?)
    } else {
        None
    };

    let settings = backup.settings;
    let stats = backup.stats;
    let targets = backup.targets.clone();
    let orders = backup.orders.clone();
    let products = backup.report_products.clone();
    let session = if restore_credentials {
        backup.credentials.clone()
    } else {
        None
    };

    let tx = store
        .db
        .unchecked_transaction()
        .map_err(|error| format!("could not start restore transaction: {error}"))?;

    for table in [
        "market_snapshot",
        "price_points",
        "orders",
        "report_products",
        "targets",
    ] {
        tx.execute(&format!("DELETE FROM {table}"), [])
            .map_err(|error| format!("could not clear {table}: {error}"))?;
    }

    let (dry_run, paused) = settings.legacy_flags();
    tx.execute(
        "UPDATE settings SET delay_per_order = ?1, max_orders_per_minute = ?2,
                dry_run = ?3, auto_refresh = ?4, paused = ?5, mode = ?6, max_fill_retries = ?7, retry_delay_ms = ?8 WHERE id = 1",
        params![
            settings.delay_per_order,
            settings.max_orders_per_minute,
            dry_run as i64,
            settings.auto_refresh as i64,
            paused as i64,
            settings.mode.as_str(),
            settings.max_fill_retries,
            settings.retry_delay_ms
        ],
    )
    .map_err(|error| format!("could not restore settings: {error}"))?;

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
    .map_err(|error| format!("could not restore stats: {error}"))?;

    for target in &targets {
        tx.execute(
            "INSERT INTO targets
             (item_id, label, item_type, rarity_color, target_price, desired_qty,
              high_stock_threshold, min_acceptable_qty, allow_partial_order, active,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                target.item_id,
                target.label,
                target.item_type,
                target.rarity_color,
                target.target_price,
                target.desired_qty,
                target.high_stock_threshold,
                target.min_acceptable_qty,
                target.allow_partial_order as i64,
                target.active as i64,
                target.created_at,
                target.updated_at
            ],
        )
        .map_err(|error| format!("could not restore target: {error}"))?;
    }

    for order in &orders {
        tx.execute(
            "INSERT OR REPLACE INTO orders
             (id, order_id, item_id, label, qty, requested_qty, unit_price, total_price, target_price,
              profit_rlt, status, at, latency_ms, timings, raw_events, note, message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                order.id,
                order.order_id,
                order.item_id,
                order.label,
                order.qty,
                order.requested_qty,
                order.unit_price,
                order.total_price,
                order.target_price,
                order.profit_rlt,
                order.status,
                order.at,
                order.latency_ms,
                order
                    .timings
                    .as_ref()
                    .and_then(|timings| serde_json::to_string(timings).ok()),
                order
                    .raw_events
                    .as_ref()
                    .and_then(crate::raw_events::encode),
                order.note,
                order.message
            ],
        )
        .map_err(|error| format!("could not restore loot entry: {error}"))?;
    }

    for product in &products {
        tx.execute(
            "INSERT INTO report_products
             (product_key, item_id, label, fills, failed, qty, spent_rlt, profit_rlt,
              first_at, last_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                product.product_key,
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
        )
        .map_err(|error| format!("could not restore profit row: {error}"))?;
    }

    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('created_at', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![backup.created_at.to_string()],
    )
    .map_err(|error| format!("could not restore meta: {error}"))?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('report_since', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![backup.report_since.to_string()],
    )
    .map_err(|error| format!("could not restore meta: {error}"))?;

    if let Some(session) = &session {
        tx.execute(
            "UPDATE session SET access_token = ?1, refresh_token = ?2, cookie = ?3,
                    source = ?4, updated_at = ?5, last_refresh_at = ?6,
                    refresh_count = ?7, last_error = ?8 WHERE id = 1",
            params![
                session.access_token,
                session.refresh_token,
                session.cookie,
                session
                    .source
                    .clone()
                    .unwrap_or_else(|| "backup".to_string()),
                session.updated_at.unwrap_or(now),
                session.last_refresh_at,
                session.refresh_count,
                session.last_error
            ],
        )
        .map_err(|error| format!("could not restore session: {error}"))?;
    }

    tx.commit()
        .map_err(|error| format!("could not commit restore: {error}"))?;

    store.reload()?;

    let safety_backup = safety.as_ref().map(|path| {
        path.strip_prefix(&store.data_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    });

    Ok(RestoreResult {
        targets: targets.len() as i64,
        orders: orders.len() as i64,
        report_products: products.len() as i64,
        credentials_restored: session.is_some(),
        safety_backup,
        safety_backup_path: safety,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger::Logger;
    use crate::model::Target;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    const NOW: i64 = 1_777_982_400_000; // 2026-05-05T12:00:00Z

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rc-rust-backup-{tag}-{nanos}"))
    }

    fn store(tag: &str) -> (Store, PathBuf) {
        let dir = temp_dir(tag);
        let store = Store::open(dir.clone(), Arc::new(Logger::new("error"))).unwrap();
        (store, dir)
    }

    fn target(item_id: &str) -> Target {
        Target {
            item_id: item_id.into(),
            label: "Hashboard".into(),
            item_type: "miner".into(),
            rarity_color: Some("8a2be2".into()),
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
    fn formats_filenames_like_the_javascript_worker() {
        assert_eq!(timestamp(NOW), "20260505T120000Z");
        assert_eq!(backup_filename(NOW), "turu-rc-backup-20260505T120000Z.json");
        assert_eq!(database_filename(NOW), "turu-rc-db-20260505T120000Z.db");
        assert_eq!(safe_prefix("pre-import"), "pre-import");
        assert_eq!(safe_prefix("  weird/name  "), "weird-name");
        assert_eq!(safe_prefix(""), "snapshot");
    }

    #[test]
    fn round_trips_a_backup_into_a_fresh_store() {
        let (mut source, source_dir) = store("roundtrip-src");
        source.upsert_config(target("6319f7baa8ce530569ed16b9"), None);
        source.flush().unwrap();
        let mut entry = crate::store::make_entry(
            "order-1",
            "6319f7baa8ce530569ed16b9",
            "Hashboard",
            "filled",
            10,
            NOW,
        );
        entry.total_price = 0.9;
        entry.profit_rlt = 0.1;
        source.add_history(entry).unwrap();
        // The engine updates stats before recording the loot row.
        source.state.stats.filled = 1;
        source.state.stats.bought = 10;
        source.state.stats.spent_rlt = 0.9;
        source.state.stats.profit_rlt = 0.1;
        source.state.stats.orders_sent = 1;
        source.flush().unwrap();
        source
            .apply_session_patch(crate::validation::SessionPatch {
                access_token: Some("access-token-123456".into()),
                refresh_token: Some("refresh-token-123456".into()),
                ..Default::default()
            })
            .unwrap();

        let payload = build_backup(&source, true, NOW).unwrap();
        assert_eq!(payload["kind"], BACKUP_KIND);
        assert_eq!(payload["counts"]["targets"], 1);
        assert_eq!(payload["counts"]["orders"], 1);
        assert_eq!(payload["counts"]["reportProducts"], 1);
        assert_eq!(
            payload["credentials"]["session"]["accessToken"],
            "access-token-123456"
        );

        let parsed = parse_backup(&payload, NOW);
        assert!(parsed.ok, "parse errors: {:?}", parsed.errors);
        let value = parsed.value.expect("parsed value");

        let (mut destination, destination_dir) = store("roundtrip-dst");
        let result = restore_backup(&mut destination, &value, true, false, NOW).unwrap();
        assert_eq!(result.targets, 1);
        assert_eq!(result.orders, 1);
        assert_eq!(result.report_products, 1);
        assert!(result.credentials_restored);

        assert_eq!(destination.state.configs.len(), 1);
        assert_eq!(
            destination.state.configs[0].rarity_color.as_deref(),
            Some("8a2be2")
        );
        assert!(destination.has_order("order-1"));
        assert_eq!(
            destination.state.session.access_token,
            "access-token-123456"
        );
        assert_eq!(destination.state.stats.filled, 1);
        assert!(
            destination
                .state
                .report
                .contains_key("6319f7baa8ce530569ed16b9")
        );

        let _ = std::fs::remove_dir_all(source_dir);
        let _ = std::fs::remove_dir_all(destination_dir);
    }

    #[test]
    fn rejects_a_document_that_is_not_a_backup() {
        let parsed = parse_backup(&json!({ "kind": "something-else" }), NOW);
        assert!(!parsed.ok);
        assert!(parsed.errors.contains_key("kind"));
        assert!(parsed.value.is_none());
    }

    #[test]
    fn rejects_a_newer_format_or_schema() {
        let mut payload = json!({
            "kind": BACKUP_KIND,
            "formatVersion": BACKUP_FORMAT_VERSION + 1,
            "schemaVersion": crate::db::SCHEMA_VERSION,
            "data": {},
        });
        let parsed = parse_backup(&payload, NOW);
        assert!(!parsed.ok);
        assert!(parsed.errors["formatVersion"].contains("newer"));

        payload["formatVersion"] = json!(BACKUP_FORMAT_VERSION);
        payload["schemaVersion"] = json!(crate::db::SCHEMA_VERSION + 1);
        let parsed = parse_backup(&payload, NOW);
        assert!(!parsed.ok);
        assert!(parsed.errors["schemaVersion"].contains("Upgrade the worker"));
    }

    #[test]
    fn skips_invalid_targets_and_rebuilds_the_report_from_orders() {
        let payload = json!({
            "kind": BACKUP_KIND,
            "formatVersion": 1,
            "schemaVersion": crate::db::SCHEMA_VERSION,
            "exportedAt": NOW,
            "data": {
                "createdAt": NOW,
                "reportSince": NOW,
                "settings": { "delayPerOrder": 250 },
                "stats": { "since": NOW },
                "targets": [
                    {
                        "itemId": "6319f7baa8ce530569ed16b9",
                        "targetPrice": 0.1,
                        "desiredQty": 10,
                        "highStockThreshold": 20,
                        "minAcceptableQty": 1
                    },
                    { "itemId": "bad", "targetPrice": 0 }
                ],
                "orders": [
                    {
                        "id": "o-1",
                        "itemId": "6319f7baa8ce530569ed16b9",
                        "label": "Hashboard",
                        "qty": 2,
                        "totalPrice": 0.2,
                        "profitRlt": 0.05,
                        "status": "filled",
                        "at": NOW
                    }
                ],
                "reportProducts": []
            }
        });

        let parsed = parse_backup(&payload, NOW);
        assert!(parsed.ok, "errors: {:?}", parsed.errors);
        assert!(
            parsed
                .warnings
                .iter()
                .any(|warning| warning.contains("Skipped target")),
            "warnings: {:?}",
            parsed.warnings
        );
        let value = parsed.value.unwrap();
        assert_eq!(value.targets.len(), 1);
        assert_eq!(value.settings.delay_per_order, 250);
        assert_eq!(value.report_products.len(), 1);
        assert_eq!(value.report_products[0].fills, 1);
        assert_eq!(value.report_products[0].qty, 2);
        assert!(
            parsed
                .warnings
                .iter()
                .any(|warning| warning.contains("rebuilt from the loot log"))
        );
    }

    #[test]
    fn creates_a_consistent_sqlite_snapshot() {
        let (store, dir) = store("snapshot");
        let path = create_database_snapshot(&store, "pre-import", NOW).unwrap();
        assert!(path.exists());
        let bytes = std::fs::metadata(&path).unwrap().len();
        assert!(bytes > 0);
        assert!(path.to_string_lossy().contains("pre-import-"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
