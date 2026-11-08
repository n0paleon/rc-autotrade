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
use rollercoin_worker::dns_probe::SocketMode;
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
    let port = base.rsplit(':').next().expect("port");
    // Use a hostname so the probe's fresh connection also exercises the DNS
    // path; the connector classifies IP literals too (covered below).
    let base = format!("http://localhost:{port}");
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
    assert_eq!(
        result.socket,
        SocketMode::Reused,
        "the purchase must reuse the socket the probe just opened"
    );
    assert!(
        result.dns.is_none(),
        "a reused socket must not carry a DNS sample"
    );

    let connections = seen.0.lock().unwrap().len();
    assert_eq!(
        connections, 1,
        "the probe and the purchase used {connections} TCP connections; \
         KeepaliveConfig must receive ApiClient::http_client()"
    );
}

/// A DNS name (not an IP literal) is required: hyper skips the resolver for IP
/// hosts, so keep-alive detection has no signal there. `localhost` is the one
/// name every machine resolves without external DNS.
#[tokio::test]
async fn purchases_report_a_new_then_a_reused_socket() {
    let (base, seen) = spawn_server().await;
    let port = base.rsplit(':').next().expect("port");
    let api = ApiClient::with_default_timeout(&format!("http://localhost:{port}"));
    let request = PurchaseRequest {
        item_id: "6319f7baa8ce530569ed16b9".into(),
        item_type: "miner".into(),
        qty: 1,
        unit_price_raw: 1.0,
    };

    let first = api.purchase(&Credentials::default(), &request).await;
    assert!(first.ok, "first purchase should succeed: {first:?}");
    assert_eq!(
        first.socket,
        SocketMode::New,
        "the first purchase must resolve the host and open a connection"
    );
    let lookup = first
        .dns
        .as_ref()
        .expect("the first purchase must keep its lookup");
    assert!(lookup.ms >= 0.0);
    assert_eq!(lookup.error, None);
    assert!(lookup.address.is_some());
    assert!(matches!(lookup.family, Some(4) | Some(6)));

    let second = api.purchase(&Credentials::default(), &request).await;
    assert!(second.ok, "second purchase should succeed: {second:?}");
    assert_eq!(
        second.socket,
        SocketMode::Reused,
        "the second purchase must reuse the pooled keep-alive socket"
    );
    assert!(
        second.dns.is_none(),
        "a reused socket performs no DNS lookup"
    );

    // The labels are not derived from a self-fulfilling client-side guess: the
    // server's accept side must confirm that both purchases arrived on the same
    // TCP connection.
    assert_eq!(
        seen.0.lock().unwrap().len(),
        1,
        "NEW then KEEP-ALIVE must correspond to exactly one server-side TCP connection"
    );

    // Negative control: a different client has a different pool, so its first
    // request must open a second TCP connection and be labelled NEW again.
    let fresh = ApiClient::with_default_timeout(&format!("http://localhost:{port}"));
    let third = fresh.purchase(&Credentials::default(), &request).await;
    assert!(third.ok, "third purchase should succeed: {third:?}");
    assert_eq!(
        third.socket,
        SocketMode::New,
        "a new client pool must report NEW"
    );
    assert!(third.dns.is_some(), "NEW must carry a real DNS sample");
    assert_eq!(
        seen.0.lock().unwrap().len(),
        2,
        "the fresh client must open a second server-side TCP connection"
    );
}

/// IP literals never touch the resolver, so the old DNS-only signal reported
/// `unknown` for every purchase against them. The connector observation must
/// still tell a fresh socket from a reused one.
#[tokio::test]
async fn ip_literal_purchases_are_classified_by_the_connector() {
    let (base, seen) = spawn_server().await;
    let api = ApiClient::with_default_timeout(&base);
    let request = PurchaseRequest {
        item_id: "6319f7baa8ce530569ed16b9".into(),
        item_type: "miner".into(),
        qty: 1,
        unit_price_raw: 1.0,
    };

    let first = api.purchase(&Credentials::default(), &request).await;
    assert!(first.ok, "first purchase should succeed: {first:?}");
    assert_eq!(first.socket, SocketMode::New);
    assert!(first.dns.is_none(), "an IP literal never performs DNS");

    let second = api.purchase(&Credentials::default(), &request).await;
    assert!(second.ok, "second purchase should succeed: {second:?}");
    assert_eq!(
        second.socket,
        SocketMode::Reused,
        "the second IP purchase must reuse the pooled socket"
    );
    assert_eq!(
        seen.0.lock().unwrap().len(),
        1,
        "both IP purchases must arrive on one server-side TCP connection"
    );
}
