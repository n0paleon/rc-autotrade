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
    ConnectionEvent, GatewayCommand, GatewayConfig, GatewayEvent, GatewayStats,
    HANDSHAKE_TIMEOUT_MS, HEALTHY_UPTIME_MS, HEARTBEAT_MS, MAX_BACKOFF_MS, RECONNECT_JITTER_MS,
    push_connection_event, record_error, set_status,
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
                    let connected_at = now_ms();
                    let first_link = stats.reconnects == 0;
                    stats.attempts = 0;
                    stats.reconnects += 1;
                    stats.connected_at = Some(connected_at);
                    stats.last_error = None;
                    let event = if first_link {
                        ConnectionEvent::connect(connected_at, handshake_ms)
                    } else {
                        ConnectionEvent::reconnect(connected_at, handshake_ms)
                    };
                    push_connection_event(&mut stats.connection_log, event);
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

        // Per-attempt retry inputs: how long the socket actually stayed up and
        // whether the deck asked for this reconnect.
        let mut connected_for: Option<Duration> = None;
        let mut manual_reconnect = false;

        if let Some(socket) = socket {
            let connected_at = Instant::now();
            let (mut write, mut read) = socket.split();
            let mut heartbeat = tokio::time::interval_at(
                Instant::now() + Duration::from_millis(HEARTBEAT_MS),
                Duration::from_millis(HEARTBEAT_MS),
            );
            let mut ping_sent_at: Option<Instant> = None;
            let mut reconnect_reason: Option<String> = None;
            // Every exit path from the read loop below assigns this before it
            // breaks, so the disconnect row always carries a real reason.
            let disconnect_reason: Option<String>;

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
                                                disconnect_reason =
                                                    Some("engine event channel closed".into());
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
                                if let Err(error) = write.send(Message::Pong(payload)).await {
                                    disconnect_reason = Some(format!(
                                        "pong write failed: {}",
                                        redact_secrets(&error.to_string())
                                    ));
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
                                let message = redact_secrets(&message);
                                record_error(&stats, &message);
                                disconnect_reason = Some(message);
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
                                let message = redact_secrets(&error.to_string());
                                record_error(&stats, &message);
                                config
                                    .logger
                                    .warn("gateway", format!("socket error: {message}"));
                                disconnect_reason = Some(format!("socket error: {message}"));
                                break;
                            }
                            None => {
                                disconnect_reason = Some("socket stream ended".into());
                                break;
                            }
                        }
                    }
                    _ = heartbeat.tick() => {
                        ping_sent_at = Some(Instant::now());
                        if let Err(error) = write.send(Message::Ping(Vec::new().into())).await {
                            disconnect_reason = Some(format!(
                                "heartbeat ping failed: {}",
                                redact_secrets(&error.to_string())
                            ));
                            break;
                        }
                    }
                    command = commands.recv() => {
                        match command {
                            Some(GatewayCommand::Stop) => {
                                disconnect_reason = Some("feed stopped".into());
                                stopped = true;
                                break;
                            }
                            None => {
                                disconnect_reason = Some("feed command channel closed".into());
                                stopped = true;
                                break;
                            }
                            Some(GatewayCommand::Reconnect(reason)) => {
                                disconnect_reason =
                                    Some(format!("reconnect requested: {reason}"));
                                reconnect_reason = Some(reason);
                                break;
                            }
                        }
                    }
                }
            }

            let session = connected_at.elapsed();
            connected_for = Some(session);
            let ended_at = now_ms();
            let _ = write.close().await;
            config
                .logger
                .debug("gateway", "socket read loop ended; dropping the connection");
            {
                let mut stats = lock(&stats);
                stats.connected_at = None;
                stats.last_ping_ms = None;
                // The link is gone whether the upstream closed it, a read or
                // write failed, or the deck asked for a reconnect. Count it once
                // per established socket so the monitor can show feed churn.
                stats.disconnects += 1;
                push_connection_event(
                    &mut stats.connection_log,
                    ConnectionEvent::disconnect(
                        ended_at,
                        session.as_millis() as i64,
                        disconnect_reason,
                    ),
                );
            }
            if stopped {
                set_status(&stats, &events, &upstream, "stopped");
                break;
            }
            if let Some(reason) = reconnect_reason {
                manual_reconnect = true;
                config
                    .logger
                    .info("gateway", format!("reconnecting: {reason}"));
            }
            set_status(&stats, &events, &upstream, "offline");
        }

        // A healthy socket that drops is a network blip, and an explicit
        // reconnect is user intent: both retry after the jitter only. A failed
        // handshake, or a socket that keeps dying young, still backs off so an
        // upstream outage cannot be hammered.
        let was_healthy =
            connected_for.is_some_and(|uptime| uptime >= Duration::from_millis(HEALTHY_UPTIME_MS));
        let retry_now = was_healthy || manual_reconnect;
        if retry_now {
            attempts = 0;
        } else {
            attempts += 1;
        }
        let jitter = (rand::random::<f64>() * RECONNECT_JITTER_MS as f64) as u64;
        let (base, delay) = if retry_now {
            (0, jitter)
        } else {
            let exponent = attempts.clamp(0, 5) as u32;
            let base = (1_000_u64 * 2_u64.pow(exponent)).min(MAX_BACKOFF_MS);
            (base, base.saturating_add(jitter))
        };
        if retry_now {
            config.logger.info(
                "gateway",
                format!("retrying immediately (jitter {jitter}ms)"),
            );
        } else {
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
        }
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
