use serde::Serialize;

/// A single 1-minute price point.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Point {
    pub t: i64,
    pub p: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FillRef {
    pub price: f64,
    pub filled: f64,
    pub partial: bool,
    pub approx: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TrendOptions {
    pub now: i64,
    pub target_price: Option<f64>,
    pub previous_state: Option<String>,
    pub book_qty: Option<i64>,
    pub min_qty: Option<i64>,
    /// `true` selects the depth series, anything else the top-ask series.
    pub depth_source: bool,
    pub fill: Option<FillRef>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendModel {
    pub status: &'static str,
    pub reason: Option<&'static str>,
    pub source: &'static str,
    pub fill_price: Option<f64>,
    pub fill_qty: Option<f64>,
    pub fill_partial: bool,
    pub fill_approx: bool,
    pub trend: Option<f64>,
    pub fast: Option<f64>,
    pub momentum_pct: Option<f64>,
    pub slope_pct_per_hour: Option<f64>,
    pub slope_t_stat: Option<f64>,
    pub noise_pct: Option<f64>,
    pub confidence: i64,
    pub error_band_pct: Option<f64>,
    pub band_source: Option<&'static str>,
    pub points: usize,
    pub span_ms: i64,
    pub max_gap_ms: Option<i64>,
    pub coverage_pct: i64,
    pub outliers: i64,
    pub direction: &'static str,
    pub stale: bool,
    pub last_sample_at: Option<i64>,
    pub computed_at: i64,
    pub book_qty: Option<i64>,
    pub min_qty: Option<i64>,
    pub thin_book: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendSignal {
    #[serde(flatten)]
    pub model: TrendModel,
    pub target: Option<f64>,
    pub proximity_pct: Option<f64>,
    pub gap_pct: Option<f64>,
    pub band_pct: Option<f64>,
    pub uncertain: bool,
    pub state: &'static str,
    pub reliable: bool,
    pub reliable_reason: Option<&'static str>,
    pub action: &'static str,
    pub action_reason: &'static str,
}
