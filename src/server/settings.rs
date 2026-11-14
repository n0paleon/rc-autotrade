//! Settings patch and runtime control handlers.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::auth::Auth;
use super::error::{error_body, fields_error};
use crate::lock::lock;
use crate::model::{Mode, Settings};
use crate::validation::{self, FieldErrors};

pub(super) async fn patch_settings(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let patch = match validation::validate_settings(&body) {
        Ok(patch) => patch,
        Err(fields) => {
            return fields_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Some settings are invalid.",
                fields,
            );
        }
    };

    let mut store = lock(&state.store);
    let resumed = patch.resumes();
    if let Err(fields) = check_arm_guard(&store.state.settings, &patch) {
        return fields_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Cannot arm live yet.",
            fields,
        );
    }
    if let Err(message) = store.patch_settings(patch) {
        state.logger.error("store", message);
        return error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save settings.",
        );
    }
    let settings = store.state.settings;
    drop(store);

    state.logger.info("config", "runtime settings updated");
    if resumed {
        state.wake_keepalive("trading resumed");
    }
    state.broadcast(
        "settings",
        serde_json::to_value(settings).unwrap_or(Value::Null),
    );
    Json(json!({ "ok": true, "settings": settings })).into_response()
}

/// Arms the worker out of `off` without ever arming live in one hop.
///
/// Going straight from off to live would put real money one click away from a
/// stopped worker, so that transition is refused: the caller has to pass
/// through shadow and confirm. Turning live *off* is always allowed.
fn check_arm_guard(
    current: &Settings,
    patch: &validation::SettingsPatch,
) -> Result<(), FieldErrors> {
    if patch.mode == Some(Mode::Live) && current.mode == Mode::Off {
        let mut fields = FieldErrors::new();
        fields.insert(
            "mode".into(),
            "Switch to shadow first and confirm the dry-run signals, then arm live.".into(),
        );
        return Err(fields);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub(super) struct ControlBody {
    action: String,
}

pub(super) async fn control(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<ControlBody>,
) -> Response {
    match body.action.as_str() {
        "off" | "shadow" | "live" | "toggle" | "pause" | "resume" => {
            let mut store = lock(&state.store);
            let current = store.state.settings.mode;
            let next = match body.action.as_str() {
                "off" | "pause" => Mode::Off,
                "shadow" => Mode::Shadow,
                "live" | "resume" => Mode::Live,
                // The header button is a panic switch in both directions: it
                // stops live trading, and it resumes into shadow — never
                // straight into live. Arming live is a deliberate act on the
                // runtime panel.
                _ if current == Mode::Live => Mode::Off,
                _ => Mode::Shadow,
            };
            let patch = validation::SettingsPatch {
                mode: Some(next),
                ..Default::default()
            };
            if let Err(fields) = check_arm_guard(&store.state.settings, &patch) {
                return fields_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Cannot arm live yet.",
                    fields,
                );
            }
            if let Err(message) = store.patch_settings(patch) {
                return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
            }
            let settings = store.state.settings;
            drop(store);
            state.logger.info(
                "control",
                match next {
                    Mode::Off => "trading off",
                    Mode::Shadow => "trading in shadow mode (no orders will be sent)",
                    Mode::Live => "trading live",
                },
            );
            state.broadcast(
                "settings",
                serde_json::to_value(settings).unwrap_or(Value::Null),
            );
            if next.probes() {
                state.wake_keepalive("trading resumed");
            }
            Json(json!({ "ok": true, "settings": settings })).into_response()
        }
        "reconnect" => {
            let gateway = lock(&state.gateway).clone();
            match gateway {
                Some(gateway) => {
                    gateway.reconnect("manual").await;
                    Json(json!({ "ok": true })).into_response()
                }
                None => error_body(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Market feed is not running.",
                ),
            }
        }
        _ => error_body(
            StatusCode::BAD_REQUEST,
            "action must be off, shadow, live, toggle or reconnect.",
        ),
    }
}
