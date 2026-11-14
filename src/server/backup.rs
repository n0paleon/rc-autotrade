//! Backup export (JSON / SQLite snapshot) and restore endpoints.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::AppState;
use super::auth::Auth;
use super::error::error_body;
use crate::lock::lock;

pub(super) async fn get_backup(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let format = params
        .get("format")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "json".to_string());
    let now = crate::store::now_ms();

    match format.as_str() {
        "json" => {
            let include_credentials = params
                .get("credentials")
                .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false);
            // Building the export walks every row and encodes a large JSON
            // document; keep it off the reactor thread.
            let built = crate::blocking::run(|| {
                let store = lock(&state.store);
                crate::backup::build_backup(&store, include_credentials, now)
                    .map(|payload| serde_json::to_vec(&payload).unwrap_or_default())
            });
            let body = match built {
                Ok(body) => body,
                Err(message) => {
                    return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
                }
            };
            attachment(
                axum::body::Body::from(body),
                "application/json; charset=utf-8",
                &crate::backup::backup_filename(now),
            )
        }
        "sqlite" | "db" => {
            // `VACUUM INTO` copies the whole database and can take a while.
            let snapshot = crate::blocking::run(|| {
                let store = lock(&state.store);
                crate::backup::create_database_snapshot(&store, "export", now)
            });
            let path = match snapshot {
                Ok(path) => path,
                Err(message) => {
                    return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
                }
            };
            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    let _ = tokio::fs::remove_file(&path).await;
                    return error_body(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("could not read the snapshot: {error}"),
                    );
                }
            };
            let _ = tokio::fs::remove_file(&path).await;
            attachment(
                axum::body::Body::from(bytes),
                "application/vnd.sqlite3",
                &crate::backup::database_filename(now),
            )
        }
        _ => error_body(
            StatusCode::BAD_REQUEST,
            "format must be json, sqlite or db.",
        ),
    }
}

pub(super) fn attachment(body: axum::body::Body, content_type: &str, filename: &str) -> Response {
    use axum::http::HeaderValue;

    let mut response = Response::new(body);
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(super) async fn post_backup(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > crate::backup::BACKUP_MAX_BYTES {
        return error_body(
            StatusCode::PAYLOAD_TOO_LARGE,
            "The backup file is too large.",
        );
    }
    let raw: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return error_body(StatusCode::BAD_REQUEST, "The body must be valid JSON."),
    };

    let now = crate::store::now_ms();
    let parsed = crate::backup::parse_backup(&raw, now);
    if !parsed.ok {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "ok": false,
                "error": "This file is not a valid backup for this worker.",
                "fields": parsed.errors,
            })),
        )
            .into_response();
    }
    let Some(value) = parsed.value else {
        return error_body(StatusCode::BAD_REQUEST, "The backup has no data.");
    };

    let restore_credentials = params
        .get("credentials")
        .map(|flag| !matches!(flag.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .unwrap_or(true);

    // A restore rewrites every table (and writes a safety snapshot first), so
    // it runs on a blocking-friendly path instead of parking a tokio worker.
    let restored = crate::blocking::run(|| {
        let mut store = lock(&state.store);
        let previous_token = store.state.session.access_token.clone();
        crate::backup::restore_backup(&mut store, &value, restore_credentials, true, now)
            .map(|result| (previous_token, result))
    });
    let (previous_token, restored) = match restored {
        Ok(restored) => restored,
        Err(message) => {
            state
                .logger
                .error("backup", format!("restore failed: {message}"));
            return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
        }
    };

    // The restore replaced every target and the market cache, so the engine's
    // live copy of that world is no longer valid.
    crate::blocking::run(|| {
        let store = lock(&state.store);
        let mut engine = lock(&state.engine);
        engine.reset_market_state();
        engine.hydrate_market(&store);
        for target in store.state.configs.clone() {
            engine.refresh_trend(&store, &target.item_id);
        }
    });
    state.wake_keepalive("backup restored");
    state.logger.info(
        "backup",
        format!(
            "restored {} targets and {} loot entries",
            restored.targets, restored.orders
        ),
    );

    let token_changed = {
        let store = lock(&state.store);
        previous_token != store.state.session.access_token
    };
    if restore_credentials && restored.credentials_restored && token_changed {
        let gateway = lock(&state.gateway).clone();
        if let Some(gateway) = gateway {
            gateway.reconnect("backup restored").await;
        }
    }

    state.broadcast("history-cleared", json!({}));
    let snapshot = crate::blocking::run(|| {
        let store = lock(&state.store);
        let engine = lock(&state.engine);
        state.snapshot(&store, &engine)
    });
    state.broadcast("snapshot", snapshot);

    Json(json!({
        "ok": true,
        "restored": {
            "counts": {
                "targets": restored.targets,
                "orders": restored.orders,
                "reportProducts": restored.report_products,
            },
            "credentialsRestored": restored.credentials_restored,
            "safetyBackup": restored.safety_backup,
        },
        "warnings": parsed.warnings,
    }))
    .into_response()
}
