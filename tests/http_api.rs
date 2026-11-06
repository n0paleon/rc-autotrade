//! HTTP surface integration: auth gate, health, and target creation with
//! item-info auto-enrichment against a mock upstream.

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

const TOKEN: &str = "test-token-123456";
const ITEM_ID: &str = "6319f7baa8ce530569ed16b9";

fn temp_dir() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rc-rust-http-{nanos}"))
}

fn config(data_dir: std::path::PathBuf, api_base: String) -> Config {
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

async fn item_info_handler(State(calls): State<Arc<Mutex<usize>>>) -> Json<Value> {
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

async fn spawn_upstream(calls: Arc<Mutex<usize>>) -> String {
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

async fn spawn_worker(data_dir: std::path::PathBuf, api_base: String) -> String {
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

#[tokio::test]
async fn health_is_public_and_state_requires_the_key() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let base = spawn_worker(dir.clone(), upstream).await;

    let client = reqwest::Client::new();
    let health = client
        .get(format!("{base}/api/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    let body: Value = health.json().await.unwrap();
    assert_eq!(body["ok"], true);

    let unauthorized = client
        .get(format!("{base}/api/state"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let authorized = client
        .get(format!("{base}/api/state"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(authorized.status(), 200);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn creating_a_target_enriches_its_label_from_item_info() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls.clone()).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/api/configs"))
        .bearer_auth(TOKEN)
        .json(&json!({
            "itemId": ITEM_ID,
            "targetPrice": 0.1,
            "desiredQty": 10,
            "highStockThreshold": 20,
            "minAcceptableQty": 1
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.unwrap();
    let config = &body["configs"][0];
    assert_eq!(config["itemId"], ITEM_ID);
    // The 1.5 s enrichment budget resolved the label before the reply.
    assert_eq!(config["label"], "Rare Hashboard");
    assert_eq!(config["rarityColor"], "a52bff");
    assert!(*upstream_calls.lock().unwrap() >= 1);

    // The enrichment is persisted, not just decorated in the response.
    let state: Value = client
        .get(format!("{base}/api/state"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["configs"][0]["label"], "Rare Hashboard");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn invalid_targets_return_field_errors() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/api/configs"))
        .bearer_auth(TOKEN)
        .json(&json!({ "itemId": "x", "targetPrice": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["fields"]["itemId"].as_str().is_some());
    assert!(body["fields"]["targetPrice"].as_str().is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn security_headers_match_the_bun_worker() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let api = client
        .get(format!("{base}/api/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(api.headers()["x-content-type-options"], "nosniff");
    assert_eq!(api.headers()["x-frame-options"], "DENY");
    assert_eq!(api.headers()["referrer-policy"], "no-referrer");
    assert!(api.headers().get("content-security-policy").is_none());

    let page = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(
        page.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    let csp = page.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("connect-src 'self' ws: wss:"));
    assert!(csp.contains("frame-ancestors 'none'"));

    let _ = std::fs::remove_dir_all(&dir);
}
