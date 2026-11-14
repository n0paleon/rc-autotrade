//! Rollercoin market feed.
//!
//! A background task owns the socket: it reconnects with jittered exponential
//! backoff, pings every 25 s, parses every text frame and pushes
//! [`GatewayEvent`]s to the core loop. It never blocks the decision path.

mod run;
mod transport;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::clock::now_ms;
use crate::events::is_marketplace_cmd;
use crate::latency::LatencyTracker;
use crate::lock::lock;
use crate::logger::Logger;

pub use crate::constants::USER_AGENT;

pub const MAX_BACKOFF_MS: u64 = 30_000;
pub const HEARTBEAT_MS: u64 = 25_000;
pub const HANDSHAKE_TIMEOUT_MS: u64 = 15_000;

/// Supplies the current `(access_token, cookie)` pair. The feed is reconnected
/// by the wiring after a token change.
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
    /// Marketplace commands only (listing/price updates and fulfillment
    /// results). `event_count` stays the total wire traffic so a feed that is
    /// only sending prediction-market frames still looks alive.
    pub market_event_count: i64,
    pub parse_errors: i64,
    /// Frames the core loop could not take because its queue was full. Market
    /// frames are superseded by the next tick, so dropping one is safe; a
    /// non-zero counter still means the consumer is falling behind.
    pub dropped: i64,
    pub cmd_counts: BTreeMap<String, i64>,
    pub last_ping_ms: Option<f64>,
}

impl GatewayStats {
    fn note_message(&mut self, cmd: &str) {
        self.event_count += 1;
        if is_marketplace_cmd(cmd) {
            self.market_event_count += 1;
        }
        *self.cmd_counts.entry(cmd.to_string()).or_insert(0) += 1;
    }
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
        let stats = lock(&self.stats);
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
            "marketEventCount": stats.market_event_count,
            "parseErrors": stats.parse_errors,
            "dropped": stats.dropped,
            "commands": commands,
            "upstream": self.upstream,
            "uptimeMs": stats.connected_at.map(|at| now_ms() - at).unwrap_or(0),
            "lastPingMs": stats.last_ping_ms,
        })
    }

    pub fn latency_snapshot(&self) -> Value {
        lock(&self.latency).snapshot()
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
        lock(&self.stats).status.clone()
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

    tokio::spawn(run::run(config, event_tx, command_rx, stats));
    (handle, event_rx)
}

fn status_payload(stats: &Arc<Mutex<GatewayStats>>, upstream: &str) -> Value {
    let stats = lock(stats);
    json!({
        "status": stats.status,
        "connectedAt": stats.connected_at,
        "lastMessageAt": stats.last_message_at,
        "lastError": stats.last_error,
        "attempts": stats.attempts,
        "reconnects": stats.reconnects,
        "eventCount": stats.event_count,
        "marketEventCount": stats.market_event_count,
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
        let mut stats = lock(stats);
        stats.status = status.to_string();
    }
    let _ = events.try_send(GatewayEvent::Status(status_payload(stats, upstream)));
}

fn record_error(stats: &Arc<Mutex<GatewayStats>>, message: &str) {
    let mut stats = lock(stats);
    stats.last_error = Some(message.chars().take(240).collect());
}
