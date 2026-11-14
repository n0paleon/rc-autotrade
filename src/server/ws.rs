//! `/ws` control socket: handshake, auth and the per-socket frame loop.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::AppState;
use super::auth::protocol_auth;
use super::error::error_body;
use super::views::{
    benchmark_snapshot, current_snapshot, gateway_snapshot, latency_snapshot, system_snapshot,
};
use crate::clock::now_ms;
use crate::constants::{APP_NAME, APP_VERSION, TELEMETRY_INTERVAL_SECS};
use crate::lock::lock;

pub(super) async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (offered, token) = protocol_auth(&headers, &params);
    let authorized = offered && {
        let store = lock(&state.store);
        store.verify_worker_token(&token)
    };
    if !offered {
        return error_body(StatusCode::BAD_REQUEST, "rcw.v1 subprotocol is required.");
    }

    ws.protocols(["rcw.v1"])
        .on_upgrade(move |socket| handle_socket(socket, state, authorized))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, authorized: bool) {
    if !authorized {
        let mut socket = socket;
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: 4001,
                reason: "invalid access key".into(),
            })))
            .await;
        state.logger.warn(
            "control",
            "control socket rejected (missing or invalid access key)",
        );
        return;
    }

    let (mut sender, mut receiver) = socket.split();
    let mut updates = state.hub.subscribe();
    let mut auth_rx = state.auth_epoch.subscribe();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.tick().await; // the first tick is immediate
    // Live counters belong to the socket, not to a global clock. All four
    // panels are pushed together on `TELEMETRY_INTERVAL_SECS`: pushing
    // `gateway` on every frame re-rendered the HUD continuously for no benefit.
    // At 3 s the `eventCount` readout moves faster than the upstream frame
    // cadence while the heavy `system` payload still goes out at a sane rate.
    let mut telemetry_tick = tokio::time::interval(Duration::from_secs(TELEMETRY_INTERVAL_SECS));
    // `Burst` (the default) would fire every skipped tick back to back after a
    // pause, flooding the socket with three frames in a row. `Delay` keeps the
    // spacing intact and simply drops the missed slots.
    telemetry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    telemetry_tick.tick().await; // the first tick is immediate
    let mut alive = true;

    let hello = {
        let mut snapshot = current_snapshot(&state);
        snapshot["ok"] = Value::Bool(false);
        let hello = json!({
            "type": "hello",
            "data": {
                "name": APP_NAME,
                "version": APP_VERSION,
                "serverTime": now_ms(),
                "recentLogs": state.logger.recent(80),
            },
            "at": now_ms(),
        });
        (hello, snapshot)
    };

    let (hello, snapshot) = hello;
    if sender
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    let snapshot_frame = json!({ "type": "snapshot", "data": snapshot, "at": now_ms() });
    if sender
        .send(Message::Text(snapshot_frame.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                if let Some(Ok(_)) = &incoming {
                    alive = true;
                }
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if text.len() > 4096 {
                            continue;
                        }
                        let Ok(message) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        match message.get("type").and_then(Value::as_str) {
                            Some("ping") => {
                                let pong = json!({
                                    "type": "pong",
                                    "data": { "t": message.get("t").cloned().unwrap_or(Value::Null), "now": now_ms() },
                                    "at": now_ms(),
                                });
                                if sender.send(Message::Text(pong.to_string().into())).await.is_err() {
                                    break;
                                }
                            }
                            Some("refresh") => {
                                let snapshot = current_snapshot(&state);
                                let frame = json!({ "type": "snapshot", "data": snapshot, "at": now_ms() });
                                if sender.send(Message::Text(frame.to_string().into())).await.is_err() {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            update = updates.recv() => {
                match update {
                    Ok(payload) => {
                        // The envelope was serialised once by `broadcast`; this
                        // is a handle clone, not a re-encoding of the payload.
                        let Ok(text) = Utf8Bytes::try_from(payload) else {
                            continue;
                        };
                        if sender.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // Rebuilding a full snapshot for one slow socket walked
                        // every target's 24 h history. Ask that deck to refresh
                        // over HTTP instead; its state is event-driven anyway.
                        state.logger.debug(
                            "socket",
                            format!("control socket lagged by {skipped} frame(s); requesting a resync"),
                        );
                        let frame = format!(
                            r#"{{"type":"resync","data":{{"reason":"lag","skipped":{skipped}}},"at":{}}}"#,
                            now_ms()
                        );
                        if sender.send(Message::Text(frame.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = telemetry_tick.tick() => {
                if let Some(gateway) = gateway_snapshot(&state) {
                    let frame = json!({ "type": "gateway", "data": gateway, "at": now_ms() });
                    if sender.send(Message::Text(frame.to_string().into())).await.is_err() {
                        break;
                    }
                }
                let system = json!({ "type": "system", "data": system_snapshot(&state), "at": now_ms() });
                if sender.send(Message::Text(system.to_string().into())).await.is_err() {
                    break;
                }
                let latency = json!({ "type": "latency", "data": latency_snapshot(&state), "at": now_ms() });
                if sender.send(Message::Text(latency.to_string().into())).await.is_err() {
                    break;
                }
                let benchmark = json!({ "type": "benchmark", "data": benchmark_snapshot(&state), "at": now_ms() });
                if sender.send(Message::Text(benchmark.to_string().into())).await.is_err() {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                // 30 s liveness probe over every control socket; it also keeps
                // idle connections warm.
                if !alive {
                    break;
                }
                alive = false;
                if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            _ = auth_rx.changed() => {
                // 4003 tells the deck to reconnect immediately (a key rotation
                // invalidates the current socket); 4001 stays for bad keys.
                let _ = sender
                    .send(Message::Close(Some(CloseFrame {
                        code: 4003,
                        reason: "access key rotated".into(),
                    })))
                    .await;
                break;
            }
        }
    }
}
