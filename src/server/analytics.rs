//! Chart endpoints backed by the store's event and price series.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::AppState;
use super::auth::Auth;
use crate::clock::now_ms;
use crate::constants::{
    EVENT_BUCKET_MS, EVENT_HISTORY_DAYS, EVENT_RETENTION_DAYS, PRICE_HISTORY_DAYS,
    PRICE_RETENTION_DAYS,
};
use crate::item_activity::{self, ActivityFilters};
use crate::lock::lock;

/// Bucketed marketplace-command counts for the activity chart: listing/price
/// updates and order fulfillment results. Buckets stay at the stored 5-minute
/// resolution; the deck sums them into 15m/30m/1h/2h windows so that boundaries
/// line up with the viewer's local clock, not the server's.
pub(super) async fn events_view(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let days = params
        .get("days")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, EVENT_HISTORY_DAYS);
    let now = now_ms();
    let store = lock(&state.store);
    // Buckets written before the marketplace-only scope cannot be split by
    // command, so they must not be presented as market activity.
    let since = (now - days * 86_400_000).max(store.event_scope_at());
    let points: Vec<Value> = store
        .event_series(since)
        .into_iter()
        .map(|(bucket_at, events)| json!({ "t": bucket_at, "n": events }))
        .collect();
    Json(json!({
        "ok": true,
        "days": days,
        "bucketMs": EVENT_BUCKET_MS,
        "since": since,
        "until": now,
        "retentionDays": EVENT_RETENTION_DAYS,
        "points": points,
    }))
    .into_response()
}

/// Ranked per-item activity for the item monitor.
///
/// Answers "which item ids was the market busiest on" for an arbitrary range,
/// including ids that were never added to the target fleet. Names come from the
/// catalogue; the first response for an unseen id spends a bounded slice of its
/// budget asking `item-info`, and later refreshes pick up whatever is left.
pub(super) async fn item_activity_view(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let filters = ActivityFilters::parse(&params, now_ms());
    // Everything that touches the store happens inside a short critical
    // section; the upstream lookups below run with no lock held.
    let (mut view, credentials, pending) = {
        let store = lock(&state.store);
        let view = item_activity::activity_view(&store, &filters);
        let pending = item_activity::pending_names(&store, &view.rows);
        (
            view,
            crate::session::credentials(&store.state.session),
            pending,
        )
    };

    if !pending.is_empty() {
        let resolved =
            item_activity::resolve_pending(&state.item_info, &credentials, &state.logger, pending)
                .await;
        let mut store = lock(&state.store);
        view.unresolved = item_activity::apply_resolved(&mut store, &mut view.rows, resolved);
        // The tracked flag is part of the row, so a target edited while the
        // table is open is reflected on the next poll without a second request.
        for row in view.rows.iter_mut() {
            row.tracked = store.find_config(&row.item_id).is_some();
        }
    }

    Json(json!({ "ok": true, "activity": view })).into_response()
}

/// Raw 1-minute closes for one target, used by the price chart's 3d/7d
/// periods. The deck buckets them client-side so every boundary follows the
/// viewer's local clock, and merges the live 24h window on top.
pub(super) async fn price_history_view(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let item_id = params
        .get("itemId")
        .or_else(|| params.get("item_id"))
        .map(|value| value.trim())
        .filter(|value| !value.is_empty());
    let Some(item_id) = item_id else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "itemId is required." })),
        )
            .into_response();
    };
    let days = params
        .get("days")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, PRICE_HISTORY_DAYS);
    let now = now_ms();
    let since = now - days * 86_400_000;
    let store = lock(&state.store);
    let points: Vec<Value> = store
        .price_series(item_id, since)
        .into_iter()
        .map(|point| {
            json!({
                "t": point.bucket_at,
                "p": point.price,
                "l": point.low,
                "h": point.high,
                "rl": point.raw_low,
                "rh": point.raw_high,
            })
        })
        .collect();
    Json(json!({
        "ok": true,
        "itemId": item_id,
        "days": days,
        "bucketMs": 60_000,
        "since": since,
        "until": now,
        "retentionDays": PRICE_RETENTION_DAYS,
        "points": points,
    }))
    .into_response()
}
