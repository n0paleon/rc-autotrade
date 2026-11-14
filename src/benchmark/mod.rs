//! Rolling order benchmark.
//!
//! The reorderable latency tracker in [`crate::latency`] measures transport
//! calls (`http`, keep-alive, websocket). This module measures the **order
//! pipeline** instead: every confirmed or rejected purchase carries a
//! [`Timings`] object with absolute epoch marks, and the deck wants to know how
//! long each stage takes — average, p50, p95, jitter and worst case — over the
//! last N orders.
//!
//! Nothing is persisted here: samples are rebuilt on demand from the
//! `orders.timings` column, so the numbers survive a restart and reset
//! automatically when the loot log is wiped.

use serde_json::{Map, Value, json};

use crate::timings::Timings;

mod history;
mod stats;

#[cfg(test)]
mod tests;

use stats::{stage_stats, summarize};

pub use history::history;

/// How many recent orders one snapshot covers.
pub const CAPACITY: usize = 200;

/// Windows the deck can switch between without another round trip.
pub const WINDOWS: [usize; 3] = [25, 50, CAPACITY];

/// Most recent end-to-end durations returned for the trend strip. Matches the
/// 50-order panel window so the chart and the averages always describe the
/// same orders.
const SERIES_LIMIT: usize = 50;

/// Widest window `/api/benchmark/history` will aggregate.
pub const HISTORY_MAX_HOURS: i64 = 168;

/// Rolling average shown by the Loot Log monitor counter. Count-based, not
/// time-based: a quiet market still reports a full window and a burst cannot
/// crowd the window to a handful of seconds.
pub const RECENT_ORDERS: usize = 50;

/// Rows decoded for one history request. Far above a normal day of orders;
/// keeps a pathological log from parking the reactor on JSON decoding.
pub const HISTORY_MAX_SAMPLES: usize = 20_000;

/// Buckets the deck offers, from five minutes to a day.
pub const HISTORY_BUCKETS: [(&str, i64); 8] = [
    ("5m", 5 * 60_000),
    ("15m", 15 * 60_000),
    ("30m", 30 * 60_000),
    ("1h", 60 * 60_000),
    ("3h", 3 * 60 * 60_000),
    ("6h", 6 * 60 * 60_000),
    ("12h", 12 * 60 * 60_000),
    ("1d", 86_400_000),
];

/// Resolves a `?bucket=` key; unknown keys fall back to the caller's default.
pub fn bucket_ms_from_key(key: &str) -> Option<i64> {
    HISTORY_BUCKETS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, ms)| *ms)
}

/// One completed order, newest first in the store result.
#[derive(Debug, Clone)]
pub struct Sample {
    pub at: i64,
    pub status: String,
    pub timings: Timings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Upstream frame received → engine handler starts.
    Delivery,
    /// Target match, pricing and safety gates.
    Decision,
    /// Counter update, SQLite write and order event.
    Commit,
    /// Payload built and HTTP request dispatched.
    Submit,
    /// Dispatch until the first response byte.
    Ttfb,
    /// Response body read and parsed.
    Body,
    /// HTTP result → accepted or rejected.
    Settle,
    /// Purchase request TTFB → marketplace fulfillment frame.
    FillWait,
    /// Fulfillment frame → loot row written.
    FillHandle,
    /// First signal mark → the last lifecycle mark.
    Total,
}

impl Phase {
    const ALL: [Phase; 10] = [
        Phase::Delivery,
        Phase::Decision,
        Phase::Commit,
        Phase::Submit,
        Phase::Ttfb,
        Phase::Body,
        Phase::Settle,
        Phase::FillWait,
        Phase::FillHandle,
        Phase::Total,
    ];

    fn key(self) -> &'static str {
        match self {
            Phase::Delivery => "delivery",
            Phase::Decision => "decision",
            Phase::Commit => "commit",
            Phase::Submit => "submit",
            Phase::Ttfb => "ttfb",
            Phase::Body => "body",
            Phase::Settle => "settle",
            Phase::FillWait => "fillWait",
            Phase::FillHandle => "fillHandle",
            Phase::Total => "total",
        }
    }

    fn from_key(key: &str) -> Option<Phase> {
        Phase::ALL.iter().copied().find(|phase| phase.key() == key)
    }

    /// Milliseconds this stage took, or `None` when the marks are incomplete or
    /// the clock moved backwards between them.
    fn measure(self, timings: &Timings) -> Option<f64> {
        match self {
            Phase::Delivery => delta(timings.signal_at, timings.handle_at),
            Phase::Decision => delta(timings.handle_at, timings.order_at),
            Phase::Commit => delta(timings.order_at, timings.dispatch_at),
            Phase::Submit => delta(timings.dispatch_at, timings.send_at),
            // The monotonic TTFB is immune to wall-clock changes and excludes a
            // 401 retry; fall back to the wall-clock delta for older rows.
            Phase::Ttfb => timings
                .ttfb_ms
                .filter(|value| *value >= 0.0)
                .or_else(|| delta(timings.send_at, timings.response_at)),
            Phase::Body => delta(timings.response_at, timings.complete_at),
            Phase::Settle => delta(
                timings.complete_at,
                timings.accepted_at.or(timings.failed_at),
            ),
            Phase::FillWait => delta(timings.ttfb_at(), timings.fill_at),
            Phase::FillHandle => delta(timings.fill_at, timings.fill_handled_at),
            Phase::Total => delta(
                timings.signal_at,
                timings
                    .fill_handled_at
                    .or(timings.fill_at)
                    .or(timings.failed_at)
                    .or(timings.accepted_at)
                    .or(timings.complete_at),
            ),
        }
    }
}

fn delta(from: Option<f64>, to: Option<f64>) -> Option<f64> {
    let (from, to) = (from?, to?);
    if !from.is_finite() || !to.is_finite() || to < from {
        return None;
    }
    Some(round2(to - from))
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// The full `/api/benchmark` / `benchmark` push payload.
pub fn snapshot(samples: &[Sample], now: i64) -> Value {
    let orders = samples.len();
    let filled = samples
        .iter()
        .filter(|sample| sample.status == "filled")
        .count();
    let failed = samples
        .iter()
        .filter(|sample| sample.status == "failed")
        .count();
    let success_rate = if orders == 0 {
        Value::Null
    } else {
        json!(round1(filled as f64 / orders as f64 * 100.0))
    };

    let mut windows = Map::new();
    for window in WINDOWS {
        let take = window.min(orders);
        windows.insert(window.to_string(), summarize(&samples[..take]));
    }

    let series: Vec<Value> = samples
        .iter()
        .take(SERIES_LIMIT)
        .filter_map(|sample| {
            let total = Phase::Total.measure(&sample.timings)?;
            Some(json!({
                "at": sample.at,
                "status": sample.status,
                "total": total,
            }))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    json!({
        "measuredAt": now,
        "capacity": CAPACITY,
        "available": orders,
        "lastOrderAt": samples.first().map(|sample| sample.at),
        "totals": {
            "orders": orders,
            "filled": filled,
            "failed": failed,
            "successRate": success_rate,
        },
        "windows": Value::Object(windows),
        "series": series,
    })
}

/// Empty payload, used before the first order and after a stats reset.
pub fn empty(now: i64) -> Value {
    snapshot(&[], now)
}

/// End-to-end latency summary over the newest `count` orders (`samples` is
/// newest-first, as returned by the store).
///
/// The Loot Log counter shows this average. It is count-based rather than
/// time-based, so quiet and busy markets both report a full window; fewer
/// than `count` recent orders simply reports what exists.
pub fn recent(samples: &[Sample], count: usize) -> Value {
    let window = samples.iter().take(count.max(1));
    let mut values = Vec::new();
    let mut orders = 0usize;
    let mut filled = 0usize;
    let mut failed = 0usize;
    let mut newest_at: Option<i64> = None;
    let mut oldest_at: Option<i64> = None;

    for sample in window {
        orders += 1;
        if sample.status == "filled" {
            filled += 1;
        }
        if sample.status == "failed" {
            failed += 1;
        }
        if let Some(ms) = Phase::Total.measure(&sample.timings) {
            values.push(ms);
        }
        newest_at = Some(newest_at.map_or(sample.at, |at| at.max(sample.at)));
        oldest_at = Some(oldest_at.map_or(sample.at, |at| at.min(sample.at)));
    }

    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut summary = stage_stats(&values, None)
        .as_object()
        .cloned()
        .unwrap_or_default();
    summary.insert("count".to_string(), json!(count.max(1)));
    summary.insert("orders".to_string(), json!(orders));
    summary.insert("filled".to_string(), json!(filled));
    summary.insert("failed".to_string(), json!(failed));
    summary.insert("until".to_string(), json!(newest_at.unwrap_or(0)));
    summary.insert("since".to_string(), json!(oldest_at.unwrap_or(0)));
    Value::Object(summary)
}
