//! Session read/write endpoints and the manual refresh trigger.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::AppState;
use super::auth::Auth;
use super::error::{error_body, fields_error};
use super::reconnect_gateway;
use crate::lock::lock;
use crate::validation;

pub(super) async fn get_session(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let store = lock(&state.store);
    Json(json!({ "ok": true, "session": store.session_status() })).into_response()
}

#[axum::debug_handler]
pub(super) async fn post_session(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let patch = match validation::validate_session_input(&body) {
        Ok(patch) => patch,
        Err(fields) => {
            return fields_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid session data.",
                fields,
            );
        }
    };

    let (changed, status) = {
        let mut store = lock(&state.store);
        let before = store.state.session.access_token.clone();
        if let Err(message) = store.apply_session_patch(patch) {
            return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
        }
        let changed = before != store.state.session.access_token;
        let status = store.session_status();
        (changed, status)
    };
    state.broadcast("session", status.clone());
    state.wake_keepalive("session updated");

    if changed {
        // Manual credential replacement must re-handshake even when the feed
        // still reports "online": that socket is authenticated with the old
        // token, so a manual change always forces a reconnect.
        reconnect_gateway(&state, "session updated", true).await;
    }

    Json(json!({ "ok": true, "changed": changed, "session": status })).into_response()
}

pub(super) async fn delete_session(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let (changed, status) = {
        let mut store = lock(&state.store);
        let before = store.state.session.access_token.clone();
        let patch = validation::SessionPatch {
            clear_access_token: true,
            clear_refresh_token: true,
            clear_cookie: true,
            ..Default::default()
        };
        if let Err(message) = store.apply_session_patch(patch) {
            return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
        }
        (
            before != store.state.session.access_token,
            store.session_status(),
        )
    };
    state.logger.info(
        "session",
        "Rollercoin session cleared from the control deck",
    );
    state.broadcast("session", status.clone());
    if changed {
        reconnect_gateway(&state, "session cleared", true).await;
    }
    Json(json!({ "ok": true, "session": status })).into_response()
}

pub(super) async fn push_session(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let patch = match validation::validate_session_input(&body) {
        Ok(patch) => patch,
        Err(fields) => {
            return fields_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid session data.",
                fields,
            );
        }
    };
    let Some(access_token) = patch.access_token.clone() else {
        return error_body(StatusCode::BAD_REQUEST, "accessToken is required.");
    };
    let refresh_token = patch.refresh_token.clone().unwrap_or_default();

    let (accepted, session) = {
        let mut store = lock(&state.store);
        let accepted = crate::session::merge_pushed(
            &mut store,
            &access_token,
            &refresh_token,
            crate::store::now_ms(),
        );
        let session = json!({
            "accessToken": store.state.session.access_token,
            "refreshToken": store.state.session.refresh_token,
        });
        (accepted, session)
    };
    if accepted {
        state.wake_keepalive("bridge session");
        let status = {
            let store = lock(&state.store);
            store.session_status()
        };
        state.broadcast("session", status);
        // A bridge push replaces the live token, so the feed must re-handshake
        // or it keeps running (and eventually failing) on the previous one.
        reconnect_gateway(&state, "bridge session", true).await;
    }

    Json(json!({ "ok": true, "accepted": accepted, "session": session })).into_response()
}

pub(super) async fn refresh_session_endpoint(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
) -> Response {
    let refreshed = crate::runtime::refresh_session(&state).await;
    let status = {
        let store = lock(&state.store);
        store.session_status()
    };
    Json(json!({ "ok": true, "refreshed": refreshed, "session": status })).into_response()
}
