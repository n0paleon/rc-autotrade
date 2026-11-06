//! Regression test for the warm probe: it must use the order client's pool.
//!
//! The Bun backend shared one global `fetch` dispatcher, so the empty
//! `purchase-item` probe kept the exact socket used by real orders hot. A
//! `reqwest::Client` owns its own pool, so the Rust port needs to share the
//! client explicitly; this test fails with two connections when it does not.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{ConnectInfo, State};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use rollercoin_worker::api::{ApiClient, Credentials, PurchaseRequest};
use rollercoin_worker::keepalive::{KeepaliveConfig, KeepaliveService};
use rollercoin_worker::latency::LatencyTracker;
use rollercoin_worker::logger::Logger;

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<HashSet<SocketAddr>>>);

async fn record(
    State(seen): State<Seen>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    _body: Option<Json<Value>>,
) -> Json<Value> {
    seen.0.lock().unwrap().insert(peer);
    Json(json!({
        "success": true,
        "data": { "usersPurchaseOrder": "order-pool-1" }
    }))
}

async fn spawn_server() -> (String, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route("/probe", post(record))
        .route("/api/marketplace/purchase-item", post(record))
        .with_state(seen.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    (format!("http://{address}"), seen)
}

#[tokio::test]
async fn the_warm_probe_and_purchases_share_one_connection() {
    let (base, seen) = spawn_server().await;
    let api = ApiClient::with_default_timeout(&base);

    let keepalive = KeepaliveService::new(KeepaliveConfig {
        url: format!("{base}/probe"),
        interval_ms: 30_000,
        timeout_ms: 2_000,
        suspend_ms: 60_000,
        jitter_ratio: 0.0,
        credentials: Arc::new(|| ("token".into(), String::new())),
        should_probe: Arc::new(|| true),
        logger: Arc::new(Logger::new("error")),
        latency: Arc::new(Mutex::new(LatencyTracker::new(120, Arc::new(|| 0)))),
        last_request_at: api.last_request_at_handle(),
        http: api.http_client(),
    });

    let probe = keepalive.probe_once().await;
    assert!(probe.ok, "probe should reach the server: {probe:?}");

    let request = PurchaseRequest {
        item_id: "6319f7baa8ce530569ed16b9".into(),
        item_type: "miner".into(),
        qty: 1,
        unit_price_raw: 1.0,
    };
    let result = api.purchase(&Credentials::default(), &request).await;
    assert!(result.ok, "purchase should succeed: {result:?}");

    let connections = seen.0.lock().unwrap().len();
    assert_eq!(
        connections, 1,
        "the probe and the purchase used {connections} TCP connections; \
         KeepaliveConfig must receive ApiClient::http_client()"
    );
}
