//! Read-only views derived from the store, engine and sidecar services.

use std::sync::Arc;

use axum::response::{IntoResponse, Response};
use axum::{Json, extract::State};
use serde_json::{Value, json};

use super::AppState;
use super::auth::Auth;
use crate::benchmark;
use crate::clock::now_ms;
use crate::constants::{APP_NAME, APP_VERSION, LOOT_LOG_PAGE_SIZE, Limits};
use crate::engine::Engine;
use crate::lock::lock;
use crate::store::Store;

impl AppState {
    pub fn configs_view(&self, store: &Store, engine: &Engine) -> Vec<Value> {
        let mut runtime = engine.runtime_snapshot(store);
        let mut runtime_entries = runtime.as_object_mut();
        store
            .state
            .configs
            .iter()
            .map(|config| {
                let mut value = serde_json::to_value(config).unwrap_or(Value::Null);
                if let Some(object) = value.as_object_mut() {
                    let key = config.item_id.to_ascii_lowercase();
                    // Move the entry out of the temporary map instead of
                    // cloning the whole history for every target on each view.
                    let entry = runtime_entries
                        .as_mut()
                        .and_then(|entries| entries.remove(&key))
                        .unwrap_or(Value::Null);
                    object.insert("runtime".to_string(), entry);
                }
                value
            })
            .collect()
    }

    /// Rolling order-pipeline benchmark rebuilt from the loot log.
    pub fn benchmark_snapshot(&self, store: &Store) -> Value {
        let now = now_ms();
        // `benchmark_samples` returns newest-first, so the Loot Log counter
        // can average the newest 30 orders straight from this slice — no
        // separate time-bounded query is needed any more.
        let samples = store.benchmark_samples(benchmark::CAPACITY);
        let mut snapshot = benchmark::snapshot(&samples, now);
        snapshot["recent"] = benchmark::recent(&samples, benchmark::RECENT_ORDERS);
        snapshot
    }

    pub fn snapshot(&self, store: &Store, engine: &Engine) -> Value {
        let now = now_ms();
        let gateway = match lock(&self.gateway).as_ref() {
            Some(handle) => handle.stats_json(),
            None => json!({
                "status": "idle",
                "connectedAt": null,
                "lastMessageAt": null,
                "lastError": null,
                "attempts": 0,
                "reconnects": 0,
                "eventCount": 0,
                "marketEventCount": 0,
                "parseErrors": 0,
                "commands": [],
                "upstream": super::router::upstream_host(&self.config.ws_url),
                "uptimeMs": 0,
                "lastPingMs": null,
            }),
        };
        let latency = lock(&self.latency).snapshot();
        let pending = engine.pending_snapshot();

        json!({
            "now": now,
            "name": APP_NAME,
            "version": APP_VERSION,
            "startedAt": self.started_at,
            "uptimeMs": now - self.started_at,
            "worker": {
                "host": self.config.host,
                "port": self.actual_port(),
                "mock": self.config.mock,
                "logLevel": self.config.log_level,
            },
            "gateway": gateway,
            "session": store.session_status(),
            "latency": latency,
            "benchmark": self.benchmark_snapshot(store),
            "keepalive": self
                .keepalive_service()
                .map(|service| service.stats_json())
                .unwrap_or(Value::Null),
            "system": lock(&self.system).snapshot(),
            "settings": store.state.settings,
            "configs": self.configs_view(store, engine),
            "stats": store.state.stats,
            "report": store.report_view(),
            "history": store
                .state
                .history
                .iter()
                .take(LOOT_LOG_PAGE_SIZE)
                .map(|entry| entry.lite())
                .collect::<Vec<_>>(),
            "historyTotal": store.history_count(),
            "pending": pending,
            "workerTokenPreview": store.state.worker_token_preview,
            "accessKeyPinned": !self.config.worker_token.is_empty(),
            "limits": {
                "delayMin": Limits::DELAY_MIN,
                "delayMax": Limits::DELAY_MAX,
                "qtyMax": Limits::QTY_MAX,
                "configsMax": Limits::CONFIGS_MAX,
                "ordersPerMinuteMax": Limits::ORDERS_PER_MINUTE_MAX,
            },
        })
    }
}

pub(super) async fn state_view(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let store = lock(&state.store);
    let engine = lock(&state.engine);
    let mut body = state.snapshot(&store, &engine);
    body["ok"] = Value::Bool(true);
    Json(body).into_response()
}

pub(super) fn current_snapshot(state: &AppState) -> Value {
    let store = lock(&state.store);
    let engine = lock(&state.engine);
    state.snapshot(&store, &engine)
}

pub(super) fn gateway_snapshot(state: &AppState) -> Option<Value> {
    lock(&state.gateway)
        .as_ref()
        .map(|gateway| gateway.stats_json())
}

pub(super) fn system_snapshot(state: &AppState) -> Value {
    lock(&state.system).snapshot()
}

pub(super) fn latency_snapshot(state: &AppState) -> Value {
    lock(&state.latency).snapshot()
}

pub(super) fn benchmark_snapshot(state: &AppState) -> Value {
    let store = lock(&state.store);
    state.benchmark_snapshot(&store)
}
