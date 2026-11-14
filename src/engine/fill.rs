use serde_json::Value;
use serde_json::json;

use crate::constants::{ORDER_STATUS_FAILED, ORDER_STATUS_FILLED, PRICE_SCALE};
use crate::events::UpstreamEvent;
use crate::model::{OrderEntry, Target};
use crate::rules::{plan_order, round_price};
use crate::store::Store;

use super::failure::FailureMeta;
use super::reconcile::price_key;
use super::result::PendingOrder;
use super::{Engine, EngineEvent, label_for, warn_throttled};

impl Engine {
    pub(super) fn handle_fill(
        &mut self,
        store: &mut Store,
        event: &UpstreamEvent,
        raw: &Value,
        received_at: i64,
    ) -> Vec<EngineEvent> {
        let Some(fill) = event.fill.clone() else {
            return Vec::new();
        };
        let pending = fill
            .order_id
            .as_ref()
            .and_then(|order_id| self.pending.get(order_id).cloned());

        let Some(pending_meta) = pending else {
            match &fill.order_id {
                Some(order_id) => self.buffer_unmatched(order_id, event, raw, received_at),
                None => self
                    .logger
                    .debug("fill", "ignoring result without an order id"),
            }
            return Vec::new();
        };

        let item_id = if pending_meta.item_id.is_empty() {
            event.item_id.clone().unwrap_or_default()
        } else {
            pending_meta.item_id.clone()
        };
        let config = store.find_config(&item_id).cloned();
        let now = self.now();
        let fill_at = received_at;
        let mut fill_timings = pending_meta.timings.clone();
        fill_timings.fill_at = Some(fill_at as f64);
        fill_timings.fill_handled_at = Some(now as f64);

        let label = if pending_meta.label.is_empty() {
            label_for(config.as_ref(), &item_id)
        } else {
            pending_meta.label.clone()
        };
        let target_price = Some(pending_meta.target_price)
            .filter(|value| value.is_finite() && *value > 0.0)
            .or_else(|| config.as_ref().map(|config| config.target_price));

        if let Some(order_id) = &fill.order_id
            && store.has_order(order_id)
        {
            self.logger.debug(
                "fill",
                format!("ignoring duplicate confirmation {order_id}"),
            );
            return Vec::new();
        }

        let mut events = Vec::new();
        if fill.status as i64 == ORDER_STATUS_FILLED && fill.count > 0 && fill.price_raw > 0.0 {
            let total_price = fill.price_raw / PRICE_SCALE;
            let unit_price = total_price / fill.count as f64;
            let reference = target_price.unwrap_or(unit_price);
            let profit = round_price((reference - unit_price) * fill.count as f64);

            {
                let stats = &mut store.state.stats;
                stats.filled += 1;
                stats.bought += fill.count;
                stats.spent_rlt = round_price(stats.spent_rlt + total_price);
                stats.profit_rlt = round_price(stats.profit_rlt + profit);
            }

            let entry = OrderEntry {
                id: fill
                    .order_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                order_id: fill.order_id.clone(),
                item_id: item_id.clone(),
                label: label.clone(),
                qty: fill.count,
                requested_qty: Some(pending_meta.qty),
                stock_at_trigger: pending_meta.stock_at_trigger,
                unit_price: round_price(unit_price),
                total_price: round_price(total_price),
                target_price: reference,
                profit_rlt: profit,
                status: "filled".to_string(),
                at: now,
                latency_ms: Some(now - pending_meta.at),
                timings: Some(fill_timings),
                raw_events: crate::raw_events::sanitize(&json!({
                    "purchaseRequest": pending_meta.request_payload,
                    "priceEvent": pending_meta.raw_price_event,
                    "resultEvent": raw,
                })),
                note: pending_meta
                    .optimistic
                    .then(|| "optimistic follow-up".to_string()),
                message: Some("-".to_string()),
            };

            let entry_id = entry.id.clone();
            if let Err(error) = store.add_history(entry.clone()) {
                warn_throttled(
                    &self.logger,
                    &mut self.last_history_error_at,
                    now,
                    "store",
                    format!("could not record fill {entry_id}: {error}"),
                );
            }
            if let Some(order_id) = &fill.order_id {
                self.pending.remove(order_id);
            }

            // The fill is booked against the local ledger: the part of the
            // request upstream did not take comes back to the book, and what it
            // did take stays spent. The raw `last_stock` is deliberately left
            // alone — it must keep reporting what upstream said.
            let stock_after = {
                let runtime = self.runtime_for(&item_id, pending_meta.qty.max(1));
                runtime.fills += 1;
                runtime.last_result = Some(json!({
                    "status": "filled", "at": now, "qty": fill.count,
                    "price": unit_price, "profit": profit
                }));
                runtime
                    .settle_fill(
                        now,
                        pending_meta.qty,
                        fill.count,
                        pending_meta.total_count_at_trigger,
                        pending_meta.unit_price,
                    )
                    .or(runtime.last_stock)
                    .unwrap_or(0)
            };

            self.logger.info(
                "fill",
                format!(
                    "FILLED {label} ×{} @ {:.6} RLT | profit {}{:.6} RLT | {}ms",
                    fill.count,
                    unit_price,
                    if profit >= 0.0 { "+" } else { "" },
                    profit,
                    entry.latency_ms.unwrap_or(0)
                ),
            );

            let mut data = serde_json::to_value(&entry).unwrap_or(Value::Null);
            if let Some(object) = data.as_object_mut() {
                object.insert("phase".to_string(), Value::String("filled".into()));
                object.insert("stockAfter".to_string(), json!(stock_after));
            }
            events.push(EngineEvent::Broadcast {
                kind: "order",
                data,
            });
            events.push(EngineEvent::Broadcast {
                kind: "stats",
                data: serde_json::to_value(store.state.stats).unwrap_or(Value::Null),
            });
            events.push(EngineEvent::Broadcast {
                kind: "report",
                data: serde_json::to_value(store.report_view()).unwrap_or(Value::Null),
            });
            self.logger
                .debug("fill", format!("recorded {entry_id} for {item_id}"));
            events.extend(self.rearm_after_fill(
                store,
                config.as_ref(),
                &pending_meta,
                &label,
                fill.count,
                fill_at,
                now,
            ));
            return events;
        }

        if fill.status as i64 == ORDER_STATUS_FAILED && fill.count == 0 && fill.price_raw == 0.0 {
            if let Some(order_id) = &fill.order_id {
                self.pending.remove(order_id);
            }
            // Upstream just said there is nothing, so this order's reservation
            // goes back to the book. The ledger is kept rather than dropped: a
            // follow-up that is still in flight owns its own share of the same
            // floor, and the next frame closes the window by showing at most
            // what is actually left.
            self.runtime_for(&item_id, pending_meta.qty.max(1))
                .release_reservation(now, pending_meta.qty, pending_meta.unit_price);
            self.logger.warn(
                "fill",
                format!(
                    "FAILED {label} — no items were obtained (order {})",
                    fill.order_id.clone().unwrap_or_else(|| "?".into())
                ),
            );
            events.extend(self.record_failure(
                store,
                FailureMeta {
                    item_id: item_id.clone(),
                    label: label.clone(),
                    // Keep the attempted order in the loot row. The upstream
                    // result is empty (count/price 0), but the log must still
                    // show what we tried to buy and how long it took.
                    qty: pending_meta.qty,
                    // Got comes straight from the event: fill.count is 0 on
                    // this branch (checked above).
                    got_qty: fill.count,
                    stock_at_trigger: pending_meta.stock_at_trigger,
                    unit_price: pending_meta.unit_price,
                    target_price: target_price.unwrap_or(pending_meta.target_price),
                    at: pending_meta.at,
                    timings: Some(fill_timings),
                    raw_price_event: Some(pending_meta.raw_price_event.clone()),
                    raw_result_event: Some(raw.clone()),
                    request_payload: pending_meta.request_payload.clone(),
                    optimistic: pending_meta.optimistic,
                },
                "no items obtained",
                fill.order_id.clone(),
            ));
            events.push(EngineEvent::Broadcast {
                kind: "order",
                data: json!({
                    "phase": "failed",
                    "itemId": item_id,
                    "label": label,
                    "orderId": fill.order_id,
                    "reason": "no items obtained",
                    "at": now,
                }),
            });
            return events;
        }

        self.logger.debug(
            "fill",
            format!("ignored fill status {} for {label}", fill.status),
        );
        events
    }

    /// Fires the next slice of a level as soon as its fill lands, instead of
    /// waiting for the next market frame to re-advertise the leftover stock.
    ///
    /// The quantity no longer comes from the intent's own unfilled remainder.
    /// It comes from the same planner a frame would call, against the local
    /// ledger the fill just settled, so every quantity relationship is covered:
    ///
    /// * `got < sent` — the order was partially matched and the rest of the
    ///   request is still on the book;
    /// * `sent < ready` — the request never covered the best level, so the level
    ///   still holds `ready - sent` units that a frame would re-advertise one WS
    ///   hop later;
    /// * a full fill of the whole level — the settled book is empty and the
    ///   planner returns `no-stock`, so nothing is fired.
    ///
    /// The local floor is the bound, not `stockAtTrigger - got`. The floor
    /// already had every order the engine placed reserved out of it, so a stale
    /// frame or an earlier follow-up can never hand the same units to two
    /// orders — the wrong buy this used to trigger when the real result landed.
    ///
    /// The plan still goes through the normal price/target/minimum checks; a
    /// slice below `min_acceptable_qty` is left alone, exactly like a thin frame
    /// would be.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn rearm_after_fill(
        &mut self,
        store: &mut Store,
        config: Option<&Target>,
        pending: &PendingOrder,
        label: &str,
        got: i64,
        signal_at: i64,
        now: i64,
    ) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        let Some(config) = config else {
            return events;
        };
        if got <= 0 {
            return events;
        }
        if !store.state.settings.mode.plans() {
            return events;
        }
        let key = price_key(pending.unit_price);
        let available = {
            let runtime = self.runtime_for(&config.item_id, config.desired_qty);
            match runtime.reconcile {
                // Only a live ledger for the same level may be traded from:
                // once a frame (or the window) has taken authority back, the
                // fill's old trigger snapshot is stale and re-arming from it
                // would buy goods that are already committed or gone.
                Some(window) if window.live(now) && window.price_key == key => window.floor,
                _ => return events,
            }
        };
        if available < 1 {
            return events;
        }

        let plan = plan_order(
            crate::rules::PlanInput {
                active: config.active,
                target_price: config.target_price,
                // A fill is a fresh look at the same level, so the target's
                // normal per-signal quantity applies. It is deliberately not
                // the intent's unfilled remainder: when `sent < ready` the book
                // is deeper than the request and a frame would order the same
                // quantity again.
                desired_qty: config.desired_qty,
                // Never re-double a level just because high-stock mode would
                // have doubled it on a frame: a fill confirms what was taken,
                // it is not a new signal.
                high_stock_threshold: i64::MAX,
                min_acceptable_qty: config.min_acceptable_qty,
                allow_partial_order: config.allow_partial_order,
            },
            pending.unit_price,
            available as f64,
            false,
            Some(config.target_price),
        );
        if !plan.is_buy() {
            self.logger.info(
                "order",
                format!(
                    "fill for {label} leaves {available} on the book, but the plan says {} — no local rearm",
                    plan.reason.unwrap_or("skip")
                ),
            );
            return events;
        }
        let qty = plan.qty.unwrap_or(1);
        // `plan_order` only applies the minimum when the book is thinner than
        // the request. A follow-up asks for a slice of the level, so it would
        // otherwise slip a below-minimum order through; holding it to the same
        // minimum keeps a config like "never fill fewer than 2" honest.
        if qty < config.min_acceptable_qty {
            self.logger.info(
                "order",
                format!(
                    "fill for {label} leaves {qty}, below the {}-unit minimum — no local rearm",
                    config.min_acceptable_qty
                ),
            );
            return events;
        }
        self.logger.info(
            "order",
            format!(
                "LOCAL REARM {label} ×{qty} @ {:.6} RLT — {available} left on the level after the fill, not waiting for the next frame",
                pending.unit_price
            ),
        );
        events.extend(self.place_order(
            store,
            config,
            label,
            &pending.item_type,
            qty,
            false,
            (pending.unit_price * PRICE_SCALE).round(),
            pending.unit_price,
            Some(available),
            // The follow-up shares the window that is already open; forwarding
            // the parent's `totalCount` would be a stale proof.
            None,
            signal_at,
            now,
            "marketplace_orders_filled",
            pending.raw_price_event.clone(),
            true,
        ));
        events
    }
}
