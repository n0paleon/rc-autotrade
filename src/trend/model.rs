use crate::num::js_round;

use super::math::{
    clamp, ema, hampel, median, prepare_points, robust_level, round, weighted_slope,
};
use super::types::{Point, TrendModel, TrendOptions};
use super::{HOUR, MINUTE};

/// Calibration factor applied to the raw error band.
fn calibration_factor(depth_source: bool) -> f64 {
    if depth_source { 1.13 } else { 1.2 }
}

fn base_model(options: &TrendOptions, points: &[Point]) -> TrendModel {
    let source = if options.depth_source { "depth" } else { "top" };
    let fill_price = options
        .fill
        .map(|fill| fill.price)
        .filter(|v| v.is_finite());
    let fill_qty = options
        .fill
        .map(|fill| fill.filled)
        .filter(|v| v.is_finite());
    let fill_partial = options.fill.map(|fill| fill.partial).unwrap_or(false);
    let fill_approx = options.fill.map(|fill| fill.approx).unwrap_or(false);
    let thin_book = match (options.book_qty, options.min_qty) {
        (Some(book), Some(min)) => book < min,
        _ => false,
    };

    TrendModel {
        status: "no_data",
        reason: Some("not enough samples"),
        source,
        fill_price,
        fill_qty,
        fill_partial,
        fill_approx,
        trend: None,
        fast: None,
        momentum_pct: None,
        slope_pct_per_hour: None,
        slope_t_stat: None,
        noise_pct: None,
        confidence: 0,
        error_band_pct: None,
        band_source: None,
        points: points.len(),
        span_ms: 0,
        max_gap_ms: None,
        coverage_pct: 0,
        outliers: 0,
        direction: "flat",
        stale: false,
        last_sample_at: points.last().map(|point| point.t),
        computed_at: options.now,
        book_qty: options.book_qty,
        min_qty: options.min_qty,
        thin_book,
    }
}

/// Market-only model, independent of `targetPrice`.
pub fn compute_trend_model(history: &[Point], options: &TrendOptions) -> TrendModel {
    let now = options.now;
    let points = prepare_points(history, now);
    let mut base = base_model(options, &points);

    if points.len() < 12 {
        return base;
    }

    let span_ms = points[points.len() - 1].t - points[0].t;
    if span_ms < 30 * MINUTE {
        base.reason = Some("warming up");
        base.span_ms = span_ms;
        return base;
    }

    let age = now - points[points.len() - 1].t;
    let stale = age > 30 * MINUTE;

    let mut max_gap = 0i64;
    for pair in points.windows(2) {
        max_gap = max_gap.max(pair[1].t - pair[0].t);
    }

    let expected = ((span_ms as f64 / MINUTE as f64).round() as i64 + 1).max(1);
    let coverage_pct =
        js_round(clamp(points.len() as f64 / expected as f64, 0.0, 1.0) * 100.0) as i64;

    let times: Vec<i64> = points.iter().map(|point| point.t).collect();
    let logs: Vec<f64> = points.iter().map(|point| point.p.ln()).collect();
    let (cleaned, outliers) = hampel(&logs, 2, 3.0);

    let fast = ema(&cleaned, &times, (15 * MINUTE) as f64);
    let slow = ema(&cleaned, &times, (60 * MINUTE) as f64);

    let last = logs.len() - 1;
    let trend = robust_level(&points, now, slow[last].exp());
    let fast_price = fast[last].exp();
    let momentum_pct = (fast[last] - slow[last]) * 100.0;
    let slope = weighted_slope(&cleaned, &times, 3 * HOUR);
    let slope_pct_per_hour = slope.map(|value| value.value);
    let slope_t_stat = slope.map(|value| value.t_stat);

    let deviations: Vec<f64> = cleaned
        .iter()
        .zip(&slow)
        .map(|(value, baseline)| (value - baseline).abs())
        .collect();
    let noise_pct = median(&deviations) * 1.4826 * 100.0;

    let fresh = clamp(1.0 - age as f64 / (30 * MINUTE) as f64, 0.0, 1.0);
    let density = clamp((coverage_pct as f64 / 100.0) / (25.0 / 100.0), 0.0, 1.0);
    let span_score = clamp(span_ms as f64 / (6 * HOUR) as f64, 0.0, 1.0);
    let count_score = clamp(points.len() as f64 / 120.0, 0.0, 1.0);
    let gap_score = 1.0 / (1.0 + max_gap as f64 / ((30 * MINUTE) as f64 * 1.5));
    let quality = (if options.depth_source { 1.0 } else { 0.85 })
        * (if base.thin_book { 0.7 } else { 1.0 })
        * (if base.fill_approx { 0.9 } else { 1.0 });

    let mut confidence = 100.0
        * quality
        * (0.4 + 0.6 * density)
        * (0.34 * fresh + 0.28 * span_score + 0.22 * count_score + 0.16 * gap_score);
    if stale {
        confidence = confidence.min(20.0);
    }

    let raw_band_pct = clamp(0.9 * noise_pct + 1.2 * momentum_pct.abs(), 2.0, 20.0);
    let error_band_pct = clamp(
        raw_band_pct * calibration_factor(options.depth_source),
        2.0,
        20.0,
    );

    let direction_significant = slope_t_stat
        .map(|value| value.abs() >= 2.0)
        .unwrap_or(false);
    let direction_value = slope_pct_per_hour.unwrap_or(momentum_pct);
    let direction = if !direction_significant {
        "flat"
    } else if direction_value > 0.3 {
        "up"
    } else if direction_value < -0.3 {
        "down"
    } else {
        "flat"
    };

    base.status = if stale { "stale" } else { "ok" };
    base.reason = if stale {
        Some("no fresh market frame")
    } else {
        None
    };
    base.trend = round(trend, 6);
    base.fast = round(fast_price, 6);
    base.momentum_pct = round(momentum_pct, 3);
    base.slope_pct_per_hour = slope_pct_per_hour.and_then(|value| round(value, 3));
    base.slope_t_stat = slope_t_stat.and_then(|value| round(value, 2));
    base.noise_pct = round(noise_pct, 3);
    base.confidence = js_round(confidence) as i64;
    base.error_band_pct = round(error_band_pct, 2);
    base.band_source = Some("calibrated");
    base.points = points.len();
    base.span_ms = span_ms;
    base.max_gap_ms = Some(max_gap);
    base.coverage_pct = coverage_pct;
    base.outliers = outliers;
    base.stale = stale;
    base.direction = direction;
    base.last_sample_at = Some(points[points.len() - 1].t);
    base.computed_at = now;
    base
}
