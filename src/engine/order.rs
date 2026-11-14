use serde_json::Value;
use serde_json::json;

use crate::model::Target;
use crate::rules::{apply_total_price_markup, round_price};
use crate::store::Store;
use crate::timings::Timings;

use super::reconcile::{Reconcile, price_key};
use super::{Engine, EngineEvent, OrderIntent, warn_throttled};

impl Engine {
    /// Builds, broadcasts and (in live mode) submits one order.
    ///
    /// Every safety gate lives here instead of in `handle_market`, so the
    /// follow-up fired from the fill path cannot skip one: mode, one order in
    /// flight per target, failure backoff, the inter-order cooldown (opt-out
    /// for a follow-up), a linked session and the per-minute rate budget.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn place_order(
        &mut self,
        store: &mut Store,
        config: &Target,
        label: &str,
        item_type: &str,
        qty: i64,
        doubled: bool,
        price_raw: f64,
        unit_price: f64,
        stock_at_trigger: Option<i64>,
        total_count_at_trigger: Option<i64>,
        signal_at: i64,
        handle_at: i64,
        signal_cmd: &str,
        raw_price_event: Value,
        follow_up: bool,
    ) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        let settings = store.state.settings;
        if !settings.mode.plans() {
            return events;
        }
        let now = self.now();
        let desired_qty = config.desired_qty;
        {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            if runtime.inflight {
                return events;
            }
            if runtime.blocked_until > now {
                return events;
            }
        }
        // A follow-up extends a fill the engine has just booked rather than
        // opening a new signal, so the inter-order cooldown does not apply to
        // it. It still pays for a rate slot below.
        if !follow_up && now - self.last_order_at < settings.delay_per_order {
            self.logger
                .debug("engine", format!("{label} in range, waiting for cooldown"));
            return events;
        }
        // Checked before the rate slot on purpose: a live worker without a
        // session cannot place the order anyway, so spending its per-minute
        // budget would only make the next frames report a misleading
        // "order cap reached" instead of the real cause.
        if settings.mode.submits() && !self.has_token {
            if now - self.last_no_token_warn_at > 30_000 {
                self.logger.warn(
                    "engine",
                    "signal found but no Rollercoin session is linked; open the Link tab",
                );
                self.last_no_token_warn_at = now;
            }
            return events;
        }
        if !self.take_rate_slot(now, settings.max_orders_per_minute) {
            let should_log = {
                let runtime = self.runtime_for(&config.item_id, desired_qty);
                if now - runtime.last_log_at > 10_000 {
                    runtime.last_log_at = now;
                    true
                } else {
                    false
                }
            };
            if should_log {
                self.logger.warn(
                    "engine",
                    format!(
                        "order cap reached ({}/min); {} skipped",
                        settings.max_orders_per_minute, label
                    ),
                );
            }
            return events;
        }

        let qty = qty.max(1);
        let notional = round_price(unit_price * qty as f64);
        // The listing total plus the configured fulfillment headroom. Reading
        // it here, at plan time, keeps the request, the log line and the
        // broadcast describing the same cap even if the setting changes while
        // the request is in flight.
        let total_price_markup_pct = settings.total_price_markup_pct;
        let total_price_raw =
            apply_total_price_markup(price_raw * qty as f64, total_price_markup_pct);
        let timings = Timings {
            v: 1,
            signal_at: Some(signal_at as f64),
            handle_at: Some(handle_at as f64),
            order_at: Some(now as f64),
            signal_cmd: Some(signal_cmd.to_string()),
            ..Default::default()
        };

        self.last_order_at = now;
        {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            runtime.last_order_at = now;
            runtime.orders += 1;
        }

        let intent = OrderIntent {
            item_id: config.item_id.clone(),
            item_type: item_type.to_string(),
            label: label.to_string(),
            qty,
            stock_at_trigger,
            total_count_at_trigger,
            unit_price_raw: price_raw,
            unit_price,
            target_price: config.target_price,
            notional,
            total_price_markup_pct,
            total_price_raw,
            doubled,
            optimistic: follow_up,
            at: now,
            timings,
            raw_price_event,
        };

        if !settings.mode.submits() {
            self.runtime_for(&config.item_id, desired_qty).last_result = Some(json!({
                "status": "dry", "at": now, "qty": qty, "price": unit_price
            }));
            self.logger.info(
                "order",
                format!("SHADOW — would buy {label} ×{qty} @ {unit_price:.6} RLT"),
            );
            events.push(EngineEvent::Broadcast {
                kind: "order",
                data: json!({
                    "phase": "dry",
                    "itemId": intent.item_id,
                    "itemType": intent.item_type,
                    "label": intent.label,
                    "qty": intent.qty,
                    "unitPriceRaw": intent.unit_price_raw,
                    "unitPrice": intent.unit_price,
                    "targetPrice": intent.target_price,
                    "notional": intent.notional,
                    "totalPriceRaw": intent.total_price_raw,
                    "markupPct": intent.total_price_markup_pct,
                    "at": intent.at,
                    "timings": intent.timings,
                    "reason": "dry-run",
                    "mode": settings.mode,
                    "doubled": intent.doubled,
                    "optimistic": intent.optimistic,
                }),
            });
            return events;
        }

        store.state.stats.orders_sent += 1;
        // Only the counters changed; rewriting the whole target list here made
        // one order cost an upsert per configured item.
        if let Err(error) = store.flush_stats() {
            warn_throttled(
                &self.logger,
                &mut self.last_flush_error_at,
                now,
                "store",
                format!("could not persist the order counter: {error}"),
            );
        }
        {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            runtime.inflight = true;
            runtime.last_result = Some(json!({
                "status": "submitted", "at": now, "qty": qty, "price": unit_price
            }));
            // The units are committed the moment the request goes out, so the
            // ledger opens here rather than waiting for the result frame. A
            // stale frame carrying the pre-purchase book can no longer order
            // them a second time, and the early rearm reads a floor that already
            // has every outstanding order reserved out of it.
            let key = price_key(unit_price);
            let live = matches!(runtime.reconcile, Some(window) if window.live(now) && window.price_key == key);
            if live {
                if let Some(window) = runtime.reconcile.as_mut() {
                    window.reserve(qty);
                }
            } else if let Some(stock) = stock_at_trigger {
                let mut window = Reconcile::open(now, stock, unit_price, total_count_at_trigger);
                window.reserve(qty);
                runtime.reconcile = Some(window);
            }
        }

        let mut submitted = intent;
        submitted.timings.dispatch_at = Some(self.now() as f64);
        // Provisional: the wiring replaces this with the real HTTP dispatch
        // time when it has one, so live broadcasts still carry a mark.
        submitted.timings.send_at = Some(self.now() as f64);

        events.push(EngineEvent::Broadcast {
            kind: "order",
            data: json!({
                "phase": "submitted",
                "itemId": submitted.item_id,
                "itemType": submitted.item_type,
                "label": submitted.label,
                "qty": submitted.qty,
                "unitPriceRaw": submitted.unit_price_raw,
                "unitPrice": submitted.unit_price,
                "targetPrice": submitted.target_price,
                "notional": submitted.notional,
                "totalPriceRaw": submitted.total_price_raw,
                "markupPct": submitted.total_price_markup_pct,
                "at": submitted.at,
                "timings": submitted.timings,
                "optimistic": submitted.optimistic,
            }),
        });
        let mut marker = if submitted.optimistic {
            " [LOCAL REARM]".to_string()
        } else if submitted.doubled {
            " [HIGH STOCK ×2]".to_string()
        } else {
            String::new()
        };
        if submitted.total_price_markup_pct > 0.0 {
            marker.push_str(&format!(
                " [+{}% HEADROOM]",
                submitted.total_price_markup_pct
            ));
        }
        self.logger.info(
            "order",
            format!(
                "SEND {} ×{} @ {:.6} RLT ({:.6} total){}",
                label, qty, submitted.unit_price, submitted.notional, marker
            ),
        );

        events.push(EngineEvent::Submit(Box::new(submitted)));
        events
    }

    pub(super) fn take_rate_slot(&mut self, now: i64, max_per_minute: i64) -> bool {
        let cutoff = now - 60_000;
        self.order_stamps.retain(|stamp| *stamp >= cutoff);
        if self.order_stamps.len() as i64 >= max_per_minute {
            return false;
        }
        self.order_stamps.push(now);
        true
    }

    /// How many rate-limit slots the current minute has already spent. Used by
    /// tests to prove that `off` costs nothing while `shadow` does.
    #[cfg(test)]
    pub(super) fn order_stamps_len(&self) -> usize {
        self.order_stamps.len()
    }
}
