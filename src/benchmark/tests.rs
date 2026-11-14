use serde_json::{Value, json};

use super::stats::stage_stats;
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
    // The fill wait starts at the monotonic TTFB boundary (1013 + 79.5),
    // not at the wall-clock response mark (1093).
    assert_eq!(Phase::FillWait.measure(&t), Some(27.5));
    assert_eq!(Phase::FillHandle.measure(&t), Some(1.0));
    assert_eq!(Phase::Total.measure(&t), Some(121.0));
}

#[test]
fn fill_wait_is_anchored_to_the_purchase_request_ttfb() {
    let t = timings(json!({
        "sendAt": 1_000,
        "responseAt": 1_120,
        "ttfbMs": 100.0,
        "fillAt": 1_300
    }));
    assert_eq!(Phase::FillWait.measure(&t), Some(200.0));
}

#[test]
fn fill_wait_keeps_the_retry_response_anchor() {
    // A 401 retry stores the retry's TTFB while sendAt stays on the first
    // dispatch, so sendAt + ttfbMs is not the boundary the retry responded
    // at. The retry's own response mark is.
    let t = timings(json!({
        "sendAt": 1_000,
        "responseAt": 1_200,
        "ttfbMs": 80.0,
        "attempts": 2,
        "fillAt": 1_300
    }));
    assert_eq!(Phase::FillWait.measure(&t), Some(100.0));
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
fn fill_wait_without_a_ttfb_sample_uses_the_response_mark() {
    let t = timings(json!({
        "responseAt": 1_100,
        "fillAt": 1_180
    }));
    assert_eq!(Phase::FillWait.measure(&t), Some(80.0));
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
            json!({ "signalAt": 1_000, "responseAt": 1_010, "acceptedAt": 1_040, "fillAt": 1_050, "fillHandledAt": 1_060 }),
        ),
        sample(
            2_000,
            "failed",
            json!({ "signalAt": 1_000, "completeAt": 1_020, "failedAt": 1_030 }),
        ),
        sample(
            1_000,
            "filled",
            json!({ "signalAt": 1_000, "responseAt": 1_050, "acceptedAt": 1_190, "fillAt": 1_200, "fillHandledAt": 1_210 }),
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
