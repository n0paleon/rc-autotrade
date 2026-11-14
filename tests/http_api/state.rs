use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use rollercoin_worker::logger::Logger;
use rollercoin_worker::store::{Store, now_ms};

use crate::{ITEM_ID, TOKEN, spawn_upstream, spawn_worker, temp_dir};

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
async fn event_history_requires_the_key_and_clamps_the_window() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let unauthorized = client
        .get(format!("{base}/api/events?days=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let body: Value = client
        .get(format!("{base}/api/events?days=99"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["days"], 7, "the window is capped at a week");
    assert_eq!(body["bucketMs"], 300_000);
    assert_eq!(body["retentionDays"], 8);
    assert!(body["points"].as_array().unwrap().is_empty());
    assert!(body["since"].as_i64().unwrap() < body["until"].as_i64().unwrap());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn order_detail_loads_the_payloads_the_list_omits() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;

    // Seed one row before the worker boots; the harness then opens the same
    // data directory and serves it through the real router.
    {
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger).expect("store");
        let mut entry = rollercoin_worker::store::make_entry(
            "order-1",
            ITEM_ID,
            "Hashboard",
            "filled",
            1,
            now_ms(),
        );
        entry.raw_events = rollercoin_worker::raw_events::sanitize(&json!({
            "priceEvent": { "cmd": "marketplace_orders_update" },
            "resultEvent": { "cmd": "marketplace_orders_filled" }
        }));
        store.add_history(entry).expect("seed row");
    }

    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let unauthorized = client
        .get(format!("{base}/api/history/order-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let body: Value = client
        .get(format!("{base}/api/history/order-1"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(
        body["entry"]["rawEvents"]["priceEvent"]["cmd"],
        "marketplace_orders_update"
    );

    let listed: Value = client
        .get(format!("{base}/api/history?limit=1"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listed["entries"][0]["rawEvents"].is_null(),
        "the list must stay lean"
    );

    let missing = client
        .get(format!("{base}/api/history/unknown"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn price_history_requires_an_item_and_clamps_the_window() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let unauthorized = client
        .get(format!("{base}/api/price-history?itemId={ITEM_ID}&days=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let missing = client
        .get(format!("{base}/api/price-history?days=1"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 400);

    let body: Value = client
        .get(format!("{base}/api/price-history?itemId={ITEM_ID}&days=99"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["itemId"], ITEM_ID);
    assert_eq!(body["days"], 7, "the window is capped at a week");
    assert_eq!(body["bucketMs"], 60_000);
    assert_eq!(body["retentionDays"], 14);
    assert!(body["points"].as_array().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn security_headers_match_the_reference_worker() {
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
    assert!(csp.contains("media-src 'self' data: https://storage.n0pal.net"));
    assert!(csp.contains("frame-ancestors 'none'"));

    let _ = std::fs::remove_dir_all(&dir);
}
