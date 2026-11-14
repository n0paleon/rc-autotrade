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

/// What the worker is allowed to do with a passing signal.
///
/// This replaces the old pair of independent booleans (`paused` + `dryRun`),
/// which could express four states even though one of them — paused with
/// dry-run on — behaved exactly like paused alone.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Stop deciding: no plan is made, no cooldown or rate slot is spent, the
    /// warm HTTP probe stops, and the market feed keeps streaming.
    Off,
    /// Decide and log exactly as live would, but never submit; dry-run orders
    /// still consume cooldown and the per-minute cap so the budget is honest.
    Shadow,
    /// Decide and submit to Rollercoin. Requires a linked session.
    #[default]
    Live,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Shadow => "shadow",
            Mode::Live => "live",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Mode::Off),
            "shadow" => Some(Mode::Shadow),
            "live" => Some(Mode::Live),
            _ => None,
        }
    }

    /// Off makes `plan_order` short-circuit; shadow and live plan normally.
    pub fn plans(self) -> bool {
        !matches!(self, Mode::Off)
    }

    /// Only live mode talks to the purchase endpoint.
    pub fn submits(self) -> bool {
        matches!(self, Mode::Live)
    }

    /// Keeps the upstream edge warm. Off stops probing on purpose, shadow keeps
    /// probing so a dry-run session still exercises the same network path.
    pub fn probes(self) -> bool {
        !matches!(self, Mode::Off)
    }

    /// Legacy mirror for the `settings` row. Off deliberately writes
    /// `dry_run = 1` so an older reader that unpauses lands in dry-run, never
    /// straight into live trading.
    pub fn legacy_flags(self) -> (bool, bool) {
        match self {
            Mode::Off => (true, true),
            Mode::Shadow => (true, false),
            Mode::Live => (false, false),
        }
    }

    /// Best-effort recovery for rows written before schema v8, or for an
    /// unrecognised `mode` string.
    pub fn from_legacy(dry_run: bool, paused: bool) -> Self {
        if paused {
            Mode::Off
        } else if dry_run {
            Mode::Shadow
        } else {
            Mode::Live
        }
    }
}

/// Runtime settings. `mode` is the only trading switch; the old `dryRun` and
/// `paused` keys are still accepted on input, echoed on output, and mirrored
/// into the two legacy columns so a pre-v8 reader still sees sane flags.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub delay_per_order: i64,
    pub max_orders_per_minute: i64,
    pub mode: Mode,
    pub auto_refresh: bool,
    /// Percentage of headroom added to the purchase request's `totalPrice`.
    /// It only changes the price ceiling sent upstream, never the plan, the
    /// quantity or the target-price gate.
    pub total_price_markup_pct: f64,
}

impl Default for Settings {
    fn default() -> Self {
        let defaults = crate::constants::DEFAULT_SETTINGS;
        Self {
            delay_per_order: defaults.delay_per_order,
            max_orders_per_minute: defaults.max_orders_per_minute,
            mode: Mode::default(),
            auto_refresh: defaults.auto_refresh,
            total_price_markup_pct: defaults.total_price_markup_pct,
        }
    }
}

impl Settings {
    /// Legacy columns written alongside `mode`, so a pre-v8 reader still sees a
    /// sane pair of flags.
    pub fn legacy_flags(&self) -> (bool, bool) {
        self.mode.legacy_flags()
    }
}

impl Serialize for Settings {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let (dry_run, paused) = self.legacy_flags();
        let mut map = serializer.serialize_map(Some(7))?;
        map.serialize_entry("delayPerOrder", &self.delay_per_order)?;
        map.serialize_entry("maxOrdersPerMinute", &self.max_orders_per_minute)?;
        map.serialize_entry("mode", &self.mode)?;
        map.serialize_entry("autoRefresh", &self.auto_refresh)?;
        map.serialize_entry("totalPriceMarkupPct", &self.total_price_markup_pct)?;
        // Read-only mirrors: they keep an older deck or backup reader working
        // and are ignored whenever `mode` is present.
        map.serialize_entry("dryRun", &dry_run)?;
        map.serialize_entry("paused", &paused)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for Settings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            delay_per_order: i64,
            max_orders_per_minute: i64,
            #[serde(default)]
            mode: Option<Mode>,
            #[serde(default)]
            dry_run: Option<bool>,
            #[serde(default)]
            paused: Option<bool>,
            #[serde(default = "default_true")]
            auto_refresh: bool,
            /// Added after schema v8; an older document simply means "no
            /// headroom", which is the pre-feature behaviour.
            #[serde(default)]
            total_price_markup_pct: f64,
        }

        let wire = Wire::deserialize(deserializer)?;
        // `mode` always wins. The legacy pair is only read when it is absent,
        // and it never infers Live from half a pair: an old document that did
        // not say "dry run is off" must not quietly arm real trading.
        let mode = match (wire.paused, wire.dry_run) {
            (Some(true), _) => Mode::Off,
            (_, Some(true)) => Mode::Shadow,
            (Some(false), Some(false)) => Mode::Live,
            (Some(false), None) | (None, Some(false)) | (None, None) => Mode::Shadow,
        };
        Ok(Settings {
            delay_per_order: wire.delay_per_order,
            max_orders_per_minute: wire.max_orders_per_minute,
            mode: wire.mode.unwrap_or(mode),
            auto_refresh: wire.auto_refresh,
            total_price_markup_pct: wire.total_price_markup_pct,
        })
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
    pub low_price: Option<f64>,
    pub high_price: Option<f64>,
    pub raw_low_price: Option<f64>,
    pub raw_high_price: Option<f64>,
    pub fill_price: Option<f64>,
    pub fill_qty: Option<f64>,
    pub fill_approx: bool,
}

/// One stored minute as the chart consumes it: close plus the intra-minute
/// low/high bands. `low`/`high` only cover prices that passed the dwell guard;
/// `raw_low`/`raw_high` cover every print, including ones the guard rejected.
/// The bands are optional so rows from a pre-band database still decode (the
/// reader coalesces them to the close).
#[derive(Debug, Clone, PartialEq)]
pub struct PriceSeriesPoint {
    pub bucket_at: i64,
    pub price: f64,
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub raw_low: Option<f64>,
    pub raw_high: Option<f64>,
}

/// The four edges written with one price point: the band that held and the raw
/// band of every print, dwell guard included.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceBand {
    pub low: f64,
    pub high: f64,
    pub raw_low: f64,
    pub raw_high: f64,
}

/// One item the market feed has shown, cached so the monitor can name it
/// without asking upstream. Filled by `item-info` the first time the id is
/// observed; `label` and `rarity` stay empty until that succeeds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogItem {
    pub item_id: String,
    pub item_type: String,
    pub label: String,
    pub rarity: Option<String>,
    pub rarity_color: Option<String>,
    pub first_seen_at: Option<i64>,
    pub last_seen_at: Option<i64>,
    pub resolved_at: Option<i64>,
    /// Times a lookup was attempted, success or not. Used to rotate the batch
    /// so a handful of unnameable ids cannot starve every other row forever.
    pub attempts: i64,
    /// Last time a lookup was attempted. A transport failure is not remembered
    /// as final, so a flaky network must still recover — but not on every
    /// refresh while other rows are waiting.
    pub last_attempt_at: Option<i64>,
}

impl CatalogItem {
    /// True while the display name still has to come from `item-info`.
    pub fn needs_resolution(&self) -> bool {
        self.resolved_at.is_none()
    }
}

/// One `(item, hour)` activity row, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemEventPoint {
    pub item_id: String,
    pub bucket_at: i64,
    pub item_type: Option<String>,
    pub events: i64,
    pub last_seen_at: Option<i64>,
}

/// One ranked row of the item monitor: aggregate over the requested range plus
/// the catalogue metadata the table displays.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemActivityRow {
    pub item_id: String,
    pub item_type: String,
    /// Resolved product name, empty until `item-info` answers.
    pub label: String,
    pub rarity: Option<String>,
    pub rarity_color: Option<String>,
    /// Market frames observed in the selected range.
    pub events: i64,
    /// Frames observed in the last 24 hours, regardless of the range filters:
    /// the heat column keeps working while the table is scoped to an old week.
    pub events_24h: i64,
    pub first_at: Option<i64>,
    pub last_at: Option<i64>,
    /// True when this id is on the target fleet.
    pub tracked: bool,
}

/// Frames and ids for one item type across the whole requested range.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemTypeTotal {
    pub item_type: String,
    pub events: i64,
    pub items: i64,
}

/// One page of the monitor plus the totals that describe the entire range.
///
/// The totals are computed before paging, so the per-type summary in the header
/// does not change as the reader moves between pages.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemActivityReport {
    /// The requested page, already ranked and sliced.
    pub rows: Vec<ItemActivityRow>,
    /// How many ids matched the filters before the page was cut.
    pub total_rows: usize,
    pub total_events: i64,
    pub type_totals: Vec<ItemTypeTotal>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_serialize_in_the_ui_shape() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        assert_eq!(json["delayPerOrder"], 200);
        assert_eq!(json["maxOrdersPerMinute"], 60);
        assert_eq!(json["mode"], "live");
        assert_eq!(json["autoRefresh"], true);
        assert_eq!(json["totalPriceMarkupPct"], 0.0);
        // Legacy mirrors, read-only for the deck but present for old readers.
        assert_eq!(json["dryRun"], false);
        assert_eq!(json["paused"], false);
    }

    #[test]
    fn a_legacy_settings_document_maps_to_one_mode() {
        // A document that says nothing about trading must not be read as
        // "armed for live": the safe reading of silence is shadow.
        let settings: Settings = serde_json::from_value(
            serde_json::json!({ "delayPerOrder": 200, "maxOrdersPerMinute": 60 }),
        )
        .unwrap();
        assert_eq!(settings.mode, Mode::Shadow, "silence is not live");
        assert!(settings.auto_refresh);
        assert_eq!(
            settings.total_price_markup_pct, 0.0,
            "an older document has no price headroom"
        );

        let paused: Settings = serde_json::from_value(serde_json::json!({
            "delayPerOrder": 200, "maxOrdersPerMinute": 60, "paused": true, "dryRun": true
        }))
        .unwrap();
        assert_eq!(paused.mode, Mode::Off, "the dead cell collapses to off");

        let dry: Settings = serde_json::from_value(serde_json::json!({
            "delayPerOrder": 200, "maxOrdersPerMinute": 60, "dryRun": true, "paused": false
        }))
        .unwrap();
        assert_eq!(dry.mode, Mode::Shadow);

        let live: Settings = serde_json::from_value(serde_json::json!({
            "delayPerOrder": 200, "maxOrdersPerMinute": 60, "dryRun": false, "paused": false
        }))
        .unwrap();
        assert_eq!(live.mode, Mode::Live, "live needs both flags to say so");
    }

    #[test]
    fn an_explicit_mode_beats_the_legacy_flags() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "delayPerOrder": 200, "maxOrdersPerMinute": 60,
            "mode": "shadow", "dryRun": false, "paused": false
        }))
        .unwrap();
        assert_eq!(settings.mode, Mode::Shadow);
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
