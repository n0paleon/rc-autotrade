//! Shared response helpers for the API handlers.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::validation;

pub(super) fn error_body(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "ok": false, "error": message }))).into_response()
}

pub(super) fn fields_error(
    status: StatusCode,
    message: &str,
    fields: validation::FieldErrors,
) -> Response {
    (
        status,
        Json(json!({ "ok": false, "error": message, "fields": fields })),
    )
        .into_response()
}
