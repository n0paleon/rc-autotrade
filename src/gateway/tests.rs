use super::*;

use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::routing::get;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::clock::now_ms;

async fn ws_echo(mut socket: WebSocket) {
    let frame = json!({
        "cmd": "marketplace_orders_update",
        "value": { "item_id": "abc123", "currency": "RLT" }
    });
    let _ = socket
        .send(AxumMessage::Text(frame.to_string().into()))
        .await;
    while let Some(Ok(message)) = socket.recv().await {
        match message {
            AxumMessage::Ping(payload) => {
                let _ = socket.send(AxumMessage::Pong(payload)).await;
            }
            AxumMessage::Close(_) => break,
            _ => {}
        }
    }
}

async fn spawn_ws_server(connections: Arc<AtomicUsize>) -> String {
    let app = Router::new().route(
        "/ws",
        get(move |ws: WebSocketUpgrade| {
            let connections = connections.clone();
            async move {
                connections.fetch_add(1, Ordering::SeqCst);
                ws.on_upgrade(ws_echo)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("ws://{address}/ws")
}

fn tracker() -> Arc<Mutex<LatencyTracker>> {
    Arc::new(Mutex::new(LatencyTracker::new(120, Arc::new(now_ms))))
}

#[test]
fn marketplace_counter_ignores_other_channels() {
    let mut stats = GatewayStats::default();
    stats.note_message("marketplace_orders_update");
    stats.note_message("marketplace_orders_filled");
    stats.note_message("prediction_market_live_price");
    stats.note_message("pe_reward_statistic_update");
    stats.note_message("marketplace_activity");
    stats.note_message("unknown");

    assert_eq!(stats.event_count, 6);
    assert_eq!(stats.market_event_count, 3);
    assert_eq!(stats.cmd_counts["marketplace_orders_update"], 1);
}

#[tokio::test]
async fn connects_receives_frames_and_counts_them() {
    let connections = Arc::new(AtomicUsize::new(0));
    let url = spawn_ws_server(connections.clone()).await;
    let latency = tracker();
    let (handle, mut events) = spawn(GatewayConfig {
        url,
        credentials: Arc::new(|| ("token".into(), "cf=1".into())),
        logger: Arc::new(Logger::new("error")),
        latency: latency.clone(),
    });

    let mut saw_frame = false;
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            match event {
                GatewayEvent::Frame { raw, .. } => {
                    assert_eq!(raw["cmd"], "marketplace_orders_update");
                    saw_frame = true;
                    break;
                }
                GatewayEvent::Status(_) => {}
            }
        }
    })
    .await;
    assert!(outcome.is_ok(), "timed out waiting for a frame");
    assert!(saw_frame);
    assert_eq!(connections.load(Ordering::SeqCst), 1);

    let stats = handle.stats_json();
    assert_eq!(stats["status"], "online");
    assert_eq!(stats["eventCount"], 1);
    assert_eq!(stats["marketEventCount"], 1);
    assert_eq!(stats["commands"][0]["cmd"], "marketplace_orders_update");
    assert!(stats["lastMessageAt"].as_i64().is_some());

    let snapshot = latency.lock().unwrap().snapshot();
    assert_eq!(snapshot["channels"]["wsHandshake"]["count"], 1);
    assert_eq!(snapshot["channels"]["wsHandshake"]["lastOk"], true);

    handle.stop().await;
}

#[tokio::test]
async fn reconnects_after_the_socket_closes() {
    let connections = Arc::new(AtomicUsize::new(0));
    let url = spawn_ws_server(connections.clone()).await;
    let (handle, mut events) = spawn(GatewayConfig {
        url,
        credentials: Arc::new(|| ("token".into(), String::new())),
        logger: Arc::new(Logger::new("error")),
        latency: tracker(),
    });

    // Drop the first connection by consuming its server task: the mock server
    // keeps the socket open, so force a reconnect from the client instead.
    let mut saw_online = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            if let GatewayEvent::Status(status) = event
                && status["status"] == "online"
            {
                saw_online = true;
                break;
            }
        }
    })
    .await
    .expect("online");
    assert!(saw_online);

    handle.reconnect("test").await;
    let reconnected = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(GatewayEvent::Status(status)) = events.recv().await
                && status["status"] == "online"
                && status["reconnects"].as_i64().unwrap_or(0) >= 2
            {
                break;
            }
        }
    })
    .await;
    assert!(reconnected.is_ok(), "did not reconnect");
    handle.stop().await;
}
