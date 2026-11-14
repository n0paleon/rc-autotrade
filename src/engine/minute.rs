use std::collections::BTreeMap;

use crate::constants::{PRICE_BUCKET_MS, PRICE_SCALE, PRICE_SETTLE_DWELL_MS};

use super::reconcile::price_key;

/// One minute's robust close.
///
/// The stored series used to take the last write of a minute as its close, so a
/// single transient print — a one-unit bait listing, a cancelled ask — redrew
/// the whole chart and then propagated into every coarser bucket. This
/// aggregator instead weighs each price by how long it actually held and
/// reports the duration-weighted median: a price seen for less than half the
/// minute cannot move the close, while a real move wins as soon as it holds.
///
/// A [`PRICE_SETTLE_DWELL_MS`] debounce runs first, so a print that vanishes
/// within a few seconds never enters the weights at all. The guard only shapes
/// the chart and the sampler; order decisions still read the raw best ask.
#[derive(Debug, Clone)]
pub(super) struct MinuteAgg {
    pub(super) bucket: i64,
    /// Price that currently rules the minute.
    pub(super) settled: f64,
    /// Instant up to which `settled` has been accrued into `weights`.
    accrued_to: i64,
    /// A different price waiting for the dwell guard to accept it.
    candidate: Option<(f64, i64)>,
    /// Micro-RLT price -> milliseconds that price held.
    weights: BTreeMap<i64, i64>,
    /// Lowest and highest price that actually held this minute. Transient
    /// prints rejected by the dwell guard are excluded on purpose.
    pub(super) low: f64,
    pub(super) high: f64,
    /// Lowest and highest print seen this minute, before the dwell guard. This
    /// is the raw band: it does include one-frame bait, so the chart can show
    /// it only when the viewer asks for it.
    pub(super) raw_low: f64,
    pub(super) raw_high: f64,
}

impl MinuteAgg {
    pub(super) fn start(bucket: i64, price: f64, at: i64) -> Self {
        Self {
            bucket,
            settled: price,
            accrued_to: at.clamp(bucket, bucket + PRICE_BUCKET_MS),
            candidate: None,
            weights: BTreeMap::new(),
            low: price,
            high: price,
            raw_low: price,
            raw_high: price,
        }
    }

    /// Records the price seen at `at` and returns the running minute close.
    pub(super) fn observe(&mut self, at: i64, price: f64) -> f64 {
        let at = at.clamp(self.bucket, self.bucket + PRICE_BUCKET_MS);
        self.accrue(at);
        // The raw band sees every print, including a candidate that may never
        // pass the dwell guard.
        self.raw_low = self.raw_low.min(price);
        self.raw_high = self.raw_high.max(price);
        if price_key(price) != price_key(self.settled) {
            match self.candidate {
                Some((candidate, since)) if price_key(candidate) == price_key(price) => {
                    if at - since >= PRICE_SETTLE_DWELL_MS {
                        self.settled = price;
                        self.candidate = None;
                        self.low = self.low.min(price);
                        self.high = self.high.max(price);
                    }
                }
                _ => self.candidate = Some((price, at)),
            }
        } else {
            self.candidate = None;
        }
        self.median()
    }

    /// Accrues the settled price up to `until`, but never past the bucket end.
    fn accrue(&mut self, until: i64) {
        let until = until.clamp(self.bucket, self.bucket + PRICE_BUCKET_MS);
        if until > self.accrued_to {
            *self.weights.entry(price_key(self.settled)).or_insert(0) += until - self.accrued_to;
            self.accrued_to = until;
        }
    }

    /// Cheapest price whose duration reaches half the bucket. On an exact tie
    /// the lower price wins, so a close is never biased upward by rounding.
    fn median(&self) -> f64 {
        let total: i64 = self.weights.values().sum();
        if total <= 0 {
            return self.settled;
        }
        let mut cumulative = 0i64;
        for (key, duration) in &self.weights {
            cumulative += *duration;
            // Reaching half is enough, so an exact tie resolves to the cheaper
            // price instead of rounding the close upward.
            if cumulative * 2 >= total {
                return *key as f64 / PRICE_SCALE;
            }
        }
        self.settled
    }

    /// Final close for the bucket, including the tail after the last frame.
    pub(super) fn close(&mut self) -> f64 {
        self.accrue(self.bucket + PRICE_BUCKET_MS);
        self.median()
    }

    /// Running close without accepting a new price, for the quiet-minute
    /// sampler: it carries the settled level instead of inventing a tick.
    pub(super) fn value(&mut self, at: i64) -> f64 {
        self.accrue(at);
        self.median()
    }
}
