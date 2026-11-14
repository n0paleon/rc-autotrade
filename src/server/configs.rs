//! Target CRUD handlers and after-response label enrichment.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::AppState;
use super::auth::Auth;
use super::error::{error_body, fields_error};
use crate::lock::lock;
use crate::validation;

pub(super) async fn get_configs(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let store = lock(&state.store);
    let engine = lock(&state.engine);
    Json(json!({ "ok": true, "configs": state.configs_view(&store, &engine) })).into_response()
}

pub(super) async fn put_configs(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let list = if body.is_array() {
        body.clone()
    } else {
        body.get("configs").cloned().unwrap_or(Value::Null)
    };

    let mut store = lock(&state.store);
    let existing = store.config_ids();
    let values = match validation::validate_targets(&list, &existing) {
        Ok(values) => values,
        Err(fields) => {
            return fields_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Some targets are invalid.",
                fields,
            );
        }
    };

    if let Err(message) = store.set_configs(values.clone()) {
        state.logger.error("config", message);
        return error_body(StatusCode::INTERNAL_SERVER_ERROR, "Could not save targets.");
    }
    let mut engine = lock(&state.engine);
    for target in &values {
        engine.refresh_trend(&store, &target.item_id);
    }
    let view = state.configs_view(&store, &engine);
    drop(engine);
    drop(store);

    state.logger.info(
        "config",
        format!(
            "target list replaced ({} item{})",
            values.len(),
            if values.len() == 1 { "" } else { "s" }
        ),
    );
    // Bulk enrichment runs after the response, like `enrichMany(...).catch`.
    spawn_enrichment(
        state.clone(),
        values
            .iter()
            .map(|target| (target.item_id.clone(), target.item_type.clone()))
            .collect(),
    );
    state.broadcast("configs", Value::Array(view.clone()));
    Json(json!({ "ok": true, "configs": view })).into_response()
}

pub(super) async fn post_config(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let saved = {
        let mut store = lock(&state.store);
        let existing = store.config_ids();
        let target = match validation::validate_target(&body, &existing, None) {
            Ok(target) => target,
            Err(fields) => {
                return fields_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Target is invalid.",
                    fields,
                );
            }
        };
        let saved = store.upsert_config(target, None);
        let mut engine = lock(&state.engine);
        engine.refresh_trend(&store, &saved.item_id);
        saved
    };
    state
        .logger
        .info("config", format!("target added: {}", saved.item_id));

    // Resolve the auto label before replying when possible, with a 1.5 s
    // budget, so the deck receives a named target.
    if let Some(item) = enrich_item(&state, &saved.item_id, &saved.item_type).await {
        let mut store = lock(&state.store);
        store.apply_item_info(
            &saved.item_id,
            &item.label,
            &item.item_type,
            item.rarity_color.as_deref(),
        );
    }

    let view = {
        let store = lock(&state.store);
        let engine = lock(&state.engine);
        state.configs_view(&store, &engine)
    };
    state.broadcast("configs", Value::Array(view.clone()));
    (
        StatusCode::CREATED,
        Json(json!({ "ok": true, "configs": view })),
    )
        .into_response()
}

/// Resolves an item label with the 1.5 s budget. Never fails the caller: an
/// unresolved label just stays as the short item id.
async fn enrich_item(
    state: &Arc<AppState>,
    item_id: &str,
    item_type: &str,
) -> Option<crate::item_info::ItemInfo> {
    let credentials = {
        let store = lock(&state.store);
        crate::session::credentials(&store.state.session)
    };
    let future = state.item_info.preview(&credentials, item_id, item_type);
    match tokio::time::timeout(Duration::from_millis(1_500), future).await {
        Ok(Ok(item)) => Some(item),
        Ok(Err(message)) => {
            state
                .logger
                .warn("item-info", format!("enrichment failed: {message}"));
            None
        }
        Err(_) => {
            state
                .logger
                .warn("item-info", "enrichment timed out after 1500ms");
            None
        }
    }
}

/// Fire-and-forget enrichment used after edits and bulk replaces.
fn spawn_enrichment(state: Arc<AppState>, targets: Vec<(String, String)>) {
    tokio::spawn(async move {
        for (item_id, item_type) in targets {
            let Some(item) = enrich_item(&state, &item_id, &item_type).await else {
                continue;
            };
            let changed = {
                let mut store = lock(&state.store);
                store.apply_item_info(
                    &item_id,
                    &item.label,
                    &item.item_type,
                    item.rarity_color.as_deref(),
                )
            };
            if changed {
                let view = {
                    let store = lock(&state.store);
                    let engine = lock(&state.engine);
                    state.configs_view(&store, &engine)
                };
                state.broadcast("configs", Value::Array(view));
            }
        }
    });
}

pub(super) async fn patch_config(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Path(item_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let mut store = lock(&state.store);
    let Some(existing) = store.find_config(&item_id).cloned() else {
        return error_body(StatusCode::NOT_FOUND, "Target not found.");
    };

    // Merge: the stored row wins for identity, the body wins for everything it
    // actually contains, exactly like `{...existing, ...body, itemId}`.
    let mut merged = serde_json::to_value(&existing).unwrap_or(Value::Null);
    if let (Some(base), Some(patch)) = (merged.as_object_mut(), body.as_object()) {
        for (key, value) in patch {
            base.insert(key.clone(), value.clone());
        }
        base.insert(
            "itemId".to_string(),
            Value::String(existing.item_id.clone()),
        );
    }

    let target = match validation::validate_target(&merged, &[], Some(&existing.item_id)) {
        Ok(target) => target,
        Err(fields) => {
            return fields_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Target is invalid.",
                fields,
            );
        }
    };

    let saved = store.upsert_config(target, Some(&existing.item_id));
    state
        .logger
        .info("config", format!("target updated: {}", saved.item_id));
    let mut engine = lock(&state.engine);
    engine.refresh_trend(&store, &saved.item_id);
    let view = state.configs_view(&store, &engine);
    drop(engine);
    drop(store);

    // Edits are enriched without blocking the response.
    spawn_enrichment(
        state.clone(),
        vec![(saved.item_id.clone(), saved.item_type.clone())],
    );
    state.broadcast("configs", Value::Array(view.clone()));
    Json(json!({ "ok": true, "configs": view })).into_response()
}

pub(super) async fn delete_config(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Path(item_id): Path<String>,
) -> Response {
    let mut store = lock(&state.store);
    let Some(existing) = store.find_config(&item_id).cloned() else {
        return error_body(StatusCode::NOT_FOUND, "Target not found.");
    };
    store.remove_config(&existing.item_id);
    state.logger.info(
        "config",
        format!(
            "target removed: {}",
            if existing.label.is_empty() {
                existing.item_id.clone()
            } else {
                existing.label.clone()
            }
        ),
    );
    let view = {
        let engine = lock(&state.engine);
        state.configs_view(&store, &engine)
    };
    drop(store);

    state.broadcast("configs", Value::Array(view.clone()));
    Json(json!({ "ok": true, "configs": view })).into_response()
}
