//! Bearer-token guard, subprotocol auth and worker access-key rotation.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::AppState;
use super::error::error_body;
use crate::lock::lock;

/// Bearer-token guard for every `/api/*` route except `/api/health`.
pub struct Auth;

impl FromRequestParts<Arc<AppState>> for Auth {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .unwrap_or_default();

        let store = lock(&state.store);
        if store.verify_worker_token(token) {
            return Ok(Auth);
        }
        Err(error_body(
            StatusCode::UNAUTHORIZED,
            "Missing or invalid worker access key.",
        ))
    }
}

pub(super) fn protocol_auth(
    headers: &HeaderMap,
    params: &HashMap<String, String>,
) -> (bool, String) {
    let protocols = headers
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();

    let offered = protocols.iter().any(|value| value == "rcw.v1");
    let token = protocols
        .iter()
        .find(|value| value.as_str() != "rcw.v1")
        .cloned()
        .or_else(|| params.get("token").cloned())
        .unwrap_or_default();
    (offered, token)
}

pub(super) async fn rotate_auth(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    if !state.config.worker_token.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": "Access key is pinned by WORKER_TOKEN. Change the environment variable to rotate it."
            })),
        )
            .into_response();
    }

    let mut store = lock(&state.store);
    let token = match store.rotate_worker_token() {
        Ok(token) => token,
        Err(message) => return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message),
    };
    drop(store);
    state.logger.warn(
        "auth",
        "worker access key rotated; revoking control sockets",
    );
    // Close every open control socket with 4003; the deck treats that code as
    // "reconnect with the new key" (4001 stays reserved for a bad key).
    let next = *state.auth_epoch.borrow() + 1;
    state.auth_epoch.send_replace(next);
    Json(json!({
        "ok": true,
        "token": token,
        "preview": crate::store::preview_token(&token),
    }))
    .into_response()
}
