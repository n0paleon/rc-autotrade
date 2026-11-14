//! Single-item metadata lookup for the deck's item picker.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::AppState;
use super::auth::Auth;
use super::error::error_body;
use crate::lock::lock;
use crate::validation;

pub(super) async fn item_info(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(item_id) = validation::normalize_item_id(&Value::String(
        params.get("itemId").cloned().unwrap_or_default(),
    )) else {
        return error_body(
            StatusCode::UNPROCESSABLE_ENTITY,
            "A valid itemId is required.",
        );
    };
    let item_type = params
        .get("itemType")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "mutation_component".to_string());
    let valid_type = item_type.len() >= 2
        && item_type.len() <= 40
        && item_type
            .chars()
            .next()
            .map(|ch| ch.is_ascii_alphabetic())
            .unwrap_or(false)
        && item_type
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if !valid_type {
        return error_body(StatusCode::UNPROCESSABLE_ENTITY, "Invalid itemType.");
    }

    let (credentials, configured) = {
        let store = lock(&state.store);
        let configured = store.find_config(&item_id).is_some();
        (
            crate::session::credentials(&store.state.session),
            configured,
        )
    };

    let result = state
        .item_info
        .preview(&credentials, &item_id, &item_type)
        .await;
    match result {
        Ok(value) => {
            if configured {
                let mut store = lock(&state.store);
                store.apply_item_info(
                    &item_id,
                    &value.label,
                    &value.item_type,
                    value.rarity_color.as_deref(),
                );
                let view = state.configs_view(&store, &lock(&state.engine));
                drop(store);
                state.broadcast("configs", Value::Array(view));
            }
            Json(json!({
                "ok": true,
                "item": {
                    "itemId": value.item_id,
                    "itemName": value.item_name,
                    "rarity": value.rarity,
                    "rarityColor": value.rarity_color,
                    "itemType": value.item_type,
                    "label": value.label,
                }
            }))
            .into_response()
        }
        Err(message) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": message })),
        )
            .into_response(),
    }
}
