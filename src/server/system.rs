//! Health, latency, system and benchmark endpoints.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::AppState;
use super::auth::Auth;
use crate::benchmark;
use crate::clock::now_ms;
use crate::constants::{APP_NAME, APP_VERSION};
use crate::lock::lock;

pub(super) async fn health(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({
        "ok": true,
        "name": APP_NAME,
        "version": APP_VERSION,
        "mock": state.config.mock,
        "uptimeMs": now_ms() - state.started_at,
    }))
    .into_response()
}

pub(super) async fn latency_view(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let latency = lock(&state.latency).snapshot();
    Json(json!({ "ok": true, "latency": latency })).into_response()
}

pub(super) async fn latency_probe(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let probe = match state.keepalive_service() {
        Some(service) if service.enabled() => Some(service.probe_once().await),
        _ => None,
    };
    let latency = lock(&state.latency).snapshot();
    let probe_json = match probe {
        Some(result) => result.to_json(),
        None => json!({ "ok": false, "skipped": true, "reason": "keepalive disabled" }),
    };
    state.broadcast("latency", latency.clone());
    Json(json!({ "ok": true, "latency": latency, "probe": probe_json })).into_response()
}

pub(super) async fn system_view(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let system = lock(&state.system).snapshot();
    Json(json!({ "ok": true, "system": system })).into_response()
}

pub(super) async fn benchmark_view(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let benchmark = {
        let store = lock(&state.store);
        state.benchmark_snapshot(&store)
    };
    Json(json!({ "ok": true, "benchmark": benchmark })).into_response()
}

pub(super) fn default_history_bucket(hours: i64) -> i64 {
    if hours <= 1 {
        5 * 60_000
    } else if hours <= 6 {
        15 * 60_000
    } else if hours <= 24 {
        60 * 60_000
    } else {
        6 * 60 * 60_000
    }
}

pub(super) async fn benchmark_history_view(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let now = now_ms();
    let hours = params
        .get("hours")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(24)
        .clamp(1, benchmark::HISTORY_MAX_HOURS);
    let bucket_ms = params
        .get("bucket")
        .and_then(|key| benchmark::bucket_ms_from_key(key))
        .unwrap_or_else(|| default_history_bucket(hours));
    let tz_offset_ms = params
        .get("tzOffsetMs")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0)
        .clamp(-14 * 3_600_000, 14 * 3_600_000);
    let stage = params
        .get("stage")
        .cloned()
        .unwrap_or_else(|| "total".to_string());
    let since = now - hours * 3_600_000;

    // Decoding up to 20k timing rows is not frame-sized work; yield the reactor
    // thread while SQLite and the aggregation run.
    let history = crate::blocking::run({
        let state = state.clone();
        let stage = stage.clone();
        move || {
            let store = lock(&state.store);
            let samples =
                store.benchmark_samples_between(since, now, benchmark::HISTORY_MAX_SAMPLES);
            drop(store);
            benchmark::history(&samples, since, now, bucket_ms, tz_offset_ms, &stage)
        }
    });

    Json(json!({ "ok": true, "history": history })).into_response()
}
