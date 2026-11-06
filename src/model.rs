//! Wire and storage types.
//!
//! Every struct serializes with `camelCase` so the Rust worker can serve the
//! existing React deck without a translation layer. Field names mirror
//! `src/worker/store.js` and `src/shared/constants.js` exactly.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::timings::Timings;

fn default_item_type() -> String {
    "mutation_component".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub item_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_item_type")]
    pub item_type: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rarity_color: Option<String>,
    pub target_price: f64,
    pub desired_qty: i64,
    pub high_stock_threshold: i64,
    pub min_acceptable_qty: i64,
    #[serde(default = "default_true")]
    pub allow_partial_order: bool,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub updated_at: Option<i64>,
}

impl Target {
    pub fn plan_input(&self) -> crate::rules::PlanInput {
        crate::rules::PlanInput {
            active: self.active,
            target_price: self.target_price,
            desired_qty: self.desired_qty,
            high_stock_threshold: self.high_stock_threshold,
            min_acceptable_qty: self.min_acceptable_qty,
            allow_partial_order: self.allow_partial_order,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub delay_per_order: i64,
    pub max_orders_per_minute: i64,
    pub dry_run: bool,
    pub auto_refresh: bool,
    pub paused: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let defaults = crate::constants::DEFAULT_SETTINGS;
        Self {
            delay_per_order: defaults.delay_per_order,
            max_orders_per_minute: defaults.max_orders_per_minute,
            dry_run: defaults.dry_run,
            auto_refresh: defaults.auto_refresh,
            paused: defaults.paused,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub since: i64,
    pub profit_rlt: f64,
    pub spent_rlt: f64,
    pub bought: i64,
    pub filled: i64,
    pub failed: i64,
    pub orders_sent: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub cookie: String,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub last_refresh_at: Option<i64>,
    #[serde(default)]
    pub refresh_count: i64,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// One permanent loot-log row, shaped exactly like `store.js#orderToEntry`.
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
    #[serde(default)]
    pub qty: i64,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MarketSnapshot {
    pub item_id: String,
    #[serde(default)]
    pub last_price: Option<f64>,
    #[serde(default)]
    pub last_stock: Option<i64>,
    #[serde(default)]
    pub allowed_price_min: Option<f64>,
    #[serde(default)]
    pub allowed_price_max: Option<f64>,
    #[serde(default)]
    pub allowed_at: Option<i64>,
    #[serde(default)]
    pub item_type: Option<String>,
    pub seen_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PricePoint {
    pub item_id: String,
    pub bucket_at: i64,
    pub price: f64,
    pub fill_price: Option<f64>,
    pub fill_qty: Option<f64>,
    pub fill_approx: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_serialize_in_the_ui_shape() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        assert_eq!(json["delayPerOrder"], 200);
        assert_eq!(json["maxOrdersPerMinute"], 60);
        assert_eq!(json["dryRun"], false);
        assert_eq!(json["autoRefresh"], true);
    }

    #[test]
    fn target_uses_camel_case_and_skips_absent_optionals() {
        let target = Target {
            item_id: "abc123".into(),
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
        };
        let json = serde_json::to_value(&target).unwrap();
        assert_eq!(json["itemId"], "abc123");
        assert_eq!(json["targetPrice"], 0.1);
        assert!(json.get("rarityColor").is_none());
        assert!(json.get("createdAt").is_none());
    }
}
