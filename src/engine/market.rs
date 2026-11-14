use serde_json::Value;

use crate::constants::PRICE_HISTORY_LIMIT;
use crate::depth::estimate_fill_price_sorted;
use crate::events::{Kind, UpstreamEvent, normalize_upstream_event};
use crate::model::Target;
use crate::rules::{Action, Plan, plan_order};
use crate::store::Store;
use crate::trend::Point;

use super::minute::MinuteAgg;
use super::{Engine, EngineEvent, MarketTick, SCAN_LOG_INTERVAL_MS, label_for};

impl Engine {
    pub fn handle_frame(
        &mut self,
        store: &mut Store,
        raw: &Value,
        received_at: i64,
        has_token: bool,
    ) -> Vec<EngineEvent> {
        self.has_token = has_token;
        let event = normalize_upstream_event(raw);
        match event.kind {
            Kind::Market => {
                let Some(item_id) = event.item_id.clone() else {
                    return Vec::new();
                };
                // The item monitor ranks every id the RLT market shows, not
                // just the ones on the target fleet, so the frame is counted
                // before the target lookup below can return early.
                if event
                    .currency
                    .as_deref()
                    .is_none_or(|currency| currency == "RLT")
                {
                    store.note_item_event(&item_id, event.item_type.as_deref(), received_at);
                }
                self.handle_market(store, &event, &item_id, raw, received_at)
            }
            Kind::Fill => self.handle_fill(store, &event, raw, received_at),
            Kind::Other => Vec::new(),
        }
    }

    fn handle_market(
        &mut self,
        store: &mut Store,
        event: &UpstreamEvent,
        item_id: &str,
        raw: &Value,
        received_at: i64,
    ) -> Vec<EngineEvent> {
        let Some(config) = store.find_config(item_id).cloned() else {
            return Vec::new();
        };
        if let Some(currency) = &event.currency
            && currency != "RLT"
        {
            return Vec::new();
        }

        if let Some(item_type) = &event.item_type
            && config.item_type != *item_type
        {
            store.update_item_type(&config.item_id, item_type);
        }

        let Some(best) = event.listings.first().copied() else {
            return Vec::new();
        };

        let now = self.now();
        let signal_at = received_at;
        let handle_at = now;
        let desired_qty = config.desired_qty;
        let label = label_for(Some(&config), &config.item_id);
        let currency = event.currency.as_deref().unwrap_or("RLT");
        let item_type = event
            .item_type
            .clone()
            .unwrap_or_else(|| config.item_type.clone());

        let allowed_min = event.allowed_price_min;
        let allowed_max = event.allowed_price_max;
        let bucket = now.div_euclid(60_000) * 60_000;
        let listings: Vec<crate::depth::Listing> = event
            .listings
            .iter()
            .map(|listing| crate::depth::Listing {
                price: listing.unit_price,
                quantity: listing.quantity,
            })
            .collect();
        let rest = event.rest.map(|rest| crate::depth::Listing {
            price: rest.unit_price,
            quantity: rest.quantity,
        });
        // The normaliser already sorted the ladder cheapest-first, so the depth
        // model can consume it directly instead of copying and sorting again.
        let fill = estimate_fill_price_sorted(&listings, rest, desired_qty);

        let mut events = Vec::new();
        let (
            trend_json,
            allowed_min,
            allowed_max,
            allowed_at,
            close,
            low,
            high,
            raw_low,
            raw_high,
            finalized,
        ) = {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            runtime.last_price = Some(best.unit_price);
            // Raw upstream depth. Kept untouched by fills on purpose: this field
            // feeds the snapshot and the trend's book size, and both must show
            // what the market reported, not a local prediction.
            runtime.last_stock = Some(best.quantity);
            runtime.last_seen_at = Some(now);
            runtime.live_seen_at = Some(now);
            runtime.seen_at = Some(now);
            runtime.stale = false;
            runtime.item_type = Some(item_type.clone());
            if allowed_min.is_some() {
                runtime.allowed_price_min = allowed_min;
            }
            if allowed_max.is_some() {
                runtime.allowed_price_max = allowed_max;
            }
            if allowed_min.is_some() || allowed_max.is_some() {
                runtime.allowed_at = Some(now);
            }

            // The stored close is the duration-weighted median of this minute,
            // not the last frame. Rolling into a new minute finalises the one
            // that just ended so its final write is exact.
            let (close, low, high, raw_low, raw_high, finalized) = match runtime.minute.take() {
                Some(mut agg) if agg.bucket == bucket => {
                    let close = agg.observe(now, best.unit_price);
                    let (low, high) = (agg.low, agg.high);
                    let (raw_low, raw_high) = (agg.raw_low, agg.raw_high);
                    runtime.minute = Some(agg);
                    (close, low, high, raw_low, raw_high, None)
                }
                Some(mut agg) if agg.bucket < bucket => {
                    let old_bucket = agg.bucket;
                    let old_close = agg.close();
                    let finalized = (
                        old_bucket,
                        old_close,
                        agg.low,
                        agg.high,
                        agg.raw_low,
                        agg.raw_high,
                    );
                    // The settled level held from the bucket boundary until
                    // this frame, so the new minute starts there.
                    let mut next = MinuteAgg::start(bucket, agg.settled, bucket);
                    let close = next.observe(now, best.unit_price);
                    let (low, high) = (next.low, next.high);
                    let (raw_low, raw_high) = (next.raw_low, next.raw_high);
                    runtime.minute = Some(next);
                    (close, low, high, raw_low, raw_high, Some(finalized))
                }
                _ => {
                    let mut agg = MinuteAgg::start(bucket, best.unit_price, now);
                    let close = agg.observe(now, best.unit_price);
                    let (low, high) = (agg.low, agg.high);
                    let (raw_low, raw_high) = (agg.raw_low, agg.raw_high);
                    runtime.minute = Some(agg);
                    (close, low, high, raw_low, raw_high, None)
                }
            };
            if let Some((old_bucket, old_close, _, _, _, _)) = finalized
                && let Some(point) = runtime
                    .history
                    .iter_mut()
                    .find(|point| point.t == old_bucket)
            {
                point.p = old_close;
            }
            match runtime.history.last_mut() {
                Some(point) if point.t == bucket => point.p = close,
                _ => {
                    runtime.history.push(Point {
                        t: bucket,
                        p: close,
                    });
                    if runtime.history.len() > PRICE_HISTORY_LIMIT {
                        let excess = runtime.history.len() - PRICE_HISTORY_LIMIT;
                        runtime.history.drain(0..excess);
                    }
                }
            }

            if runtime.fill_qty_for != desired_qty {
                runtime.fill_history.clear();
                runtime.fill_qty_for = desired_qty;
                runtime.last_fill = None;
            }
            runtime.last_fill = fill;
            if let Some(fill) = fill {
                let last = runtime.fill_history.last().copied();
                match last {
                    Some(point) if point.t >= bucket => {
                        if let Some(point) = runtime.fill_history.last_mut() {
                            point.p = fill.price;
                        }
                    }
                    _ => {
                        runtime.fill_history.push(Point {
                            t: bucket,
                            p: fill.price,
                        });
                        if runtime.fill_history.len() > PRICE_HISTORY_LIMIT {
                            let excess = runtime.fill_history.len() - PRICE_HISTORY_LIMIT;
                            runtime.fill_history.drain(0..excess);
                        }
                    }
                }
            }

            // Rebuilds the model only when it has expired; the serialised form
            // is cached next to it, so the frame path only clones a string.
            let _ = Self::compute_trend(&config, runtime, now, false);
            (
                runtime.trend_json.clone(),
                runtime.allowed_price_min,
                runtime.allowed_price_max,
                runtime.allowed_at,
                close,
                low,
                high,
                raw_low,
                raw_high,
                finalized,
            )
        };

        self.persist_market(
            store,
            &config,
            now,
            close,
            low,
            high,
            raw_low,
            raw_high,
            fill,
            bucket,
            event.allowed_price_min.is_some() || event.allowed_price_max.is_some(),
            finalized,
        );

        // The frame is the only source of truth for the book — except while a
        // fill is being reconciled, when a frame generated before our purchase
        // was applied must not be allowed to resurrect the stock we already
        // bought. The raw depth still goes to the deck and to the snapshot.
        let (effective_stock, reconciling) = {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            let effective =
                runtime.effective_stock(now, best.quantity, best.unit_price, event.total_count);
            (effective, runtime.reconcile.is_some())
        };

        let settings = store.state.settings;
        let blocked = !settings.mode.plans();
        let plan = plan_order(
            config.plan_input(),
            best.unit_price,
            effective_stock as f64,
            blocked,
            Some(config.target_price),
        );
        let hot = plan.action == Action::Buy;
        // While off, report the plan the engine *would* have made. The gate
        // above still short-circuits before any budget is spent, but the log
        // line and the `hot` flag stay honest instead of blaming "paused" for
        // a frame that was really too expensive.
        let observed = if blocked {
            plan_order(
                config.plan_input(),
                best.unit_price,
                effective_stock as f64,
                false,
                Some(config.target_price),
            )
        } else {
            plan
        };

        // A headless worker (no control socket connected) must not pay to build
        // a payload nobody reads; the wiring flips this off every frame.
        if self.broadcast_market {
            events.push(EngineEvent::Market(Box::new(MarketTick {
                item_id: config.item_id.clone(),
                label: label.clone(),
                price: best.unit_price,
                close,
                low,
                high,
                raw_low,
                raw_high,
                stock: best.quantity,
                effective_stock,
                reconciling,
                target_price: config.target_price,
                desired_qty,
                allowed_price_min: allowed_min,
                allowed_price_max: allowed_max,
                allowed_at,
                trend: trend_json.clone(),
                fill,
                currency: currency.to_string(),
                item_type: item_type.clone(),
                bucket,
                hot: observed.action == Action::Buy,
                mode: settings.mode,
                at: now,
            })));
        }

        if observed.action == Action::Buy {
            self.runtime_for(&config.item_id, desired_qty)
                .last_in_range_at = now;
        }

        self.log_scan(&config, best.unit_price, effective_stock, &observed, now);

        if !hot {
            return events;
        }
        events.extend(self.place_order(
            store,
            &config,
            &label,
            &item_type,
            plan.qty.unwrap_or(1),
            plan.doubled.unwrap_or(false),
            best.price_raw,
            best.unit_price,
            Some(best.quantity),
            event.total_count,
            signal_at,
            handle_at,
            &event.cmd,
            raw.clone(),
            false,
        ));
        events
    }

    pub(super) fn log_scan(
        &mut self,
        config: &Target,
        price: f64,
        stock: i64,
        plan: &Plan,
        now: i64,
    ) {
        let runtime = self.runtime_for(&config.item_id, config.desired_qty);
        if now - runtime.last_log_at < SCAN_LOG_INTERVAL_MS {
            return;
        }
        runtime.last_log_at = now;
        let label = label_for(Some(config), &config.item_id);
        let message = if plan.action == Action::Buy {
            format!(
                "{label} {price:.6} RLT ×{stock} — IN RANGE (target {:.6})",
                config.target_price
            )
        } else if price <= config.target_price {
            // `plan.reason` is the real reason: callers pass the observed plan
            // (evaluated even while off), never the gated one.
            format!(
                "{label} {price:.6} RLT ×{stock} — IN RANGE, waiting ({})",
                plan.reason.unwrap_or("unknown")
            )
        } else {
            format!(
                "{label} {price:.6} RLT ×{stock} — above target {:.6}",
                config.target_price
            )
        };
        self.logger.info("scan", message);
    }
}
