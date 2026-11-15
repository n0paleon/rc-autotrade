//! The reconnect/read loop that owns the market feed socket.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use super::transport::{build_request, connector};
use super::{
    GatewayCommand, GatewayConfig, GatewayEvent, GatewayStats, HANDSHAKE_TIMEOUT_MS, HEARTBEAT_MS,
    MAX_BACKOFF_MS, record_error, set_status,
};
use crate::clock::now_ms;
use crate::latency::Channel;
use crate::lock::lock;
use crate::redact::redact_secrets;

pub(super) async fn run(
    config: GatewayConfig,
    events: mpsc::Sender<GatewayEvent>,
    mut commands: mpsc::Receiver<GatewayCommand>,
    stats: Arc<Mutex<GatewayStats>>,
) {
    let upstream = config.upstream_host();
    let mut attempts: i64 = 0;
    let mut stopped = false;
    config.logger.debug_lazy("gateway", || {
        format!(
            "feed target {upstream} ({})",
            crate::redact::redact_secrets(&config.url)
        )
    });

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
            let mut stats = lock(&stats);
            stats.attempts += 1;
        }
        set_status(&stats, &events, &upstream, "connecting");
        config.logger.info("gateway", "connecting to market feed");
        config.logger.debug_lazy("gateway", || {
            format!(
                "handshake attempt {} (timeout {HANDSHAKE_TIMEOUT_MS}ms, cookie {})",
                attempts + 1,
                if cookie.is_empty() { "no" } else { "yes" }
            )
        });
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
                    let mut latency = lock(&config.latency);
                    latency.record(Channel::WsHandshake, handshake_ms, true, None);
                }
                {
                    let mut stats = lock(&stats);
                    stats.attempts = 0;
                    stats.reconnects += 1;
                    stats.connected_at = Some(now_ms());
                    stats.last_error = None;
                }
                set_status(&stats, &events, &upstream, "online");
                config.logger.info("gateway", "market feed connected");
                config.logger.debug_lazy("gateway", || {
                    format!("handshake finished in {handshake_ms:.1}ms")
                });
                Some(socket)
            }
            Ok(Err(error)) => {
                let message = redact_secrets(&error.to_string());
                {
                    let mut latency = lock(&config.latency);
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
                    let mut latency = lock(&config.latency);
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
                                    let mut stats = lock(&stats);
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
                                            let mut stats = lock(&stats);
                                            stats.note_message(&cmd);
                                        }
                                        config.logger.debug_lazy("gateway", || {
                                            format!("frame {cmd} ({} bytes)", text.len())
                                        });
                                        let event = GatewayEvent::Frame {
                                            raw: value,
                                            at: now_ms(),
                                        };
                                        if is_fill {
                                            if events.send(event).await.is_err() {
                                                break;
                                            }
                                        } else if events.try_send(event).is_err() {
                                            let mut stats = lock(&stats);
                                            stats.dropped += 1;
                                            config.logger.debug_lazy("gateway", || {
                                                format!("dropped {cmd}: engine queue is full")
                                            });
                                        }
                                    }
                                    Ok(_) => {
                                        let mut stats = lock(&stats);
                                        stats.parse_errors += 1;
                                        config.logger.debug_lazy("gateway", || {
                                            format!(
                                                "ignored non-object frame ({} bytes)",
                                                text.len()
                                            )
                                        });
                                    }
                                    Err(error) => {
                                        let mut stats = lock(&stats);
                                        stats.parse_errors += 1;
                                        config.logger.debug_lazy("gateway", || {
                                            format!(
                                                "unparseable frame ({} bytes): {error}",
                                                text.len()
                                            )
                                        });
                                    }
                                }
                            }
                            Some(Ok(Message::Ping(payload))) => {
                                config.logger.debug_lazy("gateway", || {
                                    format!("upstream ping ({} bytes) -> pong", payload.len())
                                });
                                if write.send(Message::Pong(payload)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Ok(Message::Pong(_))) => {
                                if let Some(sent) = ping_sent_at.take() {
                                    let ms = sent.elapsed().as_secs_f64() * 1000.0;
                                    {
                                        let mut stats = lock(&stats);
                                        stats.last_ping_ms = Some((ms * 10.0).round() / 10.0);
                                    }
                                    let mut latency = lock(&config.latency);
                                    latency.record(Channel::WsPing, ms, true, None);
                                    config.logger.debug_lazy("gateway", || {
                                        format!("pong in {ms:.1}ms")
                                    });
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
                                config.logger.debug("gateway", &message);
                                record_error(&stats, &redact_secrets(&message));
                                break;
                            }
                            Some(Ok(other)) => {
                                // Never format the payload: a stray binary frame
                                // can be megabytes long.
                                let label = match &other {
                                    Message::Binary(bytes) => {
                                        format!("binary frame ({} bytes)", bytes.len())
                                    }
                                    _ => "non-text frame".to_string(),
                                };
                                config.logger.debug_lazy("gateway", || {
                                    format!("ignoring {label}")
                                });
                            }
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
            config
                .logger
                .debug("gateway", "socket read loop ended; dropping the connection");
            {
                let mut stats = lock(&stats);
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

        // Jittered exponential backoff.
        attempts += 1;
        let exponent = attempts.clamp(0, 5) as u32;
        let base = (1_000_u64 * 2_u64.pow(exponent)).min(MAX_BACKOFF_MS);
        let jitter = (rand::random::<f64>() * 500.0) as u64;
        let delay = base.saturating_add(jitter);
        config
            .logger
            .info("gateway", format!("retrying in {delay}ms"));
        config.logger.debug_lazy("gateway", || {
            format!(
                "attempt {} failed; backoff base {base}ms + {}ms jitter",
                attempts,
                delay - base
            )
        });
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
