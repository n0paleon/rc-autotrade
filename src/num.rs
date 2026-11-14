//! Numeric helpers shared by the telemetry, trend and benchmark models.
//!
//! The worker mirrors JavaScript arithmetic in several places, so these helpers
//! keep the `Math.round` semantics (`floor(x + 0.5)`, including for negatives)
//! in one spot instead of re-deriving them per module.

/// `Math.round` semantics: half-up towards positive infinity.
#[inline]
pub fn js_round(value: f64) -> f64 {
    (value + 0.5).floor()
}

/// Rounds `value` to `digits` decimals.
#[inline]
pub fn round_to(value: f64, digits: i32) -> f64 {
    let factor = 10f64.powi(digits);
    js_round(value * factor) / factor
}

/// Rounds `value` to `digits` decimals after adding `f64::EPSILON`.
///
/// The trend model relies on this bias to match the JavaScript output for
/// values that sit exactly on a rounding boundary.
#[inline]
pub fn round_to_epsilon(value: f64, digits: i32) -> f64 {
    let factor = 10f64.powi(digits);
    js_round((value + f64::EPSILON) * factor) / factor
}

/// Rounds `value` to `digits` decimals, or `None` when it is not finite.
#[inline]
pub fn round_finite(value: f64, digits: i32) -> Option<f64> {
    if value.is_finite() {
        Some(round_to(value, digits))
    } else {
        None
    }
}
