use crate::num::round_to_epsilon;

use super::types::Point;
use super::{HOUR, MINUTE};

pub(super) fn clamp(value: f64, min: f64, max: f64) -> f64 {
    value.max(min).min(max)
}

/// `round(value, digits)` with `None` for non-finite input.
pub(super) fn round(value: f64, digits: i32) -> Option<f64> {
    if value.is_finite() {
        Some(round_to_epsilon(value, digits))
    } else {
        None
    }
}

pub(super) fn median(values: &[f64]) -> f64 {
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
pub(super) fn prepare_points(history: &[Point], now: i64) -> Vec<Point> {
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
pub(super) fn hampel(values: &[f64], window: usize, sigma: f64) -> (Vec<f64>, i64) {
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
pub(super) fn ema(values: &[f64], times: &[i64], tau_ms: f64) -> Vec<f64> {
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
pub(super) fn robust_level(points: &[Point], now: i64, fallback: f64) -> f64 {
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
pub(super) struct Slope {
    pub(super) value: f64,
    pub(super) t_stat: f64,
}

/// Weighted least-squares slope of log price against time, in %/hour.
pub(super) fn weighted_slope(values: &[f64], times: &[i64], window_ms: i64) -> Option<Slope> {
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
