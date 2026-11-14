use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::timings::Timings;

/// One permanent loot-log row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OrderEntry {
    pub id: String,
    #[serde(default)]
    pub order_id: Option<String>,
    #[serde(default)]
    pub item_id: String,
    #[serde(default)]
    pub label: String,
    /// Quantity actually obtained, straight from the fulfillment result event
    /// (`fill.count`). 0 for failed orders.
    #[serde(default)]
    pub qty: i64,
    /// Quantity the order was placed for. `qty` was historically used for both
    /// meanings, so a partial fill overwrote how many were actually sent.
    /// Legacy rows decode as `qty` when this is absent — except failed rows,
    /// where the attempted count is unrecoverable once `qty` is the
    /// fulfillment count, so they fall back to 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_qty: Option<i64>,
    /// Stock available when the order signal triggered (the market frame that
    /// produced the intent). Rows written before this column existed keep
    /// NULL and the UI renders "—".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stock_at_trigger: Option<i64>,
    #[serde(default)]
    pub unit_price: f64,
    #[serde(default)]
    pub total_price: f64,
    #[serde(default)]
    pub target_price: f64,
    #[serde(default)]
    pub profit_rlt: f64,
    pub status: String,
    pub at: i64,
    #[serde(default)]
    pub latency_ms: Option<i64>,
    #[serde(default)]
    pub timings: Option<Timings>,
    #[serde(rename = "rawEvents", default)]
    pub raw_events: Option<Value>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl OrderEntry {
    /// Clones the list shape, dropping the heavy frames.
    ///
    /// `raw_events` can hold two 256 KB snapshots, and the SQLite profile timer
    /// counts the Rust-side JSON decode as statement time. Lists therefore keep
    /// only the columns a row renders; the modal asks for one entry at a time.
    /// The clone is field-by-field so the payloads are never deep-copied.
    pub fn lite(&self) -> Self {
        Self {
            id: self.id.clone(),
            order_id: self.order_id.clone(),
            item_id: self.item_id.clone(),
            label: self.label.clone(),
            qty: self.qty,
            requested_qty: self.requested_qty,
            stock_at_trigger: self.stock_at_trigger,
            unit_price: self.unit_price,
            total_price: self.total_price,
            target_price: self.target_price,
            profit_rlt: self.profit_rlt,
            status: self.status.clone(),
            at: self.at,
            latency_ms: self.latency_ms,
            timings: None,
            raw_events: None,
            note: self.note.clone(),
            message: self.message.clone(),
        }
    }
}

/// Aggregate kept in `report_products`; survives history rotation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Product {
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

/// `reportView()` row: the aggregate plus the derived display columns.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductView {
    pub item_id: String,
    pub label: String,
    pub fills: i64,
    pub failed: i64,
    pub qty: i64,
    pub spent_rlt: f64,
    pub profit_rlt: f64,
    pub first_at: i64,
    pub last_at: i64,
    pub avg_buy: f64,
    pub margin_pct: Option<f64>,
    pub success_pct: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportTotals {
    pub products: usize,
    pub fills: i64,
    pub failed: i64,
    pub qty: i64,
    pub spent_rlt: f64,
    pub profit_rlt: f64,
    pub margin_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportView {
    pub since: i64,
    pub totals: ReportTotals,
    pub items: Vec<ProductView>,
}
