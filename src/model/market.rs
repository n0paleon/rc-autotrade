use serde::{Deserialize, Serialize};

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
