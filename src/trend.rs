//! Trend + signal model, ported from `src/shared/trend.js`.
//!
//! Pure: no clock, no store, no config mutation. The engine decides when to
//! call it; the UI only renders the result.
//!
//! One intentional change from the JavaScript: the Hampel filter no longer
//! allocates a `Vec` (and two sorts) per point. It uses a fixed 5-slot stack
//! buffer with insertion sort. The feasibility study measured the old version
//! at ~1.2 ms for a 1 440-point series; this keeps the maths identical while
//! removing the allocation churn that made it expensive.

use serde::Serialize;

use crate::rules::js_round;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;

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

// ── maths helpers ───────────────────────────────────────────────────────────

fn clamp(value: f64, min: f64, max: f64) -> f64 {
    value.max(min).min(max)
}

/// `round(value, digits)` from the JS module, with `Math.round` semantics and
/// `None` for non-finite input.
fn round(value: f64, digits: i32) -> Option<f64> {
    if !value.is_finite() {
        return None;
    }
    let factor = 10f64.powi(digits);
    Some(js_round((value + f64::EPSILON) * factor) / factor)
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    }
}

/// Median of up to 5 values, sorted in place with insertion sort.
fn median_small(buffer: &mut [f64; 5], len: usize) -> f64 {
    for i in 1..len {
        let value = buffer[i];
        let mut j = i;
        while j > 0 && buffer[j - 1] > value {
            buffer[j] = buffer[j - 1];
            j -= 1;
        }
        buffer[j] = value;
    }
    if len == 0 {
        return f64::NAN;
    }
    let middle = len / 2;
    if len % 2 == 1 {
        buffer[middle]
    } else {
        (buffer[middle - 1] + buffer[middle]) / 2.0
    }
}

/// One sample per timestamp, oldest first, inside the lookback window.
fn prepare_points(history: &[Point], now: i64) -> Vec<Point> {
    use std::collections::BTreeMap;
    let mut by_time: BTreeMap<i64, f64> = BTreeMap::new();
    for point in history {
        if !point.p.is_finite() || point.p <= 0.0 {
            continue;
        }
        if point.t < now - 24 * HOUR || point.t > now + MINUTE {
            continue;
        }
        by_time.insert(point.t, point.p);
    }
    by_time.into_iter().map(|(t, p)| Point { t, p }).collect()
}

/// Hampel filter in log space, allocation-free per point.
fn hampel(values: &[f64], window: usize, sigma: f64) -> (Vec<f64>, i64) {
    let mut out = values.to_vec();
    if values.len() < 5 {
        return (out, 0);
    }

    let mut outliers = 0i64;
    for i in 0..values.len() {
        let lo = i.saturating_sub(window);
        let hi = (i + window).min(values.len() - 1);

        let mut buffer = [0.0f64; 5];
        let mut len = 0;
        for (slot, value) in values[lo..=hi].iter().enumerate() {
            buffer[slot] = *value;
            len += 1;
        }
        let center = median_small(&mut buffer, len);

        let mut deviations = [0.0f64; 5];
        let mut deviation_len = 0;
        for value in &values[lo..=hi] {
            deviations[deviation_len] = (value - center).abs();
            deviation_len += 1;
        }
        let mad = median_small(&mut deviations, deviation_len);
        let scale = (1.4826 * mad).max(0.005);

        if (values[i] - center).abs() > sigma * scale {
            out[i] = center;
            outliers += 1;
        }
    }
    (out, outliers)
}

/// Time-aware EMA. `dt` is used as-is; a long hole makes `alpha → 1`.
fn ema(values: &[f64], times: &[i64], tau_ms: f64) -> Vec<f64> {
    let mut out = vec![0.0; values.len()];
    if values.is_empty() {
        return out;
    }
    out[0] = values[0];
    for i in 1..values.len() {
        let dt = (times[i] - times[i - 1]).max(0) as f64;
        let alpha = if dt > 0.0 {
            1.0 - (-dt / tau_ms).exp()
        } else {
            0.0
        };
        out[i] = out[i - 1] + alpha * (values[i] - out[i - 1]);
    }
    out
}

/// Geometric median of the recent window, falling back to a wider window.
fn robust_level(points: &[Point], now: i64, fallback: f64) -> f64 {
    let logs_of = |window: &[Point]| -> f64 {
        let logs: Vec<f64> = window.iter().map(|point| point.p.ln()).collect();
        median(&logs).exp()
    };

    let recent: Vec<Point> = points
        .iter()
        .copied()
        .filter(|point| point.t >= now - 2 * HOUR)
        .collect();
    if recent.len() >= 3 {
        return logs_of(&recent);
    }
    let wide: Vec<Point> = points
        .iter()
        .copied()
        .filter(|point| point.t >= now - 6 * HOUR)
        .collect();
    if wide.len() >= 3 {
        return logs_of(&wide);
    }
    fallback
}

#[derive(Debug, Clone, Copy)]
struct Slope {
    value: f64,
    t_stat: f64,
}

/// Weighted least-squares slope of log price against time, in %/hour.
fn weighted_slope(values: &[f64], times: &[i64], window_ms: i64) -> Option<Slope> {
    let last_time = *times.last()?;
    let cutoff = last_time - window_ms;
    let mut start = 0usize;
    while start < times.len() - 1 && times[start] < cutoff {
        start += 1;
    }

    let count = times.len() - start;
    if count < 4 {
        return None;
    }

    let mut xs = Vec::with_capacity(count);
    let mut ys = Vec::with_capacity(count);
    let mut ws = Vec::with_capacity(count);
    for i in start..times.len() {
        let dt_next = if i + 1 < times.len() {
            times[i + 1] - times[i]
        } else if i > 0 {
            times[i] - times[i - 1]
        } else {
            MINUTE
        };
        xs.push(times[i] as f64 / HOUR as f64);
        ys.push(values[i]);
        ws.push(dt_next.max(1) as f64);
    }

    let weight_sum: f64 = ws.iter().sum();
    let x_mean: f64 = xs.iter().zip(&ws).map(|(x, w)| x * w).sum::<f64>() / weight_sum;
    let y_mean: f64 = ys.iter().zip(&ws).map(|(y, w)| y * w).sum::<f64>() / weight_sum;

    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for i in 0..xs.len() {
        let dx = xs[i] - x_mean;
        numerator += ws[i] * dx * (ys[i] - y_mean);
        denominator += ws[i] * dx * dx;
    }
    if denominator <= 1e-12 {
        return None;
    }
    let raw_slope = numerator / denominator;

    let mut sse = 0.0;
    for i in 0..xs.len() {
        let fitted = y_mean + raw_slope * (xs[i] - x_mean);
        sse += ws[i] * (ys[i] - fitted).powi(2);
    }
    let dof = xs.len() as f64 - 2.0;
    let se = if dof > 0.0 && sse > 0.0 {
        (sse / dof / denominator).sqrt()
    } else {
        0.0
    };

    Some(Slope {
        value: raw_slope * 100.0,
        t_stat: if se > 0.0 { raw_slope / se } else { 0.0 },
    })
}

/// Calibration measured by `scripts/calibrate-trend.js` for this worker.
/// Mirrors `src/shared/trend-calibration.js`; 1.0 when unknown.
fn calibration_factor(depth_source: bool) -> f64 {
    if depth_source { 1.13 } else { 1.2 }
}

// ── model ───────────────────────────────────────────────────────────────────

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

/// Widens the entry band with the model's uncertainty, inside hard limits.
pub fn classification_band_pct(error_band_pct: Option<f64>) -> f64 {
    let band = error_band_pct
        .filter(|value| value.is_finite())
        .unwrap_or(2.0);
    clamp(band, 2.0, 10.0)
}

/// Positive proximity = target above the trend; negative = waiting for a dip.
pub fn classify_state(
    proximity_pct: f64,
    previous_state: Option<&str>,
    band_pct: f64,
) -> &'static str {
    let band = if band_pct.is_finite() && band_pct > 0.0 {
        band_pct
    } else {
        2.0
    };
    let held = matches!(previous_state, Some("NEAR") | Some("UNSURE"));
    let exit = if held { (band * 1.5).max(3.0) } else { band };

    if proximity_pct > exit {
        return "ABOVE";
    }
    if proximity_pct < -exit {
        return if proximity_pct <= -20.0 {
            "FAR_BELOW"
        } else {
            "BELOW"
        };
    }
    if held {
        return match previous_state {
            Some("NEAR") => "NEAR",
            Some("UNSURE") => "UNSURE",
            _ => "NEAR",
        };
    }
    if band >= 5.0 { "UNSURE" } else { "NEAR" }
}

pub fn recommend_action(
    state: &str,
    reliable: bool,
    reliable_reason: Option<&'static str>,
) -> (&'static str, &'static str) {
    match state {
        "STALE" => ("wait", "stale"),
        "NO_DATA" => ("wait", "no-data"),
        "NO_TARGET" => ("set-target", "no-target"),
        "UNSURE" => ("wait", "unresolved"),
        "ABOVE" if !reliable => ("wait", reliable_reason.unwrap_or("low-confidence")),
        "ABOVE" => ("lower", "above-trend"),
        "FAR_BELOW" => ("review", "far-below"),
        "NEAR" => ("hold", "at-trend"),
        _ => ("hold", "below-trend"),
    }
}

fn with_action(mut signal: TrendSignal) -> TrendSignal {
    let (action, reason) = recommend_action(signal.state, signal.reliable, signal.reliable_reason);
    signal.action = action;
    signal.action_reason = reason;
    signal
}

/// Adds the target-relative reading on top of the market model.
pub fn build_trend(history: &[Point], options: &TrendOptions) -> TrendSignal {
    let model = compute_trend_model(history, options);
    let target = options
        .target_price
        .filter(|value| value.is_finite() && *value > 0.0);

    if model.status != "ok" {
        let state = if model.status == "stale" {
            "STALE"
        } else {
            "NO_DATA"
        };
        let reliable_reason = if model.status == "stale" {
            Some("stale")
        } else {
            Some("no-data")
        };
        return with_action(TrendSignal {
            model,
            target,
            proximity_pct: None,
            gap_pct: None,
            band_pct: None,
            uncertain: false,
            state,
            reliable: false,
            reliable_reason,
            action: "wait",
            action_reason: "wait",
        });
    }

    if target.is_none() {
        let band_pct = round(classification_band_pct(model.error_band_pct), 2);
        return with_action(TrendSignal {
            model,
            target: None,
            proximity_pct: None,
            gap_pct: None,
            band_pct,
            uncertain: false,
            state: "NO_TARGET",
            reliable: false,
            reliable_reason: Some("no-target"),
            action: "set-target",
            action_reason: "no-target",
        });
    }

    let target = target.expect("checked above");
    let trend_level = model.trend.unwrap_or(f64::NAN);
    let proximity_pct = (target / trend_level - 1.0) * 100.0;
    let band_pct = classification_band_pct(model.error_band_pct);
    let enough_evidence = model.confidence >= 50;
    let trustworthy_band = model.error_band_pct.unwrap_or(20.0) <= 10.0;
    let state = classify_state(proximity_pct, options.previous_state.as_deref(), band_pct);
    let reliable = enough_evidence && trustworthy_band;
    let reliable_reason = if reliable {
        None
    } else if enough_evidence {
        Some("wide-band")
    } else {
        Some("low-confidence")
    };

    with_action(TrendSignal {
        model,
        target: Some(target),
        proximity_pct: round(proximity_pct, 3),
        gap_pct: round(-proximity_pct, 3),
        band_pct: round(band_pct, 2),
        uncertain: state == "UNSURE",
        state,
        reliable,
        reliable_reason,
        action: "hold",
        action_reason: "hold",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_777_982_400_000; // 2026-05-05T12:00:00Z, same as the JS tests

    fn series(hours: i64, price_at: impl Fn(i64) -> f64) -> Vec<Point> {
        let start = NOW - hours * HOUR;
        let mut points = Vec::new();
        let mut t = start;
        while t <= NOW {
            points.push(Point { t, p: price_at(t) });
            t += MINUTE;
        }
        points
    }

    fn options(target: Option<f64>) -> TrendOptions {
        TrendOptions {
            now: NOW,
            target_price: target,
            ..Default::default()
        }
    }

    #[test]
    fn flat_market_yields_a_stable_trend_and_below_target() {
        let history = series(6, |_| 2.0);
        let signal = build_trend(&history, &options(Some(1.8)));

        assert_eq!(signal.model.status, "ok");
        assert!((signal.model.trend.unwrap() - 2.0).abs() < 1e-6);
        assert!((signal.proximity_pct.unwrap() + 10.0).abs() < 0.01);
        assert_eq!(signal.state, "BELOW");
        assert!(signal.reliable);
        assert_eq!(signal.model.direction, "flat");
        assert_eq!(signal.model.outliers, 0);
    }

    #[test]
    fn a_single_cheap_spike_is_filtered_before_the_trend() {
        let history = series(6, |t| {
            if (t - (NOW - 3 * HOUR)).abs() < MINUTE / 2 {
                0.5
            } else {
                2.0
            }
        });
        let model = compute_trend_model(&history, &options(None));

        assert!(model.outliers >= 1, "the spike must be flagged");
        assert!(model.trend.unwrap() > 1.99);
        assert!(model.momentum_pct.unwrap().abs() < 0.2);
    }

    #[test]
    fn sustained_decline_reads_as_downward_and_above_trend() {
        let history = series(6, |t| {
            2.5 - (1.5 * (t - (NOW - 6 * HOUR)) as f64) / (6 * HOUR) as f64
        });
        let signal = build_trend(&history, &options(Some(2.0)));

        assert_eq!(signal.model.status, "ok");
        assert!(signal.model.slope_pct_per_hour.unwrap() < 0.0);
        assert_eq!(signal.model.direction, "down");
        assert_eq!(signal.state, "ABOVE");
        assert!(signal.model.confidence >= 50);
    }

    #[test]
    fn hysteresis_keeps_a_boundary_target_inside_near() {
        let history = series(6, |_| 2.0);

        let fresh = build_trend(&history, &options(Some(2.05)));
        assert_eq!(fresh.state, "ABOVE");

        let held = build_trend(
            &history,
            &TrendOptions {
                previous_state: Some("NEAR".into()),
                ..options(Some(2.05))
            },
        );
        assert_eq!(held.state, "NEAR");

        let lower_held = build_trend(
            &history,
            &TrendOptions {
                previous_state: Some("NEAR".into()),
                ..options(Some(1.95))
            },
        );
        assert_eq!(lower_held.state, "NEAR");

        let lower_fresh = build_trend(&history, &options(Some(1.95)));
        assert_eq!(lower_fresh.state, "BELOW");
    }

    #[test]
    fn classification_matches_the_javascript_thresholds() {
        assert_eq!(classify_state(8.0, None, 2.0), "ABOVE");
        assert_eq!(classify_state(0.0, None, 2.0), "NEAR");
        assert_eq!(classify_state(-10.0, None, 2.0), "BELOW");
        assert_eq!(classify_state(-25.0, None, 2.0), "FAR_BELOW");
    }

    #[test]
    fn too_few_samples_produce_no_data_instead_of_a_guess() {
        let history: Vec<Point> = series(1, |_| 2.0).into_iter().take(4).collect();
        let signal = build_trend(&history, &options(Some(1.9)));

        assert_eq!(signal.model.status, "no_data");
        assert_eq!(signal.state, "NO_DATA");
        assert!(signal.model.trend.is_none());
        assert!(!signal.reliable);
        assert_eq!(signal.action, "wait");
    }

    #[test]
    fn a_frozen_feed_is_stale_and_cannot_be_reliable() {
        let history: Vec<Point> = series(6, |_| 2.0)
            .into_iter()
            .filter(|point| point.t <= NOW - 2 * HOUR)
            .collect();
        let signal = build_trend(&history, &options(Some(1.8)));

        assert_eq!(signal.model.status, "stale");
        assert_eq!(signal.state, "STALE");
        assert!(signal.model.stale);
        assert!(!signal.reliable);
        assert!(signal.model.confidence <= 20);
    }

    #[test]
    fn confidence_grows_with_the_observation_window() {
        let short = build_trend(&series(1, |_| 2.0), &options(Some(1.9)));
        let long = build_trend(&series(8, |_| 2.0), &options(Some(1.9)));
        assert!(long.model.confidence > short.model.confidence);
    }

    #[test]
    fn missing_target_is_reported_as_no_target_but_keeps_the_band() {
        let history = series(6, |_| 2.0);
        let signal = build_trend(&history, &options(None));

        assert_eq!(signal.state, "NO_TARGET");
        assert_eq!(signal.action, "set-target");
        assert!(signal.band_pct.is_some());
    }

    #[test]
    fn hampel_ignores_a_series_shorter_than_five_points() {
        let (values, outliers) = hampel(&[1.0, 2.0, 1.0], 2, 3.0);
        assert_eq!(values, vec![1.0, 2.0, 1.0]);
        assert_eq!(outliers, 0);
    }

    #[test]
    fn a_1440_point_series_stays_finite_and_fast_enough_to_be_usable() {
        let history = series(24, |t| 2.0 + ((t / MINUTE) as f64 / 40.0).sin() * 0.06);
        let signal = build_trend(&history, &options(Some(2.0)));
        assert_eq!(signal.model.status, "ok");
        assert!(signal.model.trend.unwrap().is_finite());
        assert!(signal.model.confidence > 0);
    }

    fn close(actual: f64, expected: f64, tolerance: f64) -> bool {
        (actual - expected).abs() <= tolerance
    }

    /// Fixtures produced by the *JavaScript* model on the same three series
    /// (`bun -e` against `src/shared/trend.js`). This is the parity contract:
    /// if either implementation drifts, this test fails.
    #[test]
    fn matches_the_javascript_model_on_recorded_series() {
        let flat = build_trend(&series(6, |_| 2.0), &options(Some(1.8)));
        assert!(close(flat.model.trend.unwrap(), 2.0, 1e-9));
        assert!(close(flat.model.fast.unwrap(), 2.0, 1e-9));
        assert_eq!(flat.model.confidence, 85);
        assert!(close(flat.model.error_band_pct.unwrap(), 2.4, 1e-9));
        assert!(close(flat.proximity_pct.unwrap(), -10.0, 1e-9));
        assert!(close(flat.gap_pct.unwrap(), 10.0, 1e-9));
        assert_eq!(flat.state, "BELOW");
        assert_eq!(flat.model.direction, "flat");
        assert_eq!(flat.model.points, 361);
        assert_eq!(flat.model.coverage_pct, 100);
        assert_eq!(flat.model.max_gap_ms, Some(60_000));
        assert!(flat.reliable);
        assert_eq!(flat.action, "hold");

        let decline = build_trend(
            &series(6, |t| {
                2.5 - (1.5 * (t - (NOW - 6 * HOUR)) as f64) / (6 * HOUR) as f64
            }),
            &options(Some(2.0)),
        );
        assert!(close(decline.model.trend.unwrap(), 1.25, 1e-9));
        assert!(close(decline.model.fast.unwrap(), 1.058721, 1e-6));
        assert!(close(decline.model.momentum_pct.unwrap(), -14.734, 1e-3));
        assert!(close(
            decline.model.slope_pct_per_hour.unwrap(),
            -18.464,
            1e-3
        ));
        assert!(close(decline.model.slope_t_stat.unwrap(), -184.82, 1e-2));
        assert!(close(decline.model.noise_pct.unwrap(), 17.949, 1e-3));
        assert_eq!(decline.model.confidence, 85);
        assert!(close(decline.model.error_band_pct.unwrap(), 20.0, 1e-9));
        assert!(close(decline.band_pct.unwrap(), 10.0, 1e-9));
        assert!(close(decline.proximity_pct.unwrap(), 60.0, 1e-9));
        assert_eq!(decline.state, "ABOVE");
        assert_eq!(decline.model.direction, "down");
        assert!(!decline.reliable);
        assert_eq!(decline.action, "wait");

        let sine = build_trend(
            &series(24, |t| 2.0 + ((t / MINUTE) as f64 / 40.0).sin() * 0.06),
            &options(Some(2.0)),
        );
        assert!(close(sine.model.trend.unwrap(), 1.959241, 1e-6));
        assert!(close(sine.model.fast.unwrap(), 2.022128, 1e-6));
        assert!(close(sine.model.momentum_pct.unwrap(), 1.475, 1e-3));
        assert!(close(sine.model.slope_pct_per_hour.unwrap(), 0.191, 1e-3));
        assert!(close(sine.model.slope_t_stat.unwrap(), 1.4, 1e-2));
        assert!(close(sine.model.noise_pct.unwrap(), 2.589, 1e-3));
        assert_eq!(sine.model.confidence, 85);
        assert!(close(sine.model.error_band_pct.unwrap(), 4.92, 1e-9));
        assert!(close(sine.proximity_pct.unwrap(), 2.08, 1e-9));
        assert_eq!(sine.state, "NEAR");
        assert_eq!(sine.model.direction, "flat");
        assert_eq!(sine.model.points, 1441);
        assert_eq!(sine.model.span_ms, 86_400_000);
        assert!(sine.reliable);
    }
}
