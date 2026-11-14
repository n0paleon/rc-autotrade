use rusqlite::Row;

use crate::model::OrderEntry;
use crate::timings::Timings;

pub(super) fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<OrderEntry> {
    let timings: Option<String> = row.get(14)?;
    let raw_events: Option<String> = row.get(15)?;
    let status: String = row.get(11)?;
    let qty: i64 = row.get(4)?;
    // Legacy failed rows stored the attempted count in qty; the fulfillment
    // count is 0 there, so don't fall back to qty for failed rows.
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
        timings: timings
            .as_deref()
            .and_then(|text| serde_json::from_str::<Timings>(text).ok()),
        raw_events: crate::raw_events::decode(raw_events.as_deref()),
        note: row.get(16)?,
        message: row.get(17)?,
    })
}

/// The list shape: everything the loot table renders, without the JSON blobs.
pub(super) fn row_to_entry_lite(row: &Row<'_>) -> rusqlite::Result<OrderEntry> {
    let status: String = row.get(11)?;
    let qty: i64 = row.get(4)?;
    // Legacy failed rows stored the attempted count in qty; the fulfillment
    // count is 0 there, so don't fall back to qty for failed rows.
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
        timings: None,
        raw_events: None,
        note: row.get(14)?,
        message: row.get(15)?,
    })
}

/// Convenience for building an in-memory entry in tests and the engine.
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
        requested_qty: Some(qty),
        stock_at_trigger: None,
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
