//! Loot log pages, per-order detail and the reset endpoint.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::AppState;
use super::auth::Auth;
use super::error::error_body;
use crate::constants::{LOOT_LOG_MAX_PAGE_SIZE, LOOT_LOG_PAGE_SIZE};
use crate::lock::lock;
use crate::store::HistoryQuery;

pub(super) async fn history(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(LOOT_LOG_PAGE_SIZE)
        .min(LOOT_LOG_MAX_PAGE_SIZE);
    let offset = params
        .get("offset")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);

    let query = HistoryQuery {
        q: params.get("q").cloned().unwrap_or_default(),
        status: params.get("status").cloned().unwrap_or_default(),
        profit: params.get("profit").cloned().unwrap_or_default(),
        limit: Some(limit),
        offset,
        before_at: params
            .get("beforeAt")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0),
        before_id: params.get("beforeId").cloned().unwrap_or_default(),
    };

    // Even an unfiltered page runs a COUNT(*) over the whole log.
    let page = crate::blocking::run(|| {
        let store = lock(&state.store);
        store.history_page(query)
    });
    match page {
        Ok(page) => Json(json!({
            "ok": true,
            "entries": page.entries,
            "total": page.total,
            "limit": page.limit,
            "offset": page.offset,
            "hasMore": page.has_more,
        }))
        .into_response(),
        Err(message) => error_body(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

/// The two heavy JSON columns for one loot row.
///
/// The list endpoints stay lean on purpose (see `Store::order_detail`); the
/// detail modal calls this for the single entry it is showing.
pub(super) async fn history_detail(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let entry = crate::blocking::run(|| {
        let store = lock(&state.store);
        store.order_detail(&id)
    });
    match entry {
        Ok(Some(entry)) => Json(json!({ "ok": true, "entry": entry })).into_response(),
        Ok(None) => error_body(StatusCode::NOT_FOUND, "Unknown order id."),
        Err(message) => error_body(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

pub(super) async fn report(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let store = lock(&state.store);
    Json(json!({ "ok": true, "report": store.report_view() })).into_response()
}

pub(super) async fn delete_history(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let (stats, report, configs, latency, benchmark) = {
        let mut store = lock(&state.store);
        store.reset_stats();
        let stats = store.state.stats;
        let report = store.report_view();
        let mut engine = lock(&state.engine);
        engine.reset_counters();
        let configs = state.configs_view(&store, &engine);
        drop(engine);
        let mut tracker = lock(&state.latency);
        tracker.reset();
        let latency = tracker.snapshot();
        let benchmark = state.benchmark_snapshot(&store);
        (stats, report, configs, latency, benchmark)
    };

    state.logger.info(
        "stats",
        "lifetime stats, per-target fill rates and loot log reset",
    );
    state.broadcast("stats", serde_json::to_value(stats).unwrap_or(Value::Null));
    state.broadcast(
        "report",
        serde_json::to_value(report).unwrap_or(Value::Null),
    );
    state.broadcast("latency", latency);
    state.broadcast("benchmark", benchmark);
    state.broadcast("configs", Value::Array(configs));
    state.broadcast("history-cleared", json!({}));
    Json(json!({ "ok": true, "stats": stats })).into_response()
}
