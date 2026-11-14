use serde_json::Value;
use serde_json::json;

use crate::model::OrderEntry;
use crate::store::Store;
use crate::timings::Timings;

use super::{Engine, EngineEvent, warn_throttled};

pub(super) struct FailureMeta {
    pub(super) item_id: String,
    pub(super) label: String,
    pub(super) qty: i64,
    /// Count from the fulfillment result event (`fill.count`). This is what's
    /// recorded as `OrderEntry::qty` (Got). For a rejected purchase there is
    /// no result event, so it stays 0.
    pub(super) got_qty: i64,
    pub(super) stock_at_trigger: Option<i64>,
    pub(super) unit_price: f64,
    pub(super) target_price: f64,
    pub(super) at: i64,
    pub(super) timings: Option<Timings>,
    pub(super) raw_price_event: Option<Value>,
    pub(super) raw_result_event: Option<Value>,
    /// Exact purchase request body, stored with the loot row so the detail
    /// modal can show what was sent even when no result frame arrived.
    pub(super) request_payload: Option<Value>,
    /// True for a locally fired follow-up. Only changes how the row reads: the
    /// backoff decision is made by the caller.
    pub(super) optimistic: bool,
}

impl Engine {
    pub(super) fn record_failure(
        &mut self,
        store: &mut Store,
        meta: FailureMeta,
        reason: &str,
        order_id: Option<String>,
    ) -> Vec<EngineEvent> {
        store.state.stats.failed += 1;
        {
            let runtime = self.runtime_for(&meta.item_id, meta.qty.max(1));
            runtime.failures += 1;
        }
        let now = self.now();
        let mut timings = meta.timings.clone().unwrap_or_default();
        if timings.failed_at.is_none() {
            timings.failed_at = Some(now as f64);
        }

        let entry = OrderEntry {
            id: order_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            order_id,
            item_id: meta.item_id.clone(),
            label: meta.label.clone(),
            // Failures: `qty` is the count from the fulfillment result event
            // (`fill.count`, carried in `meta.got_qty`), while `requested_qty`
            // keeps the attempted count for the loot log's Sent column.
            qty: meta.got_qty,
            requested_qty: Some(meta.qty),
            stock_at_trigger: meta.stock_at_trigger,
            unit_price: meta.unit_price,
            total_price: 0.0,
            target_price: meta.target_price,
            profit_rlt: 0.0,
            status: "failed".to_string(),
            at: now,
            latency_ms: Some(now - meta.at).filter(|value| *value >= 0),
            timings: Some(timings),
            raw_events: crate::raw_events::sanitize(&json!({
                "purchaseRequest": meta.request_payload,
                "priceEvent": meta.raw_price_event,
                "resultEvent": meta.raw_result_event,
            })),
            note: Some(if meta.optimistic {
                format!("optimistic follow-up — {reason}")
                    .chars()
                    .take(160)
                    .collect()
            } else {
                reason.chars().take(160).collect()
            }),
            message: Some(reason.chars().take(160).collect()),
        };

        if let Err(error) = store.add_history(entry.clone()) {
            warn_throttled(
                &self.logger,
                &mut self.last_history_error_at,
                now,
                "store",
                format!("could not record the failed order: {error}"),
            );
        }

        vec![
            EngineEvent::Broadcast {
                kind: "history",
                data: serde_json::to_value(&entry).unwrap_or(Value::Null),
            },
            EngineEvent::Broadcast {
                kind: "stats",
                data: serde_json::to_value(store.state.stats).unwrap_or(Value::Null),
            },
            EngineEvent::Broadcast {
                kind: "report",
                data: serde_json::to_value(store.report_view()).unwrap_or(Value::Null),
            },
        ]
    }
}
