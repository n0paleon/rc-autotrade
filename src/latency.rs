//! Latency tracker, ported from `src/worker/latency.js`.
//!
//! Four passive channels — `http`, `keepalive`, `wsHandshake`, `wsPing` — each
//! keeping a bounded rolling sample so the deck can show last/avg/p50/p95/min/
//! max/jitter without persisting anything to disk.

use std::collections::VecDeque;

use serde_json::{Value, json};

use crate::rules::js_round;

pub const DEFAULT_CAPACITY: usize = 120;
pub const MAX_CAPACITY: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Http,
    Keepalive,
    WsHandshake,
    WsPing,
}

impl Channel {
    pub const ALL: [Channel; 4] = [
        Channel::Http,
        Channel::Keepalive,
        Channel::WsHandshake,
        Channel::WsPing,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Channel::Http => "http",
            Channel::Keepalive => "keepalive",
            Channel::WsHandshake => "wsHandshake",
            Channel::WsPing => "wsPing",
        }
    }

    fn index(self) -> usize {
        match self {
            Channel::Http => 0,
            Channel::Keepalive => 1,
            Channel::WsHandshake => 2,
            Channel::WsPing => 3,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Bucket {
    samples: VecDeque<f64>,
    last_at: Option<i64>,
    last_ok: Option<bool>,
    errors: u64,
    last_error: Option<String>,
}

pub type Clock = std::sync::Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct LatencyTracker {
    capacity: usize,
    buckets: [Bucket; 4],
    clock: Clock,
}

impl std::fmt::Debug for LatencyTracker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LatencyTracker")
            .field("capacity", &self.capacity)
            .field(
                "counts",
                &self
                    .buckets
                    .iter()
                    .map(|bucket| bucket.samples.len())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

fn round1(value: f64) -> f64 {
    js_round(value * 10.0) / 10.0
}

fn clamp_sample(value: f64) -> Option<f64> {
    if value.is_finite() && value >= 0.0 {
        Some(js_round(value * 10.0) / 10.0)
    } else {
        None
    }
}

impl LatencyTracker {
    pub fn new(capacity: usize, clock: Clock) -> Self {
        let capacity = if capacity == 0 {
            DEFAULT_CAPACITY
        } else {
            capacity.min(MAX_CAPACITY)
        };
        Self {
            capacity,
            buckets: Default::default(),
            clock,
        }
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    /// Record one completed sample. `ok == false` marks a failed call.
    pub fn record(&mut self, channel: Channel, ms: f64, ok: bool, error: Option<&str>) -> bool {
        let Some(value) = clamp_sample(ms) else {
            return false;
        };
        let recorded_at = self.now();
        let bucket = &mut self.buckets[channel.index()];
        bucket.samples.push_back(value);
        while bucket.samples.len() > self.capacity {
            bucket.samples.pop_front();
        }
        bucket.last_at = Some(recorded_at);
        bucket.last_ok = Some(ok);
        if !ok {
            bucket.errors += 1;
            bucket.last_error = error.map(|text| text.chars().take(160).collect());
        }
        true
    }

    fn channel_stats(bucket: &Bucket) -> Value {
        let count = bucket.samples.len();
        if count == 0 {
            return json!({
                "count": 0,
                "last": null,
                "lastAt": null,
                "lastOk": null,
                "min": null,
                "max": null,
                "avg": null,
                "p50": null,
                "p95": null,
                "jitter": null,
                "errors": 0,
                "lastError": null,
                "recent": [],
            });
        }

        let mut sorted: Vec<f64> = bucket.samples.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let sum: f64 = sorted.iter().sum();
        let avg = sum / count as f64;
        let variance = sorted
            .iter()
            .map(|value| (value - avg).powi(2))
            .sum::<f64>()
            / count as f64;
        let at = |q: f64| sorted[((q * count as f64) as usize).min(count - 1)];
        let recent: Vec<f64> = bucket
            .samples
            .iter()
            .rev()
            .take(24)
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        json!({
            "count": count,
            "last": bucket.samples.back(),
            "lastAt": bucket.last_at,
            "lastOk": bucket.last_ok,
            "min": round1(sorted[0]),
            "max": round1(sorted[count - 1]),
            "avg": round1(avg),
            "p50": round1(at(0.5)),
            "p95": round1(at(0.95)),
            "jitter": round1(variance.sqrt()),
            "errors": bucket.errors,
            "lastError": bucket.last_error,
            "recent": recent,
        })
    }

    pub fn stats(&self) -> Value {
        let mut out = serde_json::Map::new();
        for channel in Channel::ALL {
            out.insert(
                channel.key().to_string(),
                Self::channel_stats(&self.buckets[channel.index()]),
            );
        }
        Value::Object(out)
    }

    /// Freshest recent sample across channels (for the HUD chip).
    pub fn freshest(&self) -> Value {
        let mut best: Option<(Channel, &Bucket)> = None;
        for channel in Channel::ALL {
            let bucket = &self.buckets[channel.index()];
            let Some(last_at) = bucket.last_at else {
                continue;
            };
            match best {
                None => best = Some((channel, bucket)),
                Some((_, current)) => {
                    if current.last_at.unwrap_or(0) < last_at {
                        best = Some((channel, bucket));
                    }
                }
            }
        }
        match best {
            None => Value::Null,
            Some((channel, bucket)) => json!({
                "channel": channel.key(),
                "last": bucket.samples.back(),
                "lastAt": bucket.last_at,
                "lastOk": bucket.last_ok,
            }),
        }
    }

    pub fn snapshot(&self) -> Value {
        json!({
            "channels": self.stats(),
            "freshest": self.freshest(),
            "measuredAt": self.now(),
        })
    }

    pub fn reset(&mut self) {
        self.buckets = Default::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn tracker() -> (LatencyTracker, Arc<AtomicI64>) {
        let now = Arc::new(AtomicI64::new(1_000));
        let clock = now.clone();
        (
            LatencyTracker::new(
                DEFAULT_CAPACITY,
                Arc::new(move || clock.load(Ordering::SeqCst)),
            ),
            now,
        )
    }

    #[test]
    fn records_samples_and_computes_percentiles() {
        let (mut tracker, now) = tracker();
        for value in [10.0, 20.0, 30.0, 40.0] {
            assert!(tracker.record(Channel::Http, value, true, None));
            now.fetch_add(10, Ordering::SeqCst);
        }
        let stats = tracker.stats();
        let http = &stats["http"];
        assert_eq!(http["count"], 4);
        assert_eq!(http["min"], 10.0);
        assert_eq!(http["max"], 40.0);
        assert_eq!(http["avg"], 25.0);
        assert_eq!(http["p50"], 30.0); // floor(0.5 * 4) = index 2
        assert_eq!(http["p95"], 40.0);
        assert_eq!(http["last"], 40.0);
    }

    #[test]
    fn bounds_the_ring_buffer() {
        let (mut tracker, _) = tracker();
        let small = LatencyTracker::new(3, tracker.clock.clone());
        tracker = small;
        for value in [1.0, 2.0, 3.0, 4.0, 5.0] {
            tracker.record(Channel::WsPing, value, true, None);
        }
        let stats = tracker.stats();
        assert_eq!(stats["wsPing"]["count"], 3);
        assert_eq!(stats["wsPing"]["last"], 5.0);
        assert_eq!(stats["wsPing"]["min"], 3.0);
    }

    #[test]
    fn failures_are_counted_with_a_redacted_error() {
        let (mut tracker, _) = tracker();
        tracker.record(
            Channel::WsHandshake,
            250.0,
            false,
            Some("TLS handshake failed"),
        );
        let stats = tracker.stats();
        assert_eq!(stats["wsHandshake"]["errors"], 1);
        assert_eq!(stats["wsHandshake"]["lastOk"], false);
        assert_eq!(stats["wsHandshake"]["lastError"], "TLS handshake failed");
    }

    #[test]
    fn ignores_invalid_samples() {
        let (mut tracker, _) = tracker();
        assert!(!tracker.record(Channel::Http, f64::NAN, true, None));
        assert!(!tracker.record(Channel::Http, -5.0, true, None));
        assert_eq!(tracker.stats()["http"]["count"], 0);
    }

    #[test]
    fn freshest_picks_the_most_recent_channel() {
        let (mut tracker, now) = tracker();
        tracker.record(Channel::Http, 100.0, true, None);
        now.fetch_add(500, Ordering::SeqCst);
        tracker.record(Channel::WsPing, 25.0, true, None);
        let freshest = tracker.freshest();
        assert_eq!(freshest["channel"], "wsPing");
        assert_eq!(freshest["last"], 25.0);
    }

    #[test]
    fn reset_clears_every_channel() {
        let (mut tracker, _) = tracker();
        tracker.record(Channel::Http, 100.0, false, Some("boom"));
        tracker.reset();
        let stats = tracker.stats();
        assert_eq!(stats["http"]["count"], 0);
        assert_eq!(stats["http"]["errors"], 0);
        assert!(tracker.freshest().is_null());
    }
}
