//! Rollercoin market feed, ported from `src/worker/gateway.js`.
//!
//! A background task owns the socket: it reconnects with jittered exponential
//! backoff, pings every 25 s, parses every text frame and pushes
//! [`GatewayEvent`]s to the core loop. It never blocks the decision path.
//!
//! TLS uses rustls (same stack as the `reqwest` HTTP client in `api.rs`),

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::latency::{Channel, LatencyTracker};
use crate::logger::Logger;
use crate::redact::redact_secrets;

pub const MAX_BACKOFF_MS: u64 = 30_000;
pub const HEARTBEAT_MS: u64 = 25_000;
pub const HANDSHAKE_TIMEOUT_MS: u64 = 15_000;
pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Supplies the current `(access_token, cookie)` pair. The feed is reconnected
/// by the wiring after a token change, exactly like the JS gateway.
pub type CredentialsFn = Arc<dyn Fn() -> (String, String) + Send + Sync>;

#[derive(Debug, Clone, Default)]
pub struct GatewayStats {
    pub status: String,
    pub connected_at: Option<i64>,
    pub last_message_at: Option<i64>,
    pub last_error: Option<String>,
    pub attempts: i64,
    pub reconnects: i64,
    pub event_count: i64,
    pub parse_errors: i64,
    /// Frames the core loop could not take because its queue was full. Market
    /// frames are superseded by the next tick, so dropping one is safe; a
    /// non-zero counter still means the consumer is falling behind.
    pub dropped: i64,
    pub cmd_counts: BTreeMap<String, i64>,
    pub last_ping_ms: Option<f64>,
}

#[derive(Debug, Clone)]
pub enum GatewayCommand {
    Reconnect(String),
    Stop,
}

#[derive(Debug, Clone)]
pub enum GatewayEvent {
    Status(Value),
    Frame { raw: Value, at: i64 },
}

#[derive(Clone)]
pub struct GatewayHandle {
    pub commands: mpsc::Sender<GatewayCommand>,
    pub stats: Arc<Mutex<GatewayStats>>,
    pub latency: Arc<Mutex<LatencyTracker>>,
    upstream: String,
}

impl GatewayHandle {
    pub fn stats_json(&self) -> Value {
        let stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        let commands: Vec<Value> = {
            let mut entries: Vec<(&String, &i64)> = stats.cmd_counts.iter().collect();
            entries.sort_by(|a, b| b.1.cmp(a.1));
            entries
                .into_iter()
                .take(15)
                .map(|(cmd, count)| json!({ "cmd": cmd, "count": count }))
                .collect()
        };
        json!({
            "status": stats.status,
            "connectedAt": stats.connected_at,
            "lastMessageAt": stats.last_message_at,
            "lastError": stats.last_error,
            "attempts": stats.attempts,
            "reconnects": stats.reconnects,
            "eventCount": stats.event_count,
            "parseErrors": stats.parse_errors,
            "dropped": stats.dropped,
            "commands": commands,
            "upstream": self.upstream,
            "uptimeMs": stats.connected_at.map(|at| now_ms() - at).unwrap_or(0),
            "lastPingMs": stats.last_ping_ms,
        })
    }

    pub fn latency_snapshot(&self) -> Value {
        self.latency
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }

    pub async fn reconnect(&self, reason: &str) {
        let _ = self
            .commands
            .send(GatewayCommand::Reconnect(reason.to_string()))
            .await;
    }

    pub async fn stop(&self) {
        let _ = self.commands.send(GatewayCommand::Stop).await;
    }

    pub fn status(&self) -> String {
        self.stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone()
    }
}

pub struct GatewayConfig {
    pub url: String,
    pub credentials: CredentialsFn,
    pub logger: Arc<Logger>,
    pub latency: Arc<Mutex<LatencyTracker>>,
}

impl GatewayConfig {
    pub fn upstream_host(&self) -> String {
        self.url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(&self.url)
            .to_string()
    }
}

/// Starts the feed task. The returned handle observes it; events arrive on the
/// receiver and should be drained by the core loop.
pub fn spawn(config: GatewayConfig) -> (GatewayHandle, mpsc::Receiver<GatewayEvent>) {
    let (event_tx, event_rx) = mpsc::channel(256);
    let (command_tx, command_rx) = mpsc::channel(8);
    let stats = Arc::new(Mutex::new(GatewayStats {
        status: "idle".into(),
        ..Default::default()
    }));
    let latency = config.latency.clone();
    let handle = GatewayHandle {
        commands: command_tx,
        stats: stats.clone(),
        latency,
        upstream: config.upstream_host(),
    };

    tokio::spawn(run(config, event_tx, command_rx, stats));
    (handle, event_rx)
}

fn status_payload(stats: &Arc<Mutex<GatewayStats>>, upstream: &str) -> Value {
    let stats = stats.lock().unwrap_or_else(|e| e.into_inner());
    json!({
        "status": stats.status,
        "connectedAt": stats.connected_at,
        "lastMessageAt": stats.last_message_at,
        "lastError": stats.last_error,
        "attempts": stats.attempts,
        "reconnects": stats.reconnects,
        "eventCount": stats.event_count,
        "parseErrors": stats.parse_errors,
        "dropped": stats.dropped,
        "upstream": upstream,
        "uptimeMs": stats.connected_at.map(|at| now_ms() - at).unwrap_or(0),
        "lastPingMs": stats.last_ping_ms,
        "commands": [],
    })
}

fn set_status(
    stats: &Arc<Mutex<GatewayStats>>,
    events: &mpsc::Sender<GatewayEvent>,
    upstream: &str,
    status: &str,
) {
    {
        let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
        stats.status = status.to_string();
    }
    let _ = events.try_send(GatewayEvent::Status(status_payload(stats, upstream)));
}

fn build_request(
    url: &str,
    token: &str,
    cookie: &str,
) -> Option<tokio_tungstenite::tungstenite::handshake::client::Request> {
    let mut parsed = url::Url::parse(url).ok()?;
    if !token.is_empty() {
        parsed.query_pairs_mut().append_pair("token", token);
    }
    let mut request = parsed.to_string().into_client_request().ok()?;
    let headers = request.headers_mut();
    headers.insert("origin", "https://rollercoin.com".parse().ok()?);
    headers.insert("referer", "https://rollercoin.com/".parse().ok()?);
    headers.insert("user-agent", USER_AGENT.parse().ok()?);
    if !cookie.is_empty() {
        headers.insert("cookie", cookie.parse().ok()?);
    }
    Some(request)
}

fn connector() -> Option<Connector> {
    ensure_crypto_provider();
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    let config = CONFIG
        .get_or_init(|| {
            let rustls_native_certs::CertificateResult { certs, .. } =
                rustls_native_certs::load_native_certs();
            let mut root_store = rustls::RootCertStore::empty();
            root_store.add_parsable_certificates(certs);
            let mut config = rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();
            // WebSocket upgrade is HTTP/1.1; mirror the ALPN the previous
            // native-tls connector requested.
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Arc::new(config)
        })
        .clone();
    Some(Connector::Rustls(config))
}

fn ensure_crypto_provider() {
    static PROVIDER: std::sync::Once = std::sync::Once::new();
    PROVIDER.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

async fn run(
    config: GatewayConfig,
    events: mpsc::Sender<GatewayEvent>,
    mut commands: mpsc::Receiver<GatewayCommand>,
    stats: Arc<Mutex<GatewayStats>>,
) {
    let upstream = config.upstream_host();
    let mut attempts: i64 = 0;
    let mut stopped = false;

    loop {
        if stopped {
            break;
        }
        let (token, cookie) = (config.credentials)();
        if token.is_empty() {
            set_status(&stats, &events, &upstream, "auth-missing");
            config.logger.warn(
                "gateway",
                "waiting for a Rollercoin session token (open the Link tab)",
            );
            // Wait for a command or a periodic retry.
            match tokio::time::timeout(Duration::from_secs(15), commands.recv()).await {
                Ok(Some(GatewayCommand::Stop)) | Ok(None) => break,
                Ok(Some(GatewayCommand::Reconnect(_))) => continue,
                Err(_) => continue,
            }
        }

        {
            let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.attempts += 1;
        }
        set_status(&stats, &events, &upstream, "connecting");
        config.logger.info("gateway", "connecting to market feed");
        let connect_started = Instant::now();

        let Some(request) = build_request(&config.url, &token, &cookie) else {
            config
                .logger
                .error("gateway", "could not build the WebSocket request");
            set_status(&stats, &events, &upstream, "offline");
            break;
        };

        let connected = tokio::time::timeout(
            Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector()),
        )
        .await;

        let socket = match connected {
            Ok(Ok((socket, _response))) => {
                let handshake_ms = connect_started.elapsed().as_secs_f64() * 1000.0;
                {
                    let mut latency = config.latency.lock().unwrap_or_else(|e| e.into_inner());
                    latency.record(Channel::WsHandshake, handshake_ms, true, None);
                }
                {
                    let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
                    stats.attempts = 0;
                    stats.reconnects += 1;
                    stats.connected_at = Some(now_ms());
                    stats.last_error = None;
                }
                set_status(&stats, &events, &upstream, "online");
                config.logger.info("gateway", "market feed connected");
                Some(socket)
            }
            Ok(Err(error)) => {
                let message = redact_secrets(&error.to_string());
                {
                    let mut latency = config.latency.lock().unwrap_or_else(|e| e.into_inner());
                    latency.record(
                        Channel::WsHandshake,
                        connect_started.elapsed().as_secs_f64() * 1000.0,
                        false,
                        Some(&message),
                    );
                }
                let auth_error = message.contains("401") || message.contains("403");
                record_error(&stats, &message);
                set_status(
                    &stats,
                    &events,
                    &upstream,
                    if auth_error { "auth-error" } else { "offline" },
                );
                config
                    .logger
                    .warn("gateway", format!("could not connect: {message}"));
                None
            }
            Err(_) => {
                let message =
                    format!("WebSocket handshake timed out after {HANDSHAKE_TIMEOUT_MS}ms");
                {
                    let mut latency = config.latency.lock().unwrap_or_else(|e| e.into_inner());
                    latency.record(
                        Channel::WsHandshake,
                        connect_started.elapsed().as_secs_f64() * 1000.0,
                        false,
                        Some(&message),
                    );
                }
                record_error(&stats, &message);
                set_status(&stats, &events, &upstream, "offline");
                config.logger.warn("gateway", message);
                None
            }
        };

        if let Some(socket) = socket {
            let (mut write, mut read) = socket.split();
            let mut heartbeat = tokio::time::interval_at(
                Instant::now() + Duration::from_millis(HEARTBEAT_MS),
                Duration::from_millis(HEARTBEAT_MS),
            );
            let mut ping_sent_at: Option<Instant> = None;
            let mut reconnect_reason: Option<String> = None;

            loop {
                tokio::select! {
                    incoming = read.next() => {
                        match incoming {
                            Some(Ok(Message::Text(text))) => {
                                {
                                    let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
                                    stats.last_message_at = Some(now_ms());
                                }
                                let parsed = serde_json::from_str::<Value>(&text);
                                match parsed {
                                    Ok(value) if value.is_object() => {
                                        let cmd = value
                                            .get("cmd")
                                            .and_then(Value::as_str)
                                            .unwrap_or("unknown")
                                            .to_string();
                                        // A fill is the only durable record of
                                        // what an order cost, so it is worth
                                        // waiting for queue space. Market ticks
                                        // are superseded by the next frame.
                                        let is_fill = cmd.contains("fill");
                                        {
                                            let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
                                            stats.event_count += 1;
                                            *stats.cmd_counts.entry(cmd).or_insert(0) += 1;
                                        }
                                        let event = GatewayEvent::Frame {
                                            raw: value,
                                            at: now_ms(),
                                        };
                                        if is_fill {
                                            if events.send(event).await.is_err() {
                                                break;
                                            }
                                        } else if events.try_send(event).is_err() {
                                            let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
                                            stats.dropped += 1;
                                        }
                                    }
                                    _ => {
                                        let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
                                        stats.parse_errors += 1;
                                    }
                                }
                            }
                            Some(Ok(Message::Ping(payload))) => {
                                if write.send(Message::Pong(payload)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Ok(Message::Pong(_))) => {
                                if let Some(sent) = ping_sent_at.take() {
                                    let ms = sent.elapsed().as_secs_f64() * 1000.0;
                                    {
                                        let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
                                        stats.last_ping_ms = Some((ms * 10.0).round() / 10.0);
                                    }
                                    let mut latency = config.latency.lock().unwrap_or_else(|e| e.into_inner());
                                    latency.record(Channel::WsPing, ms, true, None);
                                }
                            }
                            Some(Ok(Message::Close(frame))) => {
                                let (code, reason) = frame
                                    .map(|frame| (u16::from(frame.code), frame.reason.to_string()))
                                    .unwrap_or((1006, String::new()));
                                let message = if reason.is_empty() {
                                    format!("socket closed ({code})")
                                } else {
                                    format!("socket closed ({code}: {reason})")
                                };
                                record_error(&stats, &redact_secrets(&message));
                                break;
                            }
                            Some(Ok(_)) => {}
                            Some(Err(error)) => {
                                record_error(&stats, &redact_secrets(&error.to_string()));
                                config
                                    .logger
                                    .warn("gateway", format!("socket error: {}", redact_secrets(&error.to_string())));
                                break;
                            }
                            None => break,
                        }
                    }
                    _ = heartbeat.tick() => {
                        ping_sent_at = Some(Instant::now());
                        if write.send(Message::Ping(Vec::new().into())).await.is_err() {
                            break;
                        }
                    }
                    command = commands.recv() => {
                        match command {
                            Some(GatewayCommand::Stop) | None => {
                                stopped = true;
                                break;
                            }
                            Some(GatewayCommand::Reconnect(reason)) => {
                                reconnect_reason = Some(reason);
                                break;
                            }
                        }
                    }
                }
            }

            let _ = write.close().await;
            {
                let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
                stats.connected_at = None;
                stats.last_ping_ms = None;
            }
            if stopped {
                set_status(&stats, &events, &upstream, "stopped");
                break;
            }
            if let Some(reason) = reconnect_reason {
                config
                    .logger
                    .info("gateway", format!("reconnecting: {reason}"));
            }
            set_status(&stats, &events, &upstream, "offline");
        }

        // Jittered exponential backoff, same shape as the JS worker.
        attempts += 1;
        let exponent = attempts.clamp(0, 5) as u32;
        let base = (1_000_u64 * 2_u64.pow(exponent)).min(MAX_BACKOFF_MS);
        let jitter = (rand::random::<f64>() * 500.0) as u64;
        let delay = base.saturating_add(jitter);
        config
            .logger
            .info("gateway", format!("retrying in {delay}ms"));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
            command = commands.recv() => {
                match command {
                    Some(GatewayCommand::Stop) | None => {
                        break;
                    }
                    Some(GatewayCommand::Reconnect(_)) => {}
                }
            }
        }
    }

    set_status(&stats, &events, &upstream, "stopped");
    config.logger.info("gateway", "market feed stopped");
}

fn record_error(stats: &Arc<Mutex<GatewayStats>>, message: &str) {
    let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
    stats.last_error = Some(message.chars().take(240).collect());
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
    use axum::routing::get;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        Arc::new(Mutex::new(LatencyTracker::new(120, Arc::new(|| now_ms()))))
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
                if let GatewayEvent::Status(status) = event {
                    if status["status"] == "online" {
                        saw_online = true;
                        break;
                    }
                }
            }
        })
        .await
        .expect("online");
        assert!(saw_online);

        handle.reconnect("test").await;
        let reconnected = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(GatewayEvent::Status(status)) = events.recv().await {
                    if status["status"] == "online"
                        && status["reconnects"].as_i64().unwrap_or(0) >= 2
                    {
                        break;
                    }
                }
            }
        })
        .await;
        assert!(reconnected.is_ok(), "did not reconnect");
        handle.stop().await;
    }
}
