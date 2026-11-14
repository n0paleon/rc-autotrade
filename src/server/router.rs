//! Route table, middleware wiring and the static deck fallback.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::http::header;
use axum::response::Response;
use axum::routing::{get, patch, post};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use super::AppState;
use super::analytics::{events_view, item_activity_view, price_history_view};
use super::auth::rotate_auth;
use super::backup::{get_backup, post_backup};
use super::configs::{delete_config, get_configs, patch_config, post_config, put_configs};
use super::history::{delete_history, history, history_detail, report};
use super::item_info::item_info;
use super::sessions::{
    delete_session, get_session, post_session, push_session, refresh_session_endpoint,
};
use super::settings::{control, patch_settings};
use super::system::{
    benchmark_history_view, benchmark_view, health, latency_probe, latency_view, system_view,
};
use super::views::state_view;
use super::ws::ws_handler;
use crate::constants::Limits;

pub(super) fn upstream_host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}

pub fn router(state: Arc<AppState>, dist_dir: PathBuf) -> Router {
    let index = dist_dir.join("index.html");
    let static_files = ServeDir::new(&dist_dir).not_found_service(ServeFile::new(index));

    Router::new()
        .route("/api/health", get(health))
        .route("/api/state", get(state_view))
        .route(
            "/api/configs",
            get(get_configs).put(put_configs).post(post_config),
        )
        .route(
            "/api/configs/{item_id}",
            patch(patch_config).delete(delete_config),
        )
        .route("/api/settings", patch(patch_settings))
        .route("/api/control", post(control))
        .route("/api/history", get(history).delete(delete_history))
        .route("/api/history/{id}", get(history_detail))
        .route("/api/report", get(report))
        .route("/api/events", get(events_view))
        .route("/api/item-activity", get(item_activity_view))
        .route("/api/price-history", get(price_history_view))
        .route(
            "/api/session",
            get(get_session).post(post_session).delete(delete_session),
        )
        .route("/api/auth/rotate", post(rotate_auth))
        .route(
            "/api/backup",
            get(get_backup)
                .post(post_backup)
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::backup::BACKUP_MAX_BYTES,
                )),
        )
        .route("/api/latency", get(latency_view))
        .route("/api/latency/probe", post(latency_probe))
        .route("/api/system", get(system_view))
        .route("/api/benchmark", get(benchmark_view))
        .route("/api/benchmark/history", get(benchmark_history_view))
        .route("/api/item-info", get(item_info))
        .route("/api/session/push", post(push_session))
        .route("/api/session/refresh", post(refresh_session_endpoint))
        .route("/ws", get(ws_handler))
        .fallback_service(static_files)
        .layer(axum::middleware::from_fn(security_headers))
        .layer(CorsLayer::permissive())
        .layer(axum::extract::DefaultBodyLimit::max(Limits::BODY_BYTES))
        .with_state(state)
}

/// Security headers sent on every response, plus the CSP added to HTML
/// documents. The deck's CSP allows Google Fonts, the control WebSocket, and
/// the profit sound clips hosted on storage.n0pal.net.
const HTML_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' data: https://fonts.gstatic.com; img-src 'self' data:; connect-src 'self' ws: wss:; media-src 'self' data: https://storage.n0pal.net; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use axum::http::HeaderValue;

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );

    let is_html = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("text/html"))
        .unwrap_or(false);
    if is_html {
        headers.insert(
            "content-security-policy",
            HeaderValue::from_static(HTML_CSP),
        );
    }
    response
}
