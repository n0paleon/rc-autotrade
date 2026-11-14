use std::collections::VecDeque;

use serde_json::Value;
use serde_json::json;

use crate::events::UpstreamEvent;
use crate::store::Store;
use crate::timings::Timings;

use super::failure::FailureMeta;
use super::runtime::BufferedResult;
use super::{
    Engine, EngineEvent, MAX_UNMATCHED_RESULTS, OrderIntent, PENDING_TIMEOUT_MS, PurchaseOutcome,
};

#[derive(Debug, Clone)]
pub(super) struct PendingOrder {
    pub(super) item_id: String,
    pub(super) item_type: String,
    pub(super) label: String,
    pub(super) qty: i64,
    pub(super) stock_at_trigger: Option<i64>,
    pub(super) total_count_at_trigger: Option<i64>,
    pub(super) optimistic: bool,
    pub(super) unit_price: f64,
    pub(super) target_price: f64,
    pub(super) at: i64,
    pub(super) timings: Timings,
    pub(super) raw_price_event: Value,
    pub(super) request_payload: Option<Value>,
}

impl Engine {
    pub fn on_purchase_result(
        &mut self,
        store: &mut Store,
        intent: &OrderIntent,
        outcome: PurchaseOutcome,
    ) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        let now = self.now();
        let mut timings = intent.timings.clone();
        // Overwrite the provisional engine mark with the real HTTP dispatch.
        if let Some(send_at) = outcome.send_at {
            timings.send_at = Some(send_at as f64);
        }
        timings.response_at = outcome.response_at.map(|value| value as f64);
        timings.complete_at = outcome.complete_at.map(|value| value as f64);
        timings.ttfb_ms = outcome.ttfb_ms;

        let accepted = outcome.ok && outcome.order_id.is_some();
        // Released before the result is handled: a fill that beat the HTTP
        // response can fire a follow-up from the replay below, and that
        // follow-up needs the slot. Clearing it afterwards would also mark the
        // follow-up as not in flight, which would let a third order through.
        self.runtime_for(&intent.item_id, intent.qty.max(1))
            .inflight = false;
        if accepted {
            timings.accepted_at = Some(now as f64);
            let order_id = outcome.order_id.clone().unwrap_or_default();
            self.pending.insert(
                order_id.clone(),
                PendingOrder {
                    item_id: intent.item_id.clone(),
                    item_type: intent.item_type.clone(),
                    label: intent.label.clone(),
                    qty: intent.qty,
                    stock_at_trigger: intent.stock_at_trigger,
                    total_count_at_trigger: intent.total_count_at_trigger,
                    optimistic: intent.optimistic,
                    unit_price: intent.unit_price,
                    target_price: intent.target_price,
                    at: intent.at,
                    timings: timings.clone(),
                    raw_price_event: intent.raw_price_event.clone(),
                    request_payload: outcome.request_payload.clone(),
                },
            );

            {
                let runtime = self.runtime_for(&intent.item_id, intent.qty.max(1));
                runtime.failure_streak = 0;
                runtime.blocked_until = 0;
                runtime.last_result = Some(json!({
                    "status": "accepted", "at": now, "orderId": order_id, "qty": intent.qty
                }));
            }

            self.logger.info(
                "order",
                format!(
                    "ACCEPTED {} ×{} (order {})",
                    intent.label, intent.qty, order_id
                ),
            );
            events.push(EngineEvent::Broadcast {
                kind: "order",
                data: json!({
                    "phase": "accepted",
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
                    "timings": timings,
                    "orderId": order_id,
                }),
            });

            // The upstream result can beat the purchase HTTP response.
            if let Some(index) = self.unmatched.iter().position(|(id, _)| id == &order_id)
                && let Some((_, buffered)) = self.unmatched.remove(index)
            {
                events.extend(self.handle_fill(
                    store,
                    &buffered.event,
                    &buffered.raw,
                    buffered.received_at,
                ));
            }
        } else {
            let reason = outcome
                .error
                .clone()
                .unwrap_or_else(|| "marketplace rejected the order".to_string());
            timings.failed_at = Some(now as f64);
            let (streak, backoff) = {
                let runtime = self.runtime_for(&intent.item_id, intent.qty.max(1));
                if intent.optimistic {
                    // An optimistic follow-up is a guess about the remainder of
                    // a partial fill, so a rejection is an expected outcome.
                    // Backing off here would freeze the whole target for up to
                    // a minute — far longer than the frame wait it replaced.
                    // The reconcile window is deliberately left in place: the
                    // guess failed, which is no evidence that the next frame is
                    // fresh, and the window bounds the blindness to 1.5 s.
                    runtime.last_result = Some(json!({
                        "status": "rejected", "at": now, "reason": reason,
                        "optimistic": true
                    }));
                    (runtime.failure_streak, 0)
                } else {
                    runtime.failure_streak += 1;
                    let backoff = (5_000_i64
                        * 2_i64.pow((runtime.failure_streak - 1).min(3) as u32))
                    .min(60_000);
                    runtime.blocked_until = now + backoff;
                    runtime.last_result = Some(json!({
                        "status": "rejected", "at": now, "reason": reason
                    }));
                    (runtime.failure_streak, backoff)
                }
            };
            if backoff > 0 {
                self.logger.warn(
                    "order",
                    format!(
                        "REJECTED {} ×{}: {} — backing off {}s ({})",
                        intent.label,
                        intent.qty,
                        reason,
                        backoff / 1000,
                        streak
                    ),
                );
            } else {
                self.logger.warn(
                    "order",
                    format!(
                        "REJECTED {} ×{}: {} — optimistic follow-up, not backing off",
                        intent.label, intent.qty, reason
                    ),
                );
            }
            events.extend(self.record_failure(
                store,
                FailureMeta {
                    item_id: intent.item_id.clone(),
                    label: intent.label.clone(),
                    qty: intent.qty,
                    // Rejected purchase: no fulfillment event, so Got is 0.
                    got_qty: 0,
                    stock_at_trigger: intent.stock_at_trigger,
                    unit_price: intent.unit_price,
                    target_price: intent.target_price,
                    at: intent.at,
                    timings: Some(timings.clone()),
                    raw_price_event: Some(intent.raw_price_event.clone()),
                    raw_result_event: None,
                    request_payload: outcome.request_payload.clone(),
                    optimistic: intent.optimistic,
                },
                &reason,
                outcome.order_id.clone(),
            ));
            events.push(EngineEvent::Broadcast {
                kind: "order",
                data: json!({
                    "phase": "failed",
                    "itemId": intent.item_id,
                    "label": intent.label,
                    "qty": intent.qty,
                    "reason": reason,
                    "timings": timings,
                    "at": intent.at,
                    "optimistic": intent.optimistic,
                }),
            });
        }

        events
    }

    pub(super) fn buffer_unmatched(
        &mut self,
        order_id: &str,
        event: &UpstreamEvent,
        raw: &Value,
        received_at: i64,
    ) {
        let now = self.now();
        self.prune_unmatched(now);
        self.unmatched.retain(|(id, _)| id != order_id);
        while self.unmatched.len() >= MAX_UNMATCHED_RESULTS {
            if let Some((oldest, _)) = self.unmatched.pop_front() {
                self.logger.debug(
                    "fill",
                    format!("discarding unmatched result {oldest} (buffer full)"),
                );
            }
        }
        self.unmatched.push_back((
            order_id.to_string(),
            BufferedResult {
                event: event.clone(),
                raw: raw.clone(),
                received_at,
                buffered_at: now,
            },
        ));
        self.logger.debug(
            "fill",
            format!("unmatched result {order_id}; waiting briefly for purchase response"),
        );
    }

    pub(super) fn prune_unmatched(&mut self, now: i64) {
        let mut kept = VecDeque::new();
        while let Some((order_id, buffered)) = self.unmatched.pop_front() {
            if now - buffered.buffered_at > PENDING_TIMEOUT_MS {
                self.logger.debug(
                    "fill",
                    format!("discarding unmatched result {order_id} after 120s"),
                );
            } else {
                kept.push_back((order_id, buffered));
            }
        }
        self.unmatched = kept;
    }

    pub fn prune_pending(&mut self) {
        let now = self.now();
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, meta)| now - meta.at > PENDING_TIMEOUT_MS)
            .map(|(order_id, _)| order_id.clone())
            .collect();
        for order_id in expired {
            self.pending.remove(&order_id);
            self.logger.warn(
                "order",
                format!("no confirmation for {order_id} after 120s; releasing it"),
            );
        }
        self.prune_unmatched(now);
    }

    pub fn pending_snapshot(&self) -> Vec<Value> {
        self.pending
            .iter()
            .map(|(order_id, meta)| {
                json!({
                    "orderId": order_id,
                    "itemId": meta.item_id,
                    "itemType": meta.item_type,
                    "label": meta.label,
                    "qty": meta.qty,
                    "unitPrice": meta.unit_price,
                    "targetPrice": meta.target_price,
                    "at": meta.at,
                    "timings": meta.timings,
                })
            })
            .collect()
    }
}
