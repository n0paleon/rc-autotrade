//! Per-stage percentile and jitter summaries.

use serde_json::{Map, Value, json};

use super::{Phase, Sample, round2};

/// `count/last/min/max/avg/p50/p95/jitter` for one stage, mirroring the shape
/// the latency tracker returns so the deck can share its formatting helpers.
pub(super) fn stage_stats(sorted: &[f64], last: Option<f64>) -> Value {
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
pub(super) fn summarize(samples: &[Sample]) -> Value {
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
