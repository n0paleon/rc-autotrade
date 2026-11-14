use std::collections::HashMap;

use crate::constants::{
    MARKET_SNAPSHOT_THROTTLE_MS, PRICE_BUCKET_MS, PRICE_HISTORY_LIMIT,
    PRICE_POINT_WRITE_INTERVAL_MS,
};
use crate::depth::Fill;
use crate::model::{MarketSnapshot, PriceBand, Target};
use crate::store::Store;
use crate::trend::Point;

use super::minute::MinuteAgg;
use super::{Engine, PriceSample};

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn persist_market(
        &mut self,
        store: &Store,
        config: &Target,
        now: i64,
        price: f64,
        low: f64,
        high: f64,
        raw_low: f64,
        raw_high: f64,
        fill: Option<Fill>,
        bucket: i64,
        band_changed: bool,
        finalized: Option<(i64, f64, f64, f64, f64, f64)>,
    ) {
        let (persisted_at, point_write_at, last_bucket) = {
            let runtime = self.runtime_for(&config.item_id, config.desired_qty);
            (
                runtime.persisted_at,
                runtime.point_write_at,
                runtime.last_point_bucket,
            )
        };

        // A frame that carries the marketplace band forces a snapshot regardless
        // of the throttle; ordinary ticks are throttled to one write per window.
        if band_changed || now - persisted_at >= MARKET_SNAPSHOT_THROTTLE_MS {
            let snapshot = {
                let runtime = self.runtime_for(&config.item_id, config.desired_qty);
                MarketSnapshot {
                    item_id: config.item_id.clone(),
                    last_price: runtime.last_price,
                    last_stock: runtime.last_stock,
                    allowed_price_min: runtime.allowed_price_min,
                    allowed_price_max: runtime.allowed_price_max,
                    allowed_at: runtime.allowed_at,
                    item_type: runtime.item_type.clone(),
                    seen_at: runtime.seen_at.unwrap_or(now),
                }
            };
            if let Err(error) = store.save_market_snapshot(&snapshot) {
                let runtime = self.runtime_for(&config.item_id, config.desired_qty);
                if now - runtime.persist_error_at > 60_000 {
                    runtime.persist_error_at = now;
                    self.logger.warn(
                        "engine",
                        format!("could not persist market snapshot: {error}"),
                    );
                }
            } else {
                self.runtime_for(&config.item_id, config.desired_qty)
                    .persisted_at = now;
            }
        }

        // The minute that just ended is written with its exact median instead
        // of the running value its last frame happened to see. `record_price_samples`
        // only touches the close and band, so a fill recorded in that bucket
        // survives.
        if let Some((old_bucket, old_close, old_low, old_high, old_raw_low, old_raw_high)) =
            finalized
            && let Err(error) = store.record_price_samples(&[(
                &config.item_id,
                old_bucket,
                old_close,
                old_low,
                old_high,
                old_raw_low,
                old_raw_high,
            )])
        {
            let runtime = self.runtime_for(&config.item_id, config.desired_qty);
            if now - runtime.persist_error_at > 60_000 {
                runtime.persist_error_at = now;
                self.logger
                    .warn("engine", format!("could not finalise price point: {error}"));
            }
        }

        if last_bucket != Some(bucket) || now - point_write_at >= PRICE_POINT_WRITE_INTERVAL_MS {
            let fill_tuple = fill.map(|fill| (fill.price, fill.filled as f64, fill.approx));
            let band = PriceBand {
                low,
                high,
                raw_low,
                raw_high,
            };
            if let Err(error) =
                store.record_price_point(&config.item_id, bucket, price, band, fill_tuple)
            {
                let runtime = self.runtime_for(&config.item_id, config.desired_qty);
                if now - runtime.persist_error_at > 60_000 {
                    runtime.persist_error_at = now;
                    self.logger
                        .warn("engine", format!("could not record price point: {error}"));
                }
            } else {
                let runtime = self.runtime_for(&config.item_id, config.desired_qty);
                runtime.last_point_bucket = Some(bucket);
                runtime.point_write_at = now;
            }
        }
    }

    /// Rebuilds the display cache from SQLite after a restart.
    pub fn hydrate_market(&mut self, store: &Store) -> usize {
        let snapshots = store.load_market_snapshots();
        let mut restored = 0;
        for snapshot in snapshots {
            if snapshot.seen_at <= 0 {
                continue;
            }
            let Some(config) = store.find_config(&snapshot.item_id).cloned() else {
                continue;
            };
            let now = self.now();
            let desired_qty = config.desired_qty;
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            if snapshot.last_price.is_some() {
                runtime.last_price = snapshot.last_price;
            }
            if snapshot.last_stock.is_some() {
                runtime.last_stock = snapshot.last_stock;
            }
            if snapshot.allowed_price_min.is_some() {
                runtime.allowed_price_min = snapshot.allowed_price_min;
            }
            if snapshot.allowed_price_max.is_some() {
                runtime.allowed_price_max = snapshot.allowed_price_max;
            }
            runtime.allowed_at = snapshot.allowed_at;
            runtime.item_type = snapshot
                .item_type
                .clone()
                .or_else(|| runtime.item_type.clone())
                .or_else(|| Some(config.item_type.clone()));
            runtime.seen_at = Some(snapshot.seen_at);
            runtime.hydrated_at = Some(now);
            runtime.stale = true;

            let stored = store.load_price_history(&config.item_id, PRICE_HISTORY_LIMIT, 0);
            if !stored.is_empty() {
                let mut merged: HashMap<i64, f64> = runtime
                    .history
                    .iter()
                    .map(|point| (point.t, point.p))
                    .collect();
                for point in &stored {
                    merged.insert(point.bucket_at, point.price);
                }
                let mut points: Vec<Point> =
                    merged.into_iter().map(|(t, p)| Point { t, p }).collect();
                points.sort_by_key(|point| point.t);
                if points.len() > PRICE_HISTORY_LIMIT {
                    points.drain(0..points.len() - PRICE_HISTORY_LIMIT);
                }
                runtime.history = points;

                let fill_points: Vec<Point> = stored
                    .iter()
                    .filter_map(|point| {
                        point.fill_price.map(|price| Point {
                            t: point.bucket_at,
                            p: price,
                        })
                    })
                    .collect();
                if !fill_points.is_empty() {
                    let mut merged_fill: HashMap<i64, f64> = runtime
                        .fill_history
                        .iter()
                        .map(|point| (point.t, point.p))
                        .collect();
                    for point in &fill_points {
                        merged_fill.insert(point.t, point.p);
                    }
                    let mut points: Vec<Point> = merged_fill
                        .into_iter()
                        .map(|(t, p)| Point { t, p })
                        .collect();
                    points.sort_by_key(|point| point.t);
                    if points.len() > PRICE_HISTORY_LIMIT {
                        points.drain(0..points.len() - PRICE_HISTORY_LIMIT);
                    }
                    runtime.fill_history = points;

                    if let Some(latest) = stored.last()
                        && let Some(fill_price) = latest.fill_price
                    {
                        runtime.last_fill = Some(Fill {
                            price: fill_price,
                            filled: latest.fill_qty.unwrap_or(1.0) as i64,
                            requested: desired_qty,
                            partial: false,
                            approx: latest.fill_approx,
                            levels: 0,
                        });
                    }
                    runtime.fill_qty_for = desired_qty;
                }
            }

            let runtime = self.runtime_for(&config.item_id, desired_qty);
            Self::compute_trend(&config, runtime, now, true);
            restored += 1;
        }

        if restored > 0 {
            self.logger.info(
                "engine",
                format!(
                    "restored {restored} cached market snapshots from sqlite — waiting for live frames"
                ),
            );
        }
        restored
    }

    /// Writes every known snapshot once, used on graceful shutdown.
    pub fn flush_market_cache(&mut self, store: &Store) {
        let rows: Vec<MarketSnapshot> = self
            .runtime
            .iter()
            .filter_map(|(item_id, runtime)| {
                let seen_at = runtime.seen_at.or(runtime.last_seen_at)?;
                store.find_config(item_id)?;
                Some(MarketSnapshot {
                    item_id: item_id.clone(),
                    last_price: runtime.last_price,
                    last_stock: runtime.last_stock,
                    allowed_price_min: runtime.allowed_price_min,
                    allowed_price_max: runtime.allowed_price_max,
                    allowed_at: runtime.allowed_at,
                    item_type: runtime.item_type.clone(),
                    seen_at,
                })
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        if let Err(error) = store.save_market_snapshots(&rows) {
            self.logger
                .warn("engine", format!("could not persist market cache: {error}"));
        }
    }

    /// Writes one close per target per minute, even when that item's own frames
    /// are quiet.
    ///
    /// The feed is change-driven: an item whose book does not move sends no
    /// frame, which used to read as "no data" on the chart. Once a target has
    /// been seen live in this process, its last price is sampled every minute so
    /// the line stays flat instead of breaking.
    ///
    /// `feed_alive` is false when the upstream itself is down. Nothing is
    /// written then, because a real outage must stay a visible gap rather than
    /// a fabricated flat line.
    pub fn sample_prices(
        &mut self,
        store: &mut Store,
        at: i64,
        feed_alive: bool,
    ) -> Vec<PriceSample> {
        if !feed_alive {
            return Vec::new();
        }
        let bucket = at.div_euclid(PRICE_BUCKET_MS) * PRICE_BUCKET_MS;
        let item_ids: Vec<String> = store
            .state
            .configs
            .iter()
            .map(|config| config.item_id.clone())
            .collect();
        let mut samples = Vec::new();
        let mut finalized = Vec::new();
        for item_id in item_ids {
            let Some(runtime) = self.runtime.get_mut(&item_id.to_ascii_lowercase()) else {
                continue;
            };
            // Never seen by a live frame since boot: sampling would bridge the
            // downtime this process just came out of.
            if runtime.live_seen_at.is_none() {
                continue;
            }
            let Some(last_price) = runtime.last_price else {
                continue;
            };

            // The frame stream may have gone quiet before the minute rolled:
            // finish it here so its stored close is the exact median, then
            // carry the settled level into the new minute.
            if let Some(mut agg) = runtime.minute.take() {
                if agg.bucket < bucket {
                    let old_bucket = agg.bucket;
                    let old_close = agg.close();
                    finalized.push((
                        item_id.clone(),
                        old_bucket,
                        old_close,
                        agg.low,
                        agg.high,
                        agg.raw_low,
                        agg.raw_high,
                    ));
                    if let Some(point) = runtime.history.iter_mut().find(|p| p.t == old_bucket) {
                        point.p = old_close;
                    }
                    let mut next = MinuteAgg::start(bucket, agg.settled, bucket);
                    next.value(at);
                    runtime.minute = Some(next);
                } else if agg.bucket == bucket {
                    runtime.minute = Some(agg);
                } else {
                    let mut next = MinuteAgg::start(bucket, last_price, bucket);
                    next.value(at);
                    runtime.minute = Some(next);
                }
            } else {
                let mut next = MinuteAgg::start(bucket, last_price, bucket);
                next.value(at);
                runtime.minute = Some(next);
            }

            // A frame already owns this minute; the sampler must not overwrite
            // a fresher close with the previous one.
            if matches!(runtime.history.last(), Some(point) if point.t >= bucket) {
                continue;
            }
            let (price, low, high, raw_low, raw_high) = match runtime.minute.as_mut() {
                Some(agg) => {
                    let price = agg.value(at);
                    (price, agg.low, agg.high, agg.raw_low, agg.raw_high)
                }
                None => (last_price, last_price, last_price, last_price, last_price),
            };
            runtime.history.push(Point {
                t: bucket,
                p: price,
            });
            if runtime.history.len() > PRICE_HISTORY_LIMIT {
                let excess = runtime.history.len() - PRICE_HISTORY_LIMIT;
                runtime.history.drain(0..excess);
            }
            samples.push(PriceSample {
                item_id,
                bucket,
                price,
                low,
                high,
                raw_low,
                raw_high,
            });
        }
        let mut rows: Vec<(&str, i64, f64, f64, f64, f64, f64)> = finalized
            .iter()
            .map(
                |(item_id, bucket_at, price, low, high, raw_low, raw_high)| {
                    (
                        item_id.as_str(),
                        *bucket_at,
                        *price,
                        *low,
                        *high,
                        *raw_low,
                        *raw_high,
                    )
                },
            )
            .collect();
        rows.extend(samples.iter().map(|sample| {
            (
                sample.item_id.as_str(),
                sample.bucket,
                sample.price,
                sample.low,
                sample.high,
                sample.raw_low,
                sample.raw_high,
            )
        }));
        if let Err(error) = store.record_price_samples(&rows)
            && at - self.sample_error_at > 60_000
        {
            self.sample_error_at = at;
            self.logger
                .warn("engine", format!("could not record price samples: {error}"));
        }
        samples
    }
}
