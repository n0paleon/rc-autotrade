use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::constants::{APP_NAME, Limits, MAX_REPORT_PRODUCTS};
use crate::model::{OrderEntry, Settings, Stats, Target};
use crate::timings;

use super::derive::{aggregate_products, derive_stats};
use super::json::{
    non_negative, number, optional_text, positive_time, signed, text, time_or, warnings_finish,
    whole,
};
use super::{
    BACKUP_FORMAT_VERSION, BACKUP_KIND, BACKUP_MAX_ORDERS, ParseOutcome, ParsedBackup,
    ReportProduct,
};

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
        stock_at_trigger: entry
            .get("stockAtTrigger")
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

/// Validates a backup document, including the warning list.
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
            if let Some(value) = patch.total_price_markup_pct {
                settings.total_price_markup_pct = value;
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
    if let Some(orders) = &raw_orders
        && orders.len() > BACKUP_MAX_ORDERS
    {
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
