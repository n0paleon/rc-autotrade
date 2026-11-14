//! Local-clock bucket aggregation for the benchmark history chart.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::stats::stage_stats;
use super::{Phase, Sample};

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
    let bucket_summary = |at: i64| points.iter().find(|point| point["at"] == at).cloned();
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
