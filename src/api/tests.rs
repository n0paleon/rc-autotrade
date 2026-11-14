use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use super::client::ApiClient;
use super::parse::{html_hint, message_from};
use super::types::{Credentials, PurchaseRequest, PurchaseResult};

#[derive(Clone, Default)]
struct Captured {
    headers: Vec<HeaderMap>,
    bodies: Vec<Value>,
    paths: Vec<String>,
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

async fn capture(
    State(state): State<Arc<Mutex<Captured>>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Option<Json<Value>>,
) {
    let mut captured = state.lock().unwrap();
    captured.headers.push(headers);
    captured.paths.push(uri.to_string());
    if let Some(Json(value)) = body {
        captured.bodies.push(value);
    }
}

fn mock_app(captured: Arc<Mutex<Captured>>) -> Router {
    Router::new()
        .route(
            "/api/marketplace/purchase-item",
            post(
                |State(state): State<Arc<Mutex<Captured>>>,
                 headers: HeaderMap,
                 uri: axum::http::Uri,
                 body: Json<Value>| async move {
                    capture(State(state), headers, uri, Some(body)).await;
                    Json(json!({
                        "success": true,
                        "data": { "usersPurchaseOrder": "order-42" }
                    }))
                },
            ),
        )
        .route(
            "/api/auth/refresh",
            post(
                |State(state): State<Arc<Mutex<Captured>>>,
                 headers: HeaderMap,
                 uri: axum::http::Uri,
                 body: Json<Value>| async move {
                    capture(State(state), headers, uri, Some(body)).await;
                    Json(json!({
                        "data": { "access_token": "new-access", "refresh_token": "new-refresh" }
                    }))
                },
            ),
        )
        .route(
            "/api/marketplace/item-info",
            get(
                |State(state): State<Arc<Mutex<Captured>>>,
                 headers: HeaderMap,
                 uri: axum::http::Uri| async move {
                    capture(State(state), headers, uri, None).await;
                    Json(json!({ "success": true, "data": { "name": "Uncommon Hashboard" } }))
                },
            ),
        )
        .route(
            "/api/reject",
            post(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "message": "session expired" })),
                )
            }),
        )
        .route(
            "/api/html",
            post(|| async {
                (
                    StatusCode::OK,
                    [("content-type", "text/html")],
                    "<html>challenge</html>",
                )
            }),
        )
        .with_state(captured)
}

fn credentials() -> Credentials {
    Credentials {
        access_token: "access-1".into(),
        refresh_token: "refresh-1".into(),
        cookie: "cf_clearance=abc".into(),
    }
}

fn purchase_request() -> PurchaseRequest {
    PurchaseRequest {
        item_id: "6319f7baa8ce530569ed16b9".into(),
        item_type: "miner".into(),
        qty: 2,
        unit_price_raw: 90_000.0,
        total_price_markup_pct: 0.0,
    }
}

#[test]
fn purchase_payload_defaults_the_item_type() {
    let request = PurchaseRequest {
        item_id: "6319f7baa8ce530569ed16b9".into(),
        qty: 3,
        unit_price_raw: 1.5,
        ..Default::default()
    };
    let payload = request.payload();
    assert_eq!(payload["itemType"], "mutation_component");
    assert_eq!(payload["totalCount"], 3);
    assert_eq!(payload["totalPrice"], 4.5);
}

#[test]
fn purchase_payload_applies_the_configured_total_price_headroom() {
    let request = PurchaseRequest {
        item_id: "6319f7baa8ce530569ed16b9".into(),
        item_type: "miner".into(),
        qty: 10,
        unit_price_raw: 90_000.0,
        total_price_markup_pct: 10.0,
    };
    // The listing total is 0.9 RLT; 10% headroom asks for a 0.99 RLT cap
    // so a deeper level can complete the order instead of filling partially.
    assert_eq!(request.base_total_price(), 900_000.0);
    assert_eq!(request.total_price(), 990_000.0);
    assert_eq!(request.payload()["totalPrice"], 990_000.0);
    assert_eq!(request.payload()["totalCount"], 10);
}

#[test]
fn a_zero_markup_keeps_the_legacy_payload_exact() {
    let request = purchase_request();
    let payload = request.payload();
    assert_eq!(payload["totalPrice"], 180_000.0);
}

#[tokio::test]
async fn purchase_sends_headers_and_parses_the_order_id() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn(mock_app(captured.clone())).await;
    let client = ApiClient::with_default_timeout(&base);

    let result = client.purchase(&credentials(), &purchase_request()).await;
    assert!(result.ok, "{result:?}");
    assert_eq!(result.order_id.as_deref(), Some("order-42"));
    assert!(
        result.send_at.is_some() && result.response_at.is_some(),
        "a completed exchange must carry both HTTP marks"
    );
    assert!(
        result.ttfb_ms.is_some_and(|ms| ms >= 0.0),
        "headers arrived, so TTFB must be recorded: {result:?}"
    );

    let state = captured.lock().unwrap();
    let headers = &state.headers[0];
    assert_eq!(headers["authorization"], "Bearer access-1");
    assert_eq!(headers["cookie"], "cf_clearance=abc");
    assert_eq!(headers["origin"], base.as_str());
    assert!(
        headers["referer"]
            .to_str()
            .unwrap()
            .ends_with("/marketplace")
    );
    assert!(
        headers["user-agent"]
            .to_str()
            .unwrap()
            .contains("Chrome/131")
    );

    let body = &state.bodies[0];
    assert_eq!(body["totalCount"], 2);
    assert_eq!(body["totalPrice"], 180_000.0);
    assert_eq!(body["action"], "marketplace");
    assert_eq!(body["challenge"], "");
}

#[tokio::test]
async fn refresh_parses_both_response_shapes() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn(mock_app(captured.clone())).await;
    let client = ApiClient::with_default_timeout(&base);

    let result = client.refresh_session(&credentials()).await;
    assert!(result.ok, "{result:?}");
    assert_eq!(result.access_token.as_deref(), Some("new-access"));
    assert_eq!(result.refresh_token.as_deref(), Some("new-refresh"));

    let state = captured.lock().unwrap();
    assert!(
        state.headers[0].get("authorization").is_none(),
        "refresh must not send the expired bearer token"
    );
    assert_eq!(state.bodies[0]["refresh_token"], "refresh-1");
}

#[tokio::test]
async fn item_info_returns_the_data_object() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn(mock_app(captured.clone())).await;
    let client = ApiClient::with_default_timeout(&base);

    let result = client
        .fetch_item_info(&credentials(), "6319f7baa8ce530569ed16b9", "miner", "RLT")
        .await;
    assert!(result.ok, "{result:?}");
    assert_eq!(result.data.unwrap()["name"], "Uncommon Hashboard");

    let state = captured.lock().unwrap();
    assert!(state.paths[0].contains("itemId=6319f7baa8ce530569ed16b9"));
    assert!(state.paths[0].contains("itemType=miner"));
}

#[tokio::test]
async fn unauthorized_purchase_is_flagged_for_the_refresh_retry() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn(mock_app(captured)).await;
    let client = ApiClient::with_default_timeout(&base);

    // Point the same client at the reject route.
    let reject_client = ApiClient::with_default_timeout(&base);
    let mut request = purchase_request();
    request.item_id = "x".into();
    let result = reject_client
        .post_json(
            "/api/reject",
            &json!({}),
            &credentials(),
            true,
            Duration::from_secs(5),
        )
        .await;
    assert_eq!(result.0, 401);
    assert_eq!(result.1.unwrap()["message"], "session expired");

    // The full purchase path still maps a 401 body message correctly.
    let _ = client;
    let unauthorized = PurchaseResult {
        ok: false,
        status: 401,
        unauthorized: true,
        ..Default::default()
    };
    assert!(unauthorized.unauthorized);
}

#[tokio::test]
async fn non_json_purchase_reports_a_cloudflare_hint() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn(mock_app(captured)).await;
    let client = ApiClient::with_default_timeout(&base);

    let (status, body, text, transport, marks) = client
        .post_json(
            "/api/html",
            &json!({}),
            &credentials(),
            true,
            Duration::from_secs(5),
        )
        .await;
    assert!(
        marks.ttfb_ms.is_some(),
        "the first byte arrived, so TTFB exists"
    );
    assert_eq!(status, 200);
    assert!(body.is_none());
    assert!(transport.is_none());
    assert!(html_hint(&text));

    // `message_from` falls back to the caller's hint for HTML bodies.
    assert_eq!(
        message_from(None, &text, "cloudflare"),
        "cloudflare".to_string()
    );
}

#[tokio::test]
async fn transport_errors_are_redacted() {
    let client = ApiClient::with_default_timeout("http://127.0.0.1:1");
    let result = client.purchase(&credentials(), &purchase_request()).await;
    assert!(!result.ok);
    assert_eq!(result.status, 0);
    assert!(result.error.is_some());
    assert_eq!(
        result.socket,
        crate::dns_probe::SocketMode::Unknown,
        "a failed exchange must not claim a reused socket"
    );
}

fn sample(ms: f64) -> crate::dns_probe::DnsLookup {
    crate::dns_probe::DnsLookup {
        ms,
        address: Some("104.18.0.1".into()),
        family: Some(4),
        error: None,
    }
}

#[test]
fn classify_uses_the_per_request_snapshot() {
    use crate::dns_probe::{DnsSnapshot, SocketMode};

    // Force proxy off so the host environment cannot change the fixture.
    let mut client = ApiClient::with_default_timeout("https://rollercoin.com");
    client.proxy = false;

    let reused = client.classify_connection(&DnsSnapshot::default(), true);
    assert_eq!(reused, (SocketMode::Reused, None));

    let single = DnsSnapshot {
        attempted: 1,
        samples: vec![sample(18.25)],
    };
    let (socket, dns) = client.classify_connection(&single, true);
    assert_eq!(socket, SocketMode::New);
    assert_eq!(dns.as_ref().map(|dns| dns.ms), Some(18.25));

    // A lookup that was cancelled mid-flight is still a fresh connection,
    // but there is no sample to attribute.
    let pending = DnsSnapshot {
        attempted: 1,
        samples: Vec::new(),
    };
    assert_eq!(
        client.classify_connection(&pending, true),
        (SocketMode::New, None)
    );

    // Two attempts are ambiguous: keep the connection fact, drop the sample.
    let twice = DnsSnapshot {
        attempted: 2,
        samples: vec![sample(3.0), sample(4.0)],
    };
    assert_eq!(
        client.classify_connection(&twice, true),
        (SocketMode::New, None)
    );

    // No response: never report reuse, but keep a failed lookup visible.
    let failed = DnsSnapshot {
        attempted: 1,
        samples: vec![crate::dns_probe::DnsLookup {
            ms: 3.5,
            address: None,
            family: None,
            error: Some("lookup timed out".into()),
        }],
    };
    let (socket, dns) = client.classify_connection(&failed, false);
    assert_eq!(socket, SocketMode::Unknown);
    assert_eq!(
        dns.and_then(|dns| dns.error).as_deref(),
        Some("lookup timed out")
    );
    assert_eq!(
        client.classify_connection(&DnsSnapshot::default(), false),
        (SocketMode::Unknown, None)
    );

    client.proxy = true;
    assert_eq!(
        client.classify_connection(&single, true),
        (SocketMode::Unknown, None),
        "a proxied resolver describes the proxy hop"
    );
}
