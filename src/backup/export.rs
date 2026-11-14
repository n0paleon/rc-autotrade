use serde_json::{Value, json};

use crate::constants::{APP_NAME, APP_VERSION};
use crate::model::{OrderEntry, Target};
use crate::store::Store;
use crate::timings;

use super::{BACKUP_FORMAT_VERSION, BACKUP_KIND};

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
        "stockAtTrigger": entry.stock_at_trigger,
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
                "SELECT id, order_id, item_id, label, qty, requested_qty, stock_at_trigger, unit_price, total_price,
                        target_price, profit_rlt, status, at, latency_ms, timings, raw_events,
                        note, message
                 FROM orders ORDER BY at ASC, rowid ASC",
            )
            .map_err(|error| format!("could not read orders: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                let timings_text: Option<String> = row.get(14)?;
                let raw_text: Option<String> = row.get(15)?;
                let status: String = row.get(11)?;
                let qty: i64 = row.get(4)?;
                // Legacy failed rows stored the attempted count in qty; the
                // fulfillment count is 0 there, so don't fall back to qty.
                let requested_qty = row.get::<_, Option<i64>>(5)?.or_else(|| {
                    if status == "failed" {
                        Some(0)
                    } else {
                        Some(qty)
                    }
                });
                Ok(OrderEntry {
                    id: row.get(0)?,
                    order_id: row.get(1)?,
                    item_id: row.get(2)?,
                    label: row.get(3)?,
                    qty,
                    requested_qty,
                    stock_at_trigger: row.get::<_, Option<i64>>(6)?,
                    unit_price: row.get(7)?,
                    total_price: row.get(8)?,
                    target_price: row.get(9)?,
                    profit_rlt: row.get(10)?,
                    status: row.get(11)?,
                    at: row.get(12)?,
                    latency_ms: row.get(13)?,
                    timings: timings::decode(timings_text.as_deref()),
                    raw_events: crate::raw_events::decode(raw_text.as_deref()),
                    note: row.get(16)?,
                    message: row.get(17)?,
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
