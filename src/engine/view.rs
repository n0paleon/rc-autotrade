use serde_json::Value;
use serde_json::json;

use crate::store::Store;
use crate::trend::TrendSignal;

use super::Engine;
use super::label_for;
use super::runtime::Runtime;

impl Engine {
    pub fn reset_market_state(&mut self) {
        self.runtime.clear();
    }

    pub fn reset_counters(&mut self) {
        for runtime in self.runtime.values_mut() {
            runtime.orders = 0;
            runtime.fills = 0;
            runtime.failures = 0;
            runtime.last_result = None;
        }
    }

    /// Releases runtimes for targets that no longer exist and have not been
    /// seen for `max_idle_ms`. A target that is deleted and re-added within that
    /// window keeps its history, so the chart is unchanged.
    pub fn prune_orphan_runtimes(&mut self, store: &Store, now: i64, max_idle_ms: i64) -> usize {
        let before = self.runtime.len();
        self.runtime.retain(|item_id, runtime| {
            if store.find_config(item_id).is_some() {
                return true;
            }
            let last_seen = runtime.seen_at.or(runtime.hydrated_at).unwrap_or(0);
            let has_data = runtime.last_price.is_some()
                || !runtime.history.is_empty()
                || !runtime.fill_history.is_empty()
                || runtime.trend.is_some();
            has_data && now.saturating_sub(last_seen) <= max_idle_ms
        });
        before - self.runtime.len()
    }

    pub fn refresh_trend(&mut self, store: &Store, item_id: &str) -> Option<TrendSignal> {
        let config = store.find_config(item_id)?.clone();
        let now = self.now();
        let desired_qty = config.desired_qty;
        let runtime = self.runtime_for(&config.item_id, desired_qty);
        if runtime.fill_qty_for != desired_qty {
            runtime.fill_history.clear();
            runtime.last_fill = None;
            runtime.fill_qty_for = desired_qty;
        }
        Self::compute_trend(&config, runtime, now, true)
    }

    pub fn runtime_snapshot(&self, store: &Store) -> Value {
        let mut out = serde_json::Map::new();
        // Only targets that still exist are exposed. Orphan runtimes are kept
        // for a grace period (see `prune_orphan_runtimes`) but never serialized,
        // so a long delete history no longer costs CPU on every snapshot.
        for config in &store.state.configs {
            let key = config.item_id.to_ascii_lowercase();
            let Some(runtime) = self.runtime.get(&key) else {
                continue;
            };
            let label = label_for(Some(config), &key);
            out.insert(key, runtime_json(runtime, label));
        }
        Value::Object(out)
    }
}

pub(super) fn runtime_json(runtime: &Runtime, label: String) -> Value {
    let mut value = serde_json::Map::new();
    value.insert("label".into(), json!(label));
    value.insert("lastPrice".into(), json!(runtime.last_price));
    value.insert("lastStock".into(), json!(runtime.last_stock));
    value.insert("lastSeenAt".into(), json!(runtime.last_seen_at));
    value.insert("seenAt".into(), json!(runtime.seen_at));
    value.insert("stale".into(), json!(runtime.stale));
    value.insert("itemType".into(), json!(runtime.item_type));
    value.insert("allowedPriceMin".into(), json!(runtime.allowed_price_min));
    value.insert("allowedPriceMax".into(), json!(runtime.allowed_price_max));
    value.insert("allowedAt".into(), json!(runtime.allowed_at));
    value.insert(
        "history".into(),
        serde_json::to_value(&runtime.history).unwrap_or(Value::Null),
    );
    value.insert(
        "lastFill".into(),
        serde_json::to_value(runtime.last_fill).unwrap_or(Value::Null),
    );
    value.insert(
        "trend".into(),
        serde_json::to_value(&runtime.trend).unwrap_or(Value::Null),
    );
    value.insert("trendAt".into(), json!(runtime.trend_at));
    value.insert("hydratedAt".into(), json!(runtime.hydrated_at));
    value.insert("inflight".into(), json!(runtime.inflight));
    value.insert("orders".into(), json!(runtime.orders));
    value.insert("fills".into(), json!(runtime.fills));
    value.insert("failures".into(), json!(runtime.failures));
    value.insert("lastResult".into(), json!(runtime.last_result));
    value.insert("failureStreak".into(), json!(runtime.failure_streak));
    value.insert("blockedUntil".into(), json!(runtime.blocked_until));
    value.insert(
        "reconcile".into(),
        serde_json::to_value(runtime.reconcile).unwrap_or(Value::Null),
    );
    value.insert("lastLogAt".into(), json!(runtime.last_log_at));
    value.insert("lastOrderAt".into(), json!(runtime.last_order_at));
    value.insert("lastInRangeAt".into(), json!(runtime.last_in_range_at));
    Value::Object(value)
}
