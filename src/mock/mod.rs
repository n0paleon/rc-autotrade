//! Development-only upstream simulator.
//!
//! Enabled with `MOCK=1`. It speaks the same frame shapes as Rollercoin so the
//! engine, API surface and deck can be exercised end to end without a session
//! and without touching the real marketplace. Purchases are simulated locally:
//! 92% fill, 8% fail, after a 500–1400 ms delay.

mod frames;
mod run;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::api::{PurchaseRequest, PurchaseResult};
use crate::clock::now_ms;
use crate::gateway::GatewayEvent;
use crate::latency::LatencyTracker;
use crate::lock::lock;
use crate::logger::Logger;

pub const DEFAULT_INTERVAL_MS: u64 = 1_400;
pub const MIN_INTERVAL_MS: u64 = 200;
pub const MAX_INTERVAL_MS: u64 = 10_000;
pub const CONNECT_DELAY_MS: u64 = 350;

/// Items the simulator mentions but never trades. They exist so the item
/// monitor has something to rank besides the two seeded targets: the whole point
/// of that table is that it covers ids which are *not* on the fleet.
///
/// `(item_id, name, item_type, level)` — the level mirrors the miner rarity
/// ladder the real `item-info` payload carries, so the cached label reads like
/// the production one ("Epic Aurora Miner").
pub const AMBIENT_ITEMS: [(&str, &str, &str, u8); 6] = [
    (
        "65aa0001d3e4f5061728394a",
        "Clover Hashboard",
        "mutation_component",
        0,
    ),
    ("65aa0002d3e4f5061728394b", "Watt Battery", "battery", 0),
    ("65aa0003d3e4f5061728394c", "Server Rack Mk II", "rack", 0),
    ("65aa0004d3e4f5061728394d", "Aurora Miner", "miner", 3),
    (
        "65aa0005d3e4f5061728394e",
        "Copper Coil",
        "mutation_component",
        0,
    ),
    (
        "65aa0006d3e4f5061728394f",
        "Liquid Cooler",
        "utility_item",
        0,
    ),
];

#[derive(Debug, Clone)]
pub struct MockItem {
    pub item_id: String,
    pub label: String,
    pub item_type: String,
    pub target_price: f64,
    pub desired_qty: i64,
    pub high_stock_threshold: i64,
    pub min_acceptable_qty: i64,
    pub active: bool,
}

pub type ItemProvider = Arc<dyn Fn() -> Vec<MockItem> + Send + Sync>;

pub struct MockConfig {
    pub items: ItemProvider,
    pub interval_ms: u64,
    pub logger: Arc<Logger>,
    pub latency: Arc<Mutex<LatencyTracker>>,
}

#[derive(Debug, Clone, Default)]
pub struct MockStats {
    pub status: String,
    pub connected_at: Option<i64>,
    pub last_message_at: Option<i64>,
    pub last_error: Option<String>,
    pub attempts: i64,
    pub reconnects: i64,
    pub event_count: i64,
    /// Marketplace commands only, mirroring the real gateway.
    pub market_event_count: i64,
    pub parse_errors: i64,
    /// Simulated frames the core loop could not take (queue full).
    pub dropped: i64,
    pub cmd_counts: BTreeMap<String, i64>,
    pub last_ping_ms: Option<f64>,
}

enum MockCommand {
    Purchase {
        request: PurchaseRequest,
        reply: oneshot::Sender<PurchaseResult>,
    },
    Reconnect,
    Stop,
}

#[derive(Clone)]
pub struct MockHandle {
    commands: mpsc::Sender<MockCommand>,
    stats: Arc<Mutex<MockStats>>,
}

impl MockHandle {
    pub fn stats_json(&self) -> Value {
        let stats = lock(&self.stats);
        let mut entries: Vec<(&String, &i64)> = stats.cmd_counts.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1));
        let commands: Vec<Value> = entries
            .into_iter()
            .take(15)
            .map(|(cmd, count)| json!({ "cmd": cmd, "count": count }))
            .collect();
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
            "upstream": "mock",
            "uptimeMs": stats.connected_at.map(|at| now_ms() - at).unwrap_or(0),
            "lastPingMs": stats.last_ping_ms,
        })
    }

    pub fn status(&self) -> String {
        lock(&self.stats).status.clone()
    }

    pub async fn reconnect(&self, _reason: &str) {
        let _ = self.commands.send(MockCommand::Reconnect).await;
    }

    pub async fn stop(&self) {
        let _ = self.commands.send(MockCommand::Stop).await;
    }

    /// Simulated purchase: returns an accepted order id immediately, then emits
    /// a fill frame after the configured delay.
    pub async fn purchase(&self, request: PurchaseRequest) -> PurchaseResult {
        let (reply, receiver) = oneshot::channel();
        if self
            .commands
            .send(MockCommand::Purchase { request, reply })
            .await
            .is_err()
        {
            return PurchaseResult {
                ok: false,
                status: 503,
                error: Some("Mock market feed is offline.".into()),
                ..Default::default()
            };
        }
        receiver.await.unwrap_or_else(|_| PurchaseResult {
            ok: false,
            status: 503,
            error: Some("Mock market feed did not answer.".into()),
            ..Default::default()
        })
    }
}

/// Starts the simulator task.
pub fn spawn(config: MockConfig) -> (MockHandle, mpsc::Receiver<GatewayEvent>) {
    let (event_tx, event_rx) = mpsc::channel(256);
    let (command_tx, command_rx) = mpsc::channel(32);
    let stats = Arc::new(Mutex::new(MockStats {
        status: "idle".into(),
        ..Default::default()
    }));
    let handle = MockHandle {
        commands: command_tx,
        stats: stats.clone(),
    };
    tokio::spawn(run::run(config, event_tx, command_rx, stats));
    (handle, event_rx)
}

/// Convenience for tests: a provider with one active item.
pub fn single_item_provider(item: MockItem) -> ItemProvider {
    Arc::new(move || vec![item.clone()])
}
