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
async fn benchmark_requires_the_key_and_aggregates_seeded_timings() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;

    // Seed one timed fill before the worker boots; the endpoint rebuilds the
    // benchmark from the loot log rather than an in-memory ring.
    {
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger).expect("store");
        let mut entry = rollercoin_worker::store::make_entry(
            "bench-filled",
            ITEM_ID,
            "Hashboard",
            "filled",
            1,
            now_ms(),
        );
        entry.timings = rollercoin_worker::timings::sanitize(&json!({
            "signalAt": 1_000,
            "handleAt": 1_004,
            "orderAt": 1_010,
            "dispatchAt": 1_012,
            "sendAt": 1_013,
            "responseAt": 1_093,
            "completeAt": 1_095,
            "acceptedAt": 1_096,
            "fillAt": 1_200,
            "fillHandledAt": 1_210,
            "ttfbMs": 70.0
        }));
        store.add_history(entry).expect("seed row");
    }

    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let unauthorized = client
        .get(format!("{base}/api/benchmark"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let body: Value = client
        .get(format!("{base}/api/benchmark"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], true);
    let benchmark = &body["benchmark"];
    assert_eq!(benchmark["available"], 1);
    assert_eq!(benchmark["totals"]["filled"], 1);
    assert_eq!(benchmark["windows"]["25"]["stages"]["total"]["count"], 1);
    assert_eq!(benchmark["windows"]["25"]["stages"]["total"]["avg"], 210.0);
    // The monotonic TTFB wins over the wall-clock send→response delta.
    assert_eq!(benchmark["windows"]["25"]["stages"]["ttfb"]["avg"], 70.0);
    // Fill wait starts at that same TTFB boundary (1013 + 70), not at the
    // wall-clock response mark (1093).
    assert_eq!(
        benchmark["windows"]["25"]["stages"]["fillWait"]["avg"],
        117.0
    );
    assert_eq!(
        benchmark["recent"]["count"],
        rollercoin_worker::benchmark::RECENT_ORDERS
    );
    assert_eq!(benchmark["recent"]["orders"], 1);
    assert_eq!(benchmark["recent"]["avg"], 210.0);

    let history: Value = client
        .get(format!(
            "{base}/api/benchmark/history?hours=1&bucket=5m&tzOffsetMs=0&stage=total"
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(history["ok"], true);
    assert_eq!(history["history"]["stage"], "total");
    assert_eq!(history["history"]["summary"]["orders"], 1);
    assert_eq!(history["history"]["summary"]["avg"], 210.0);
    assert_eq!(history["history"]["points"][0]["n"], 1);
    assert_eq!(history["history"]["bucketMs"], 300_000);

    let state: Value = client
        .get(format!("{base}/api/state"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["benchmark"]["available"], 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn benchmark_averages_the_newest_orders_regardless_of_age() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let two_hours_ago = now_ms() - 2 * 3_600_000;
    {
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger).expect("store");
        let mut entry = rollercoin_worker::store::make_entry(
            "bench-old",
            ITEM_ID,
            "Hashboard",
            "filled",
            1,
            two_hours_ago,
        );
        entry.timings = rollercoin_worker::timings::sanitize(&json!({
            "signalAt": 1_000,
            "fillHandledAt": 1_210
        }));
        store.add_history(entry).expect("seed row");
    }

    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();
    let body: Value = client
        .get(format!("{base}/api/benchmark"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let recent = &body["benchmark"]["recent"];
    // Count-based window: order age is irrelevant now, so the 2-hour-old order
    // is still the "recent" one with no fallback flag needed.
    assert!(recent.get("fallback").is_none());
    assert_eq!(recent["until"], two_hours_ago);
    assert_eq!(recent["orders"], 1);
    assert_eq!(recent["avg"], 210.0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn deleting_history_wipes_the_loot_log() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;

    {
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger).expect("store");
        store
            .add_history(rollercoin_worker::store::make_entry(
                "order-reset",
                ITEM_ID,
                "Hashboard",
                "filled",
                1,
                now_ms(),
            ))
            .expect("seed row");
    }

    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let before: Value = client
        .get(format!("{base}/api/history?limit=10"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["total"], 1);

    let reset = client
        .delete(format!("{base}/api/history"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(reset.status(), 200);
    let reset_body: Value = reset.json().await.unwrap();
    assert_eq!(reset_body["ok"], true);
    assert_eq!(reset_body["stats"]["filled"], 0);

    let after: Value = client
        .get(format!("{base}/api/history?limit=10"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["total"], 0, "the loot log must stay wiped");
    assert!(after["entries"].as_array().unwrap().is_empty());

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
async fn control_walks_off_shadow_live_and_never_jumps() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let control = |action: &'static str| {
        let client = client.clone();
        let url = format!("{base}/api/control");
        async move {
            client
                .post(url)
                .bearer_auth(TOKEN)
                .json(&json!({ "action": action }))
                .send()
                .await
                .unwrap()
        }
    };

    // Default is live, so the first step is to stop.
    assert_eq!(control("off").await.status(), 200);

    // The guardrail: off cannot arm live in one hop.
    let refused = control("live").await;
    assert_eq!(refused.status(), 422);
    let body: Value = refused.json().await.unwrap();
    assert!(body["fields"]["mode"].as_str().is_some());

    // The documented path works.
    let shadow = control("shadow").await;
    assert_eq!(shadow.status(), 200);
    let body: Value = shadow.json().await.unwrap();
    assert_eq!(body["settings"]["mode"], "shadow");
    // The legacy mirror keeps an older reader on the safe side.
    assert_eq!(body["settings"]["dryRun"], true);
    assert_eq!(body["settings"]["paused"], false);

    let live = control("live").await;
    assert_eq!(live.status(), 200);
    let body: Value = live.json().await.unwrap();
    assert_eq!(body["settings"]["mode"], "live");
    assert_eq!(body["settings"]["dryRun"], false);
    assert_eq!(body["settings"]["paused"], false);

    // Turning live off is always allowed, in any state.
    assert_eq!(control("off").await.status(), 200);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn settings_patch_accepts_mode_and_the_legacy_aliases() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    // The legacy alias still means something: paused wins and maps to off.
    let body: Value = client
        .patch(format!("{base}/api/settings"))
        .bearer_auth(TOKEN)
        .json(&json!({ "paused": true, "dryRun": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["settings"]["mode"], "off");

    // An explicit mode is authoritative over the aliases.
    let body: Value = client
        .patch(format!("{base}/api/settings"))
        .bearer_auth(TOKEN)
        .json(&json!({ "mode": "shadow", "paused": false, "dryRun": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["settings"]["mode"], "shadow");

    let invalid = client
        .patch(format!("{base}/api/settings"))
        .bearer_auth(TOKEN)
        .json(&json!({ "mode": "armed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 422);

    // Fulfillment headroom: the deck writes it through the same patch route,
    // and the accepted value comes back in the settings object.
    let body: Value = client
        .patch(format!("{base}/api/settings"))
        .bearer_auth(TOKEN)
        .json(&json!({ "totalPriceMarkupPct": 10 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["settings"]["totalPriceMarkupPct"], 10.0);

    let out_of_range = client
        .patch(format!("{base}/api/settings"))
        .bearer_auth(TOKEN)
        .json(&json!({ "totalPriceMarkupPct": 101 }))
        .send()
        .await
        .unwrap();
    assert_eq!(out_of_range.status(), 422);
    let body: Value = out_of_range.json().await.unwrap();
    assert!(body["fields"]["totalPriceMarkupPct"].as_str().is_some());

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
    assert!(csp.contains("media-src 'self' data: https://storage.n0pal.net"));
    assert!(csp.contains("frame-ancestors 'none'"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn item_activity_ranks_ids_outside_the_fleet_and_names_them_from_item_info() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls.clone()).await;

    // Two ids that are NOT on the target fleet: the monitor exists to surface
    // exactly those. The catalogue starts empty, so the names must come from
    // upstream during the request.
    {
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger).expect("store");
        let now = now_ms();
        for _ in 0..7 {
            store.note_item_event("stranger-busy", Some("mutation_component"), now);
        }
        for _ in 0..2 {
            store.note_item_event("stranger-quiet", Some("rack"), now);
        }
        store.flush_item_events().expect("flush");
    }

    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let unauthorized = client
        .get(format!("{base}/api/item-activity?days=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let body: Value = client
        .get(format!("{base}/api/item-activity?days=1&limit=10"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], true);
    let activity = &body["activity"];
    assert_eq!(activity["historyDays"], 30);
    assert_eq!(
        activity["bucketMs"], 300_000,
        "counts are bucketed in 5 minutes"
    );
    assert_eq!(activity["totalEvents"], 9);
    // Pagination metadata describes the whole range, not the page.
    assert_eq!(activity["totalRows"], 2);
    assert_eq!(activity["page"], 1);
    assert_eq!(activity["pages"], 1);
    assert_eq!(activity["hasMore"], false);
    // Per-type summary for the header. The two ids were counted under different
    // feed types, so the total splits accordingly — a later `item-info` lookup
    // must not move a row between categories.
    let type_totals = activity["typeTotals"].as_array().expect("type totals");
    assert_eq!(type_totals.len(), 2);
    let busy_total = type_totals
        .iter()
        .find(|total| total["itemType"] == "mutation_component")
        .expect("parts total");
    assert_eq!(busy_total["events"], 7);
    assert_eq!(busy_total["items"], 1);
    let quiet_total = type_totals
        .iter()
        .find(|total| total["itemType"] == "rack")
        .expect("rack total");
    assert_eq!(quiet_total["events"], 2);
    assert_eq!(quiet_total["items"], 1);
    // The splits add up to the range total.
    let split: i64 = type_totals
        .iter()
        .map(|total| total["events"].as_i64().unwrap())
        .sum();
    assert_eq!(split, activity["totalEvents"].as_i64().unwrap());
    let rows = activity["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["itemId"], "stranger-busy");
    assert_eq!(rows[0]["events"], 7);
    assert_eq!(rows[0]["tracked"], false);
    assert_eq!(rows[0]["itemType"], "mutation_component");
    // The mock upstream answers every item-info call with the same payload, so
    // the row must be named from it rather than left as a raw id.
    assert_eq!(rows[0]["label"], "Rare Hashboard");
    assert_eq!(rows[0]["rarity"], "Rare");
    assert_eq!(activity["unresolved"], 0);
    assert!(
        *upstream_calls.lock().unwrap() > 0,
        "the first response must resolve names upstream"
    );

    // The second call must not hit upstream again: the catalogue remembered the
    // names, and a miss would be remembered too.
    let before = *upstream_calls.lock().unwrap();
    let cached: Value = client
        .get(format!("{base}/api/item-activity?days=1&limit=10"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(*upstream_calls.lock().unwrap(), before, "names are cached");
    assert_eq!(
        cached["activity"]["rows"][0]["label"], "Rare Hashboard",
        "the cached name still renders"
    );

    // Category filters read the same rows. Each id keeps the type its frames
    // were counted under, so the filter and the per-type summary agree even
    // after `item-info` supplies the names.
    let filtered: Value = client
        .get(format!(
            "{base}/api/item-activity?days=1&itemType=mutation_component"
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let typed_rows = filtered["activity"]["rows"].as_array().unwrap();
    assert_eq!(typed_rows.len(), 1);
    assert_eq!(typed_rows[0]["itemId"], "stranger-busy");
    assert_eq!(
        filtered["activity"]["totalEvents"], 7,
        "the totals follow the type filter"
    );
    // The header badges do not: they keep one count per type across the whole
    // range, so the rack badge stays visible while the table shows parts only.
    let filtered_totals = filtered["activity"]["typeTotals"].as_array().unwrap();
    assert_eq!(filtered_totals.len(), 2);
    assert_eq!(
        filtered_totals
            .iter()
            .find(|total| total["itemType"] == "rack")
            .expect("rack total")["events"],
        2
    );

    let rack: Value = client
        .get(format!("{base}/api/item-activity?days=1&itemType=rack"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rack_rows = rack["activity"]["rows"].as_array().unwrap();
    assert_eq!(rack_rows.len(), 1);
    assert_eq!(rack_rows[0]["itemId"], "stranger-quiet");

    let no_match: Value = client
        .get(format!("{base}/api/item-activity?days=1&itemType=battery"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        no_match["activity"]["rows"].as_array().unwrap().is_empty(),
        "a type nobody reported must match nothing"
    );

    let empty: Value = client
        .get(format!("{base}/api/item-activity?days=1&q=does-not-exist"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty["activity"]["rows"].as_array().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn item_activity_paginates_without_changing_the_range_totals() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;

    // Five ids with strictly decreasing volume, so the ranking is unambiguous:
    // p-4 (5 frames) down to p-0 (1 frame).
    {
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger).expect("store");
        let now = now_ms();
        for index in 0..5 {
            let item_type = if index % 2 == 0 { "miner" } else { "rack" };
            for _ in 0..=index {
                store.note_item_event(&format!("p-{index}"), Some(item_type), now);
            }
        }
        store.flush_item_events().expect("flush");
    }

    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let page_one: Value = client
        .get(format!("{base}/api/item-activity?days=1&limit=2&offset=0"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first = &page_one["activity"];
    assert_eq!(first["rows"].as_array().unwrap().len(), 2);
    assert_eq!(first["totalRows"], 5);
    assert_eq!(first["pages"], 3);
    assert_eq!(first["page"], 1);
    assert_eq!(first["hasMore"], true);
    assert_eq!(first["rows"][0]["itemId"], "p-4");
    assert_eq!(first["rows"][1]["itemId"], "p-3");
    // 5+4+3+2+1
    assert_eq!(first["totalEvents"], 15);
    // The share column divides every row by this, so it has to count the whole
    // range rather than the page: 5 of 15 must read 33%, not 100%.
    let split: i64 = first["typeTotals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|total| total["events"].as_i64().unwrap())
        .sum();
    assert_eq!(split, 15, "the type totals add up to the range total");
    assert!(
        first.get("peakEvents").is_none(),
        "the share column no longer needs a separate peak"
    );

    let page_three: Value = client
        .get(format!("{base}/api/item-activity?days=1&limit=2&offset=4"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let last = &page_three["activity"];
    assert_eq!(last["rows"].as_array().unwrap().len(), 1);
    assert_eq!(last["rows"][0]["itemId"], "p-0");
    assert_eq!(last["page"], 3);
    assert_eq!(last["hasMore"], false);
    // Paging never changes the totals: they describe the range.
    assert_eq!(last["totalEvents"], first["totalEvents"]);
    assert_eq!(last["totalRows"], first["totalRows"]);
    assert_eq!(last["typeTotals"], first["typeTotals"]);

    // An offset past the end is an empty page that still reports the real count.
    let past: Value = client
        .get(format!("{base}/api/item-activity?days=1&limit=2&offset=99"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(past["activity"]["rows"].as_array().unwrap().is_empty());
    assert_eq!(past["activity"]["totalRows"], 5);

    let _ = std::fs::remove_dir_all(&dir);
}
