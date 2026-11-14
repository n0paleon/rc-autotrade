use std::collections::HashMap;

use axum::http::HeaderMap;
use serde_json::json;

use super::Envelope;
use super::auth::protocol_auth;
use super::router::upstream_host;

#[test]
fn the_control_envelope_keeps_its_wire_shape() {
    let frame = serde_json::to_string(&Envelope {
        kind: "market",
        data: &json!({ "itemId": "abc", "hot": true }),
        at: 42,
    })
    .unwrap();
    assert_eq!(
        frame,
        r#"{"type":"market","data":{"hot":true,"itemId":"abc"},"at":42}"#
    );
}

#[test]
fn extracts_the_host_from_the_upstream_url() {
    assert_eq!(
        upstream_host("wss://nws.rollercoin.com/"),
        "nws.rollercoin.com"
    );
    assert_eq!(upstream_host("wss://example.com"), "example.com");
}

#[test]
fn protocol_auth_accepts_the_token_as_second_protocol() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "sec-websocket-protocol",
        "rcw.v1, super-secret-key".parse().unwrap(),
    );
    let (offered, token) = protocol_auth(&headers, &HashMap::new());
    assert!(offered);
    assert_eq!(token, "super-secret-key");
}

#[test]
fn protocol_auth_falls_back_to_the_query_parameter() {
    let headers = HeaderMap::new();
    let mut params = HashMap::new();
    params.insert("token".to_string(), "from-query".to_string());
    let (offered, token) = protocol_auth(&headers, &params);
    assert!(!offered);
    assert_eq!(token, "from-query");
}
