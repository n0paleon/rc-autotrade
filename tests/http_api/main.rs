//! HTTP surface integration: auth gate, health, and target creation with
//! item-info auto-enrichment against a mock upstream.

mod activity;
mod benchmark;
mod configs;
mod state;

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use rollercoin_worker::api::ApiClient;
use rollercoin_worker::config::Config;
use rollercoin_worker::engine::Engine;
use rollercoin_worker::item_info::ItemInfoService;
use rollercoin_worker::latency::LatencyTracker;
use rollercoin_worker::logger::Logger;
use rollercoin_worker::server::{AppState, router};
use rollercoin_worker::store::{Store, now_ms};
use rollercoin_worker::system::SystemMonitor;

pub(crate) const TOKEN: &str = "test-token-123456";
pub(crate) const ITEM_ID: &str = "6319f7baa8ce530569ed16b9";

pub(crate) fn temp_dir() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rc-rust-http-{nanos}"))
}

pub(crate) fn config(data_dir: std::path::PathBuf, api_base: String) -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 0,
        worker_token: TOKEN.into(),
        data_dir,
        api_base: api_base.clone(),
        ws_url: "ws://127.0.0.1:1/ws".into(),
        cookie: String::new(),
        keep_alive_ms: 0,
        keep_alive_url: format!("{api_base}/api/marketplace/purchase-item"),
        system_sample_ms: 2_000,
        mock: false,
        mock_interval_ms: 1_400,
        log_level: "error".into(),
        cwd: std::env::temp_dir(),
    }
}

pub(crate) async fn item_info_handler(State(calls): State<Arc<Mutex<usize>>>) -> Json<Value> {
    *calls.lock().unwrap() += 1;
    Json(json!({
        "success": true,
        "data": {
            "name": { "en": "Hashboard" },
            "itemType": "mutation_component",
            "rarityGroup": { "title": { "en": "Rare" }, "baseHexColor": "#A52BFF" }
        }
    }))
}

pub(crate) async fn spawn_upstream(calls: Arc<Mutex<usize>>) -> String {
    let app = Router::new()
        .route("/api/marketplace/item-info", get(item_info_handler))
        .route(
            "/api/marketplace/purchase-item",
            post(|| async { Json(json!({})) }),
        )
        .with_state(calls);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

pub(crate) async fn spawn_worker(data_dir: std::path::PathBuf, api_base: String) -> String {
    let logger = Arc::new(Logger::new("error"));
    let mut store = Store::open(data_dir.clone(), logger.clone()).expect("store");
    store.ensure_worker_token(TOKEN).expect("token");
    // Give the SPA fallback something to serve so the HTML headers are testable.
    let dist = data_dir.join("dist");
    std::fs::create_dir_all(&dist).expect("dist");
    std::fs::write(
        dist.join("index.html"),
        "<!doctype html><html><body>deck</body></html>",
    )
    .expect("index");
    let engine = Engine::new(Arc::new(now_ms), logger.clone());
    let latency = Arc::new(Mutex::new(LatencyTracker::new(120, Arc::new(now_ms))));
    let api = Arc::new(ApiClient::with_default_timeout(&api_base));
    let item_info = Arc::new(ItemInfoService::new(api.clone(), logger.clone()));
    let system = Arc::new(Mutex::new(SystemMonitor::new(2_000, store.db_metrics())));
    let state = Arc::new(AppState::new(
        config(data_dir.clone(), api_base),
        store,
        engine,
        api,
        item_info,
        latency,
        system,
        logger,
    ));
    let app = router(state, data_dir.join("dist"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}
