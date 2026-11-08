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

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::timings::Timings;

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
    /// Accepted order → marketplace fulfillment frame.
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
            Phase::FillWait => delta(timings.accepted_at, timings.fill_at),
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

/// `count/last/min/max/avg/p50/p95/jitter` for one stage, mirroring the shape
/// the latency tracker returns so the deck can share its formatting helpers.
fn stage_stats(sorted: &[f64], last: Option<f64>) -> Value {
    let count = sorted.len();
    if count == 0 {
        return json!({
            "count": 0,
            "last": null,
            "min": null,
            "max": null,
            "avg": null,
            "p50": null,
            "p95": null,
            "jitter": null,
        });
    }

    let sum: f64 = sorted.iter().sum();
    let avg = sum / count as f64;
    let variance = sorted
        .iter()
        .map(|value| (value - avg).powi(2))
        .sum::<f64>()
        / count as f64;
    let at = |q: f64| sorted[((q * count as f64) as usize).min(count - 1)];

    json!({
        "count": count,
        "last": last.map(round2),
        "min": round2(sorted[0]),
        "max": round2(sorted[count - 1]),
        "avg": round2(avg),
        "p50": round2(at(0.5)),
        "p95": round2(at(0.95)),
        "jitter": round2(variance.sqrt()),
    })
}

/// Stats for every stage over the newest `window` samples (newest first).
fn summarize(samples: &[Sample]) -> Value {
    let mut stages = Map::new();
    for phase in Phase::ALL {
        let mut values: Vec<f64> = Vec::with_capacity(samples.len());
        let mut last: Option<f64> = None;
        for sample in samples {
            let Some(ms) = phase.measure(&sample.timings) else {
                continue;
            };
            if last.is_none() {
                last = Some(ms);
            }
            values.push(ms);
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        stages.insert(phase.key().to_string(), stage_stats(&values, last));
    }
    json!({ "samples": samples.len(), "stages": Value::Object(stages) })
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

#[derive(Debug, Default)]
struct BucketAccum {
    values: Vec<f64>,
    filled: usize,
    failed: usize,
    last: Option<f64>,
}

/// Aggregates the order log into local-clock buckets for the pro chart.
///
/// `stage` selects one pipeline phase (`total`, `ttfb`, `fillWait`, …); unknown
/// keys fall back to the end-to-end span. Buckets are aligned to the viewer's
/// clock (`tz_offset_ms`, east-positive) so a 1d bucket starts at local midnight
/// instead of 07:00 in Jakarta. Buckets without any measurable order are not
/// invented: the chart connects the orders it actually has.
pub fn history(
    samples: &[Sample],
    since: i64,
    until: i64,
    bucket_ms: i64,
    tz_offset_ms: i64,
    stage: &str,
) -> Value {
    let size = bucket_ms.clamp(60_000, 86_400_000);
    let offset = tz_offset_ms.clamp(-14 * 3_600_000, 14 * 3_600_000);
    let phase = Phase::from_key(stage).unwrap_or(Phase::Total);
    let align = |at: i64| ((at + offset).div_euclid(size)) * size - offset;

    let mut ordered: Vec<&Sample> = samples
        .iter()
        .filter(|sample| sample.at >= since && sample.at <= until)
        .collect();
    ordered.sort_by_key(|sample| sample.at);

    let mut buckets: BTreeMap<i64, BucketAccum> = BTreeMap::new();
    let mut all: Vec<f64> = Vec::new();
    let mut orders = 0usize;
    let mut filled = 0usize;
    let mut failed = 0usize;

    for sample in ordered {
        orders += 1;
        let is_filled = sample.status == "filled";
        let is_failed = sample.status == "failed";
        if is_filled {
            filled += 1;
        }
        if is_failed {
            failed += 1;
        }

        let bucket = buckets.entry(align(sample.at)).or_default();
        if is_filled {
            bucket.filled += 1;
        }
        if is_failed {
            bucket.failed += 1;
        }
        let Some(ms) = phase.measure(&sample.timings) else {
            continue;
        };
        bucket.values.push(ms);
        bucket.last = Some(ms);
        all.push(ms);
    }

    let mut points = Vec::with_capacity(buckets.len());
    for (at, mut bucket) in buckets {
        bucket
            .values
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut object = stage_stats(&bucket.values, bucket.last)
            .as_object()
            .cloned()
            .unwrap_or_default();
        object.insert("at".to_string(), json!(at));
        object.insert("n".to_string(), json!(bucket.values.len()));
        object.insert("filled".to_string(), json!(bucket.filled));
        object.insert("failed".to_string(), json!(bucket.failed));
        points.push(Value::Object(object));
    }

    all.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut summary = stage_stats(&all, None)
        .as_object()
        .cloned()
        .unwrap_or_default();
    summary.insert("orders".to_string(), json!(orders));
    summary.insert("filled".to_string(), json!(filled));
    summary.insert("failed".to_string(), json!(failed));
    summary.insert("buckets".to_string(), json!(points.len()));

    // Headline stats the expanded chart shows. `total` is the same set of
    // values regardless of bucket size, so copying it into the header makes
    // bucket switches a no-op — misleading, not interesting. The header
    // instead reports the *newest bucket*'s stats, so switching the bucket
    // flips it to a different period: exactly what the user expects when
    // they change the bucket. Buckets at the period edge can be partial, so
    // also report the oldest bucket for context.
    let bucket_summary = |at: i64| {
        points.iter().find(|point| point["at"] == at).cloned()
    };
    let newest_bucket_at = points.last().and_then(|point| point["at"].as_i64());
    let oldest_bucket_at = points.first().and_then(|point| point["at"].as_i64());
    let bucket_stats = newest_bucket_at.and_then(bucket_summary);

    json!({
        "since": since,
        "until": until,
        "bucketMs": size,
        "tzOffsetMs": offset,
        "stage": phase.key(),
        "summary": Value::Object(summary),
        "bucketSummary": bucket_stats,
        "oldestBucketAt": oldest_bucket_at.unwrap_or(0),
        "newestBucketAt": newest_bucket_at.unwrap_or(0),
        "points": points,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timings(raw: Value) -> Timings {
        crate::timings::sanitize(&raw).expect("valid timings")
    }

    fn sample(at: i64, status: &str, raw: Value) -> Sample {
        Sample {
            at,
            status: status.to_string(),
            timings: timings(raw),
        }
    }

    #[test]
    fn measures_every_stage_of_a_filled_order() {
        let t = timings(json!({
            "signalAt": 1_000,
            "handleAt": 1_004,
            "orderAt": 1_010,
            "dispatchAt": 1_012,
            "sendAt": 1_013,
            "responseAt": 1_093,
            "completeAt": 1_095,
            "acceptedAt": 1_096,
            "fillAt": 1_120,
            "fillHandledAt": 1_121,
            "ttfbMs": 79.5
        }));

        assert_eq!(Phase::Delivery.measure(&t), Some(4.0));
        assert_eq!(Phase::Decision.measure(&t), Some(6.0));
        assert_eq!(Phase::Commit.measure(&t), Some(2.0));
        assert_eq!(Phase::Submit.measure(&t), Some(1.0));
        assert_eq!(Phase::Ttfb.measure(&t), Some(79.5));
        assert_eq!(Phase::Body.measure(&t), Some(2.0));
        assert_eq!(Phase::Settle.measure(&t), Some(1.0));
        assert_eq!(Phase::FillWait.measure(&t), Some(24.0));
        assert_eq!(Phase::FillHandle.measure(&t), Some(1.0));
        assert_eq!(Phase::Total.measure(&t), Some(121.0));
    }

    #[test]
    fn ttfb_falls_back_to_the_wall_clock_delta() {
        let t = timings(json!({
            "signalAt": 10,
            "sendAt": 20,
            "responseAt": 60
        }));
        assert_eq!(Phase::Ttfb.measure(&t), Some(40.0));
    }

    #[test]
    fn missing_or_backwards_marks_become_none() {
        let t = timings(json!({ "signalAt": 500, "handleAt": 400 }));
        assert_eq!(Phase::Delivery.measure(&t), None);
        assert_eq!(Phase::FillWait.measure(&t), None);
        assert_eq!(Phase::Total.measure(&t), None);
    }

    #[test]
    fn percentile_and_jitter_match_the_latency_tracker() {
        let stats = stage_stats(&[10.0, 20.0, 30.0, 40.0], Some(40.0));
        assert_eq!(stats["count"], 4);
        assert_eq!(stats["min"], 10.0);
        assert_eq!(stats["max"], 40.0);
        assert_eq!(stats["avg"], 25.0);
        assert_eq!(stats["p50"], 30.0);
        assert_eq!(stats["p95"], 40.0);
        assert_eq!(stats["last"], 40.0);
        assert_eq!(stats["jitter"], 11.18);
    }

    #[test]
    fn snapshot_counts_outcomes_and_returns_preset_windows() {
        let samples = vec![
            sample(
                3_000,
                "filled",
                json!({ "signalAt": 1_000, "acceptedAt": 1_040, "fillAt": 1_050, "fillHandledAt": 1_060 }),
            ),
            sample(
                2_000,
                "failed",
                json!({ "signalAt": 1_000, "completeAt": 1_020, "failedAt": 1_030 }),
            ),
            sample(
                1_000,
                "filled",
                json!({ "signalAt": 1_000, "acceptedAt": 1_190, "fillAt": 1_200, "fillHandledAt": 1_210 }),
            ),
        ];
        let value = snapshot(&samples, 4_000);

        assert_eq!(value["available"], 3);
        assert_eq!(value["totals"]["orders"], 3);
        assert_eq!(value["totals"]["filled"], 2);
        assert_eq!(value["totals"]["failed"], 1);
        assert_eq!(value["totals"]["successRate"], 66.7);
        assert_eq!(value["lastOrderAt"], 3_000);

        // The 25-order window only has the three samples we supplied.
        assert_eq!(value["windows"]["25"]["samples"], 3);
        assert_eq!(value["windows"]["25"]["stages"]["total"]["count"], 3);
        assert_eq!(value["windows"]["25"]["stages"]["fillWait"]["count"], 2);

        // Series is oldest first so the chart reads left to right.
        assert_eq!(value["series"][0]["at"], 1_000);
        assert_eq!(value["series"][2]["at"], 3_000);
        assert_eq!(value["series"][0]["status"], "filled");
    }

    #[test]
    fn empty_snapshot_has_zeroed_totals() {
        let value = empty(1_234);
        assert_eq!(value["available"], 0);
        assert_eq!(value["totals"]["orders"], 0);
        assert!(value["totals"]["successRate"].is_null());
        assert_eq!(value["windows"]["25"]["stages"]["total"]["count"], 0);
        assert!(value["series"].as_array().unwrap().is_empty());
    }

    #[test]
    fn recent_averages_only_the_newest_n_orders() {
        // `recent` takes the newest `count` samples; `samples` is newest-first.
        let samples = vec![
            sample(
                10_000,
                "filled",
                json!({ "signalAt": 1_000, "fillHandledAt": 1_100 }),
            ),
            sample(
                9_000,
                "failed",
                json!({ "signalAt": 1_000, "failedAt": 1_300 }),
            ),
            sample(
                8_000,
                "filled",
                json!({ "signalAt": 1_000, "fillHandledAt": 1_900 }),
            ),
        ];
        let value = recent(&samples, 2);
        assert_eq!(value["count"], 2);
        assert_eq!(value["until"], 10_000);
        assert_eq!(value["since"], 9_000);
        assert_eq!(value["orders"], 2);
        assert_eq!(value["filled"], 1);
        assert_eq!(value["failed"], 1);
        assert_eq!(value["count"], 2);
        assert_eq!(value["avg"], 200.0);
        assert_eq!(value["p95"], 300.0);
    }

    #[test]
    fn recent_ignores_age_and_uses_every_available_order() {
        // Count-based: even very old orders count when fewer than `count`
        // exist; there is no wall-clock window to fall out of.
        let samples = vec![
            sample(
                1_000,
                "filled",
                json!({ "signalAt": 1_000, "fillHandledAt": 1_100 }),
            ),
            sample(
                500,
                "filled",
                json!({ "signalAt": 1_000, "fillHandledAt": 1_300 }),
            ),
            sample(
                100,
                "filled",
                json!({ "signalAt": 1_000, "fillHandledAt": 1_900 }),
            ),
        ];
        let value = recent(&samples, RECENT_ORDERS);
        assert_eq!(value["count"], RECENT_ORDERS);
        assert_eq!(value["until"], 1_000);
        assert_eq!(value["since"], 100);
        assert_eq!(value["orders"], 3);
        assert_eq!(value["avg"], 433.33);
    }

    #[test]
    fn bucket_keys_resolve_to_durations() {
        assert_eq!(bucket_ms_from_key("5m"), Some(300_000));
        assert_eq!(bucket_ms_from_key("1h"), Some(3_600_000));
        assert_eq!(bucket_ms_from_key("1d"), Some(86_400_000));
        assert_eq!(bucket_ms_from_key("7m"), None);
    }

    #[test]
    fn history_buckets_orders_and_keeps_a_range_summary() {
        let base = 1_700_000_000_000i64;
        let samples = vec![
            sample(
                base,
                "filled",
                json!({ "signalAt": 1_000, "fillHandledAt": 1_100 }),
            ),
            sample(
                base + 10_000,
                "failed",
                json!({ "signalAt": 1_000, "failedAt": 1_300 }),
            ),
            sample(
                base + 70_000,
                "filled",
                json!({ "signalAt": 1_000, "fillHandledAt": 1_200 }),
            ),
        ];
        let value = history(&samples, base - 60_000, base + 120_000, 60_000, 0, "total");

        assert_eq!(value["stage"], "total");
        assert_eq!(value["summary"]["orders"], 3);
        assert_eq!(value["summary"]["filled"], 2);
        assert_eq!(value["summary"]["failed"], 1);
        assert_eq!(value["summary"]["buckets"], 2);
        assert_eq!(value["summary"]["avg"], 200.0);
        assert_eq!(value["summary"]["p50"], 200.0);
        assert_eq!(value["summary"]["p95"], 300.0);
        assert_eq!(value["summary"]["jitter"], 81.65);

        let points = value["points"].as_array().unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0]["n"], 2);
        assert_eq!(points[0]["filled"], 1);
        assert_eq!(points[0]["failed"], 1);
        assert_eq!(points[0]["avg"], 200.0);
        assert_eq!(points[0]["p95"], 300.0);
        assert_eq!(points[1]["n"], 1);
        assert_eq!(points[1]["avg"], 200.0);

        // The header summary is the newest bucket's stats, so it is *not*
        // the whole-range average and differs when the bucket size changes.
        assert_eq!(value["bucketSummary"]["n"], 1);
        assert_eq!(value["bucketSummary"]["avg"], 200.0);
        // `align` buckets onto the clock: the two samples at base and
        // base+10s land in the bucket that starts at base - (base mod 60s),
        // and the sample at base+70s lands in the next 60s bucket — so the
        // newest bucket's start is `base + 60_000 - (base mod 60_000)`.
        let aligned_first = base - (base % 60_000);
        assert_eq!(value["newestBucketAt"], aligned_first + 60_000);
        assert_eq!(value["oldestBucketAt"], aligned_first);
    }

    #[test]
    fn history_aligns_buckets_to_the_viewer_clock() {
        let base = 1_700_000_000_000i64;
        let samples = vec![sample(
            base,
            "filled",
            json!({ "signalAt": 1_000, "fillHandledAt": 1_100 }),
        )];
        let utc = history(
            &samples,
            base - 86_400_000,
            base + 86_400_000,
            86_400_000,
            0,
            "total",
        );
        let jakarta = history(
            &samples,
            base - 86_400_000,
            base + 86_400_000,
            86_400_000,
            7 * 3_600_000,
            "total",
        );
        let utc_at = utc["points"][0]["at"].as_i64().unwrap();
        let jakarta_at = jakarta["points"][0]["at"].as_i64().unwrap();
        assert_eq!(utc_at % 86_400_000, 0);
        assert_eq!((jakarta_at + 7 * 3_600_000) % 86_400_000, 0);
        assert_ne!(utc_at, jakarta_at);
    }

    #[test]
    fn history_can_chart_another_stage_and_drops_out_of_range_rows() {
        let base = 1_700_000_000_000i64;
        let samples = vec![
            sample(
                base - 120_000,
                "filled",
                json!({ "signalAt": 1_000, "ttfbMs": 999 }),
            ),
            sample(base, "filled", json!({ "signalAt": 1_000, "ttfbMs": 50 })),
            sample(
                base + 10_000,
                "filled",
                json!({ "signalAt": 1_000, "ttfbMs": 70 }),
            ),
        ];
        let value = history(&samples, base, base + 60_000, 60_000, 0, "ttfb");
        assert_eq!(value["stage"], "ttfb");
        assert_eq!(value["summary"]["orders"], 2);
        assert_eq!(value["summary"]["avg"], 60.0);
        assert_eq!(value["points"][0]["avg"], 60.0);
    }
}
