use std::collections::HashMap;

use axum::http::HeaderMap;
use serde_json::json;

use super::Envelope;
use super::auth::protocol_auth;
use super::router::upstream_host;
use super::views::trim_runtime_history;

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
fn compact_snapshot_keeps_only_the_newest_price_points() {
    let mut configs = vec![json!({
        "itemId": "abc",
        "runtime": {
            "history": [
                { "t": 1, "p": 1.0 },
                { "t": 2, "p": 2.0 },
                { "t": 3, "p": 3.0 }
            ]
        }
    })];

    trim_runtime_history(&mut configs, 2);

    let history = configs[0]["runtime"]["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["t"], 2);
    assert_eq!(history[1]["t"], 3);
}

#[test]
fn compact_snapshot_leaves_short_histories_untouched() {
    let mut configs = vec![json!({ "itemId": "abc", "runtime": { "history": [{ "t": 1 }] } })];

    trim_runtime_history(&mut configs, 2);

    assert_eq!(
        configs[0]["runtime"]["history"].as_array().unwrap().len(),
        1
    );
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
