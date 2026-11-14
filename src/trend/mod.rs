//! Trend + signal model.
//!
//! Pure: no clock, no store, no config mutation. The engine decides when to
//! call it; the UI only renders the result.

mod math;
mod model;
mod signal;
mod types;

#[cfg(test)]
mod tests;

pub use model::compute_trend_model;
pub use signal::{build_trend, classification_band_pct, classify_state, recommend_action};
pub use types::{FillRef, Point, TrendModel, TrendOptions, TrendSignal};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
