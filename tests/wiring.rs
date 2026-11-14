//! End-to-end wiring test: mock market feed + mock purchase API + real engine,
//! store and session, driven through `runtime::Worker`.
//!
//! Proves the loop the JS worker owns in `index.js`:
//! frame → decision → HTTP purchase → accepted → fill → loot row.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use rollercoin_worker::api::ApiClient;
use rollercoin_worker::config::Config;
use rollercoin_worker::engine::Engine;
use rollercoin_worker::latency::LatencyTracker;
use rollercoin_worker::logger::Logger;
use rollercoin_worker::model::Target;
use rollercoin_worker::runtime::Worker;
use rollercoin_worker::server::AppState;
use rollercoin_worker::store::{Store, now_ms};
use rollercoin_worker::system::SystemMonitor;

const ITEM_ID: &str = "6319f7baa8ce530569ed16b9";
const ORDER_ID: &str = "order-wire-1";

fn temp_dir() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rc-rust-wiring-{nanos}"))
}

fn config(data_dir: std::path::PathBuf, ws_url: String, api_base: String) -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 0,
        worker_token: String::new(),
        data_dir,
        api_base: api_base.clone(),
        ws_url,
        cookie: String::new(),
        keep_alive_ms: 30_000,
        keep_alive_url: format!("{api_base}/api/marketplace/purchase-item"),
        system_sample_ms: 2_000,
        mock: false,
        mock_interval_ms: 1_400,
        log_level: "error".into(),
        cwd: std::env::temp_dir(),
    }
}

async fn ws_feed(mut socket: WebSocket) {
    let market = json!({
        "cmd": "marketplace_orders_update",
        "value": {
            "item_id": ITEM_ID,
            "currency": "RLT",
            "item_type": "miner",
            "data": { "list": [{ "price": 90_000, "quantity": 10 }] }
        }
    });
    let _ = socket.send(Message::Text(market.to_string().into())).await;

    // Give the purchase call time to register, then confirm the fill. If it
    // arrives early, the engine's unmatched buffer must replay it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let fill = json!({
        "cmd": "marketplace_orders_filled",
        "value": {
            "item_id": ITEM_ID,
            "order_id": ORDER_ID,
            "count": 10,
            "price": 900_000,
            "status": 12
        }
    });
    let _ = socket.send(Message::Text(fill.to_string().into())).await;

    while let Some(Ok(message)) = socket.recv().await {
        match message {
            Message::Ping(payload) => {
                let _ = socket.send(Message::Pong(payload)).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

async fn purchase(
    State(calls): State<Arc<AtomicUsize>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(headers["authorization"], "Bearer test-token");
    assert_eq!(body["totalCount"], 10);
    // 10 × 90_000 micro-RLT = 900_000 with the 10% fulfillment headroom the
    // test configures below, so the marketplace sees a 990_000 cap and may
    // complete the whole order instead of filling it partially.
    assert_eq!(body["totalPrice"].as_f64(), Some(990_000.0));
    Json(json!({ "success": true, "data": { "usersPurchaseOrder": ORDER_ID } }))
}

async fn ws_handler(ws: WebSocketUpgrade) -> axum::response::Response {
    ws.on_upgrade(ws_feed)
}

async fn spawn_feed() -> String {
    let app = Router::new().route("/ws", get(ws_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("ws://{address}/ws")
}

async fn spawn_api(calls: Arc<AtomicUsize>) -> String {
    let app = Router::new()
        .route("/api/marketplace/purchase-item", post(purchase))
        .with_state(calls);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

async fn wait_for<F: Fn() -> bool>(label: &str, predicate: F) -> bool {
    for _ in 0..200 {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("timed out waiting for {label}");
    false
}

#[tokio::test]
async fn feed_to_fill_round_trip() {
    let calls = Arc::new(AtomicUsize::new(0));
    let ws_url = spawn_feed().await;
    let api_base = spawn_api(calls.clone()).await;
    let dir = temp_dir();

    let logger = Arc::new(Logger::new("error"));
    let mut store = Store::open(dir.clone(), logger.clone()).expect("store");
    store.upsert_config(
        Target {
            item_id: ITEM_ID.into(),
            label: "Hashboard".into(),
            item_type: "miner".into(),
            rarity_color: None,
            target_price: 0.1,
            desired_qty: 10,
            high_stock_threshold: 20,
            min_acceptable_qty: 1,
            allow_partial_order: true,
            active: true,
            created_at: None,
            updated_at: None,
        },
        None,
    );
    store.flush().expect("flush");
    store
        .patch_settings(rollercoin_worker::validation::SettingsPatch {
            total_price_markup_pct: Some(10.0),
            ..Default::default()
        })
        .expect("markup setting");
    store
        .apply_session_patch(rollercoin_worker::validation::SessionPatch {
            access_token: Some("test-token".into()),
            refresh_token: Some("test-refresh".into()),
            ..Default::default()
        })
        .expect("session");

    let engine = Engine::new(Arc::new(now_ms), logger.clone());
    let latency = Arc::new(Mutex::new(LatencyTracker::new(120, Arc::new(now_ms))));
    let api = Arc::new(ApiClient::with_default_timeout(&api_base).with_latency(latency.clone()));
    let cfg = config(dir.clone(), ws_url, api_base);
    let system = Arc::new(Mutex::new(SystemMonitor::new(2_000, store.db_metrics())));
    let item_info = Arc::new(rollercoin_worker::item_info::ItemInfoService::new(
        api.clone(),
        logger.clone(),
    ));
    let state = Arc::new(AppState::new(
        cfg,
        store,
        engine,
        api,
        item_info,
        latency.clone(),
        system,
        logger,
    ));

    let worker = Worker::start(state.clone());

    let saw_order_sent = {
        let state = state.clone();
        wait_for("order sent", move || {
            state.store.lock().unwrap().state.stats.orders_sent >= 1
        })
        .await
    };
    assert!(saw_order_sent, "engine never emitted a submit");

    let saw_fill = {
        let state = state.clone();
        wait_for("fill recorded", move || {
            let store = state.store.lock().unwrap();
            store.state.stats.filled >= 1 && store.has_order(ORDER_ID)
        })
        .await
    };
    assert!(saw_fill, "fill never reached the loot log");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "purchase called once");

    {
        let store = state.store.lock().unwrap();
        assert_eq!(store.state.stats.filled, 1);
        assert_eq!(store.state.stats.bought, 10);
        assert!((store.state.stats.spent_rlt - 0.9).abs() < 1e-9);
        let entry = store.state.history.first().expect("history row");
        assert_eq!(entry.order_id.as_deref(), Some(ORDER_ID));
        assert_eq!(entry.status, "filled");
        assert!(entry.timings.is_some());
        assert!(entry.raw_events.is_some());

        // The resolver probe runs on every real purchase. This upstream is
        // `127.0.0.1`, so the row must say the lookup was skipped instead of
        // carrying a bogus latency sample.
        let timings = entry.timings.as_ref().expect("timings");
        assert_eq!(timings.dns_host.as_deref(), Some("127.0.0.1"));
        assert_eq!(timings.dns_mode.as_deref(), Some("skipped"));
        assert!(timings.dns_ms.is_none());
        // hyper bypasses the resolver for IP literals, so socket reuse is not
        // observable for this upstream.
        assert_eq!(timings.socket_mode.as_deref(), Some("unknown"));

        // Same row after the SQLite round-trip the deck reads through
        // `/api/history`. Lists skip the payloads; [`order_detail`] is the
        // reader the detail modal uses, and must keep the DNS fields.
        let page = store
            .history_page(rollercoin_worker::store::HistoryQuery {
                limit: Some(1),
                ..Default::default()
            })
            .expect("history page");
        let listed = page.entries.first().expect("stored row");
        assert!(
            listed.timings.is_none() && listed.raw_events.is_none(),
            "list rows must not carry the JSON payloads"
        );
        let stored = store
            .order_detail(&listed.id)
            .expect("detail query")
            .expect("stored row");
        let stored_timings = stored.timings.as_ref().expect("stored timings");
        assert_eq!(stored_timings.dns_mode.as_deref(), Some("skipped"));
        assert_eq!(stored_timings.socket_mode.as_deref(), Some("unknown"));
    }

    // The latency tracker must have seen both the purchase and the handshake.
    let snapshot = latency.lock().unwrap().snapshot();
    assert!(snapshot["channels"]["http"]["count"].as_i64().unwrap() >= 1);
    assert!(
        snapshot["channels"]["wsHandshake"]["count"]
            .as_i64()
            .unwrap()
            >= 1
    );

    // The gateway stats the deck reads must show a live feed.
    let snapshot = {
        let store = state.store.lock().unwrap();
        let engine = state.engine.lock().unwrap();
        state.snapshot(&store, &engine)
    };
    assert_eq!(snapshot["gateway"]["status"], "online");
    assert!(snapshot["gateway"]["eventCount"].as_i64().unwrap() >= 2);
    assert!(snapshot["gateway"]["marketEventCount"].as_i64().unwrap() >= 2);

    // The activity chart reads the same frames the HUD counts, bucketed.
    let counted: i64 = state
        .store
        .lock()
        .unwrap()
        .event_series(now_ms() - 3_600_000)
        .iter()
        .map(|(_, events)| events)
        .sum();
    assert!(
        counted >= 2,
        "every market frame must land in the activity buckets, got {counted}"
    );
    assert!(snapshot["pending"].as_array().unwrap().is_empty());
    assert_eq!(snapshot["keepalive"]["enabled"], true);
    assert_eq!(snapshot["keepalive"]["running"], true);
    assert_eq!(snapshot["system"]["db"]["instrumented"], true);
    assert!(
        snapshot["system"]["db"]["queries"]["statements"]
            .as_i64()
            .unwrap_or(0)
            > 0,
        "SQLite trace should have recorded statements"
    );

    worker.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn control_socket_pushes_live_telemetry() {
    use futures_util::StreamExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    const TOKEN: &str = "test-token-123456";

    let dir = temp_dir();
    let logger = Arc::new(Logger::new("error"));
    let mut store = Store::open(dir.clone(), logger.clone()).expect("store");
    rollercoin_worker::runtime::seed_mock_state(&mut store, &logger).expect("seed");
    store.ensure_worker_token(TOKEN).expect("token");

    let engine = Engine::new(Arc::new(now_ms), logger.clone());
    let latency = Arc::new(Mutex::new(LatencyTracker::new(120, Arc::new(now_ms))));
    let api = Arc::new(ApiClient::with_default_timeout("http://127.0.0.1:1"));
    let item_info = Arc::new(rollercoin_worker::item_info::ItemInfoService::new(
        api.clone(),
        logger.clone(),
    ));
    let system = Arc::new(Mutex::new(SystemMonitor::new(500, store.db_metrics())));
    let mut cfg = config(
        dir.clone(),
        "ws://127.0.0.1:1/ws".into(),
        "http://127.0.0.1:1".into(),
    );
    cfg.mock = true;
    cfg.mock_interval_ms = 200;
    cfg.keep_alive_ms = 0;
    let state = Arc::new(AppState::new(
        cfg, store, engine, api, item_info, latency, system, logger,
    ));

    let app = rollercoin_worker::server::router(state.clone(), dir.join("dist"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let worker = Worker::start(state.clone());

    let mut request = format!("ws://{address}/ws").into_client_request().unwrap();
    request.headers_mut().insert(
        "sec-websocket-protocol",
        axum::http::HeaderValue::from_static("rcw.v1, test-token-123456"),
    );
    let (mut socket, _) = connect_async(request).await.expect("control socket");

    let mut system_seen = false;
    let mut gateway_seen = false;
    let mut latency_seen = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while !(system_seen && gateway_seen && latency_seen) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Some(Ok(message))) = tokio::time::timeout(remaining, socket.next()).await else {
            break;
        };
        let WsMessage::Text(text) = message else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("system") => system_seen = true,
            Some("gateway") => gateway_seen = true,
            Some("latency") => latency_seen = true,
            _ => {}
        }
    }

    assert!(system_seen, "no periodic `system` push reached the socket");
    assert!(
        gateway_seen,
        "no periodic `gateway` push reached the socket"
    );
    assert!(
        latency_seen,
        "no periodic `latency` push reached the socket"
    );

    server.abort();
    worker.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn mock_mode_trades_without_any_upstream() {
    let dir = temp_dir();
    let logger = Arc::new(Logger::new("error"));
    let mut store = Store::open(dir.clone(), logger.clone()).expect("store");
    rollercoin_worker::runtime::seed_mock_state(&mut store, &logger).expect("seed");
    store
        .patch_settings(rollercoin_worker::validation::SettingsPatch {
            delay_per_order: Some(0),
            ..Default::default()
        })
        .expect("settings");

    let engine = Engine::new(Arc::new(now_ms), logger.clone());
    let latency = Arc::new(Mutex::new(LatencyTracker::new(120, Arc::new(now_ms))));
    let api = Arc::new(ApiClient::with_default_timeout("http://127.0.0.1:1"));
    let item_info = Arc::new(rollercoin_worker::item_info::ItemInfoService::new(
        api.clone(),
        logger.clone(),
    ));
    let system = Arc::new(Mutex::new(SystemMonitor::new(2_000, store.db_metrics())));
    let mut cfg = config(
        dir.clone(),
        "ws://127.0.0.1:1/ws".into(),
        "http://127.0.0.1:1".into(),
    );
    cfg.mock = true;
    cfg.mock_interval_ms = 200;
    cfg.keep_alive_ms = 0;
    let state = Arc::new(AppState::new(
        cfg, store, engine, api, item_info, latency, system, logger,
    ));

    let worker = Worker::start(state.clone());

    let filled = {
        let state = state.clone();
        wait_for("mock fill", move || {
            state.store.lock().unwrap().state.stats.filled >= 1
        })
        .await
    };
    assert!(filled, "the simulator never produced a fill");

    {
        let store = state.store.lock().unwrap();
        assert!(store.state.history.iter().any(|entry| {
            entry
                .order_id
                .as_deref()
                .map(|id| id.starts_with("mock_"))
                .unwrap_or(false)
        }));
        assert!(store.state.stats.orders_sent >= 1);
    }

    let snapshot = {
        let store = state.store.lock().unwrap();
        let engine = state.engine.lock().unwrap();
        state.snapshot(&store, &engine)
    };
    assert_eq!(snapshot["gateway"]["status"], "online");
    assert_eq!(snapshot["gateway"]["upstream"], "mock");
    assert_eq!(snapshot["keepalive"]["enabled"], false);
    assert_eq!(snapshot["worker"]["mock"], true);

    worker.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}
