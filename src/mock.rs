//! Development-only upstream simulator, ported from `src/worker/mock.js`.
//!
//! Enabled with `MOCK=1`. It speaks the same frame shapes as Rollercoin so the
//! engine, API surface and deck can be exercised end to end without a session
//! and without touching the real marketplace. Purchases are simulated locally:
//! 92% fill, 8% fail, after a 500–1400 ms delay.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::api::{PurchaseRequest, PurchaseResult};
use crate::events::is_marketplace_cmd;
use crate::gateway::GatewayEvent;
use crate::latency::{Channel, LatencyTracker};
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

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
        let stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
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
        self.stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone()
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

fn status_payload(stats: &Arc<Mutex<MockStats>>) -> Value {
    let stats = stats.lock().unwrap_or_else(|e| e.into_inner());
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
        "commands": [],
        "upstream": "mock",
        "uptimeMs": stats.connected_at.map(|at| now_ms() - at).unwrap_or(0),
        "lastPingMs": stats.last_ping_ms,
    })
}

fn set_status(stats: &Arc<Mutex<MockStats>>, events: &mpsc::Sender<GatewayEvent>, status: &str) {
    {
        let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
        stats.status = status.to_string();
    }
    let _ = events.try_send(GatewayEvent::Status(status_payload(stats)));
}

fn record_frame(stats: &Arc<Mutex<MockStats>>, cmd: &str) {
    let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
    stats.event_count += 1;
    if is_marketplace_cmd(cmd) {
        stats.market_event_count += 1;
    }
    stats.last_message_at = Some(now_ms());
    *stats.cmd_counts.entry(cmd.to_string()).or_insert(0) += 1;
}

fn emit(
    events: &mpsc::Sender<GatewayEvent>,
    stats: &Arc<Mutex<MockStats>>,
    cmd: &str,
    value: Value,
) {
    record_frame(stats, cmd);
    if events
        .try_send(GatewayEvent::Frame {
            raw: json!({ "cmd": cmd, "value": value }),
            at: now_ms(),
        })
        .is_err()
    {
        let mut guard = stats.lock().unwrap_or_else(|e| e.into_inner());
        guard.dropped += 1;
    }
}

/// Builds one `marketplace_orders_update` frame the way the simulator does.
fn market_frame(item: &MockItem, phase: u64, walk: &mut HashMap<String, f64>) -> Value {
    let factor = match phase {
        3 => 0.87 + rand::random::<f64>() * 0.05,
        4 => 0.93 + rand::random::<f64>() * 0.04,
        _ => 0.99 + rand::random::<f64>() * 0.22,
    };
    let previous = walk.get(&item.item_id).copied().unwrap_or(factor);
    let smoothed = previous * 0.55 + factor * 0.45;
    walk.insert(item.item_id.clone(), smoothed);

    let stock = if phase == 3 || phase == 4 {
        let extra = (rand::random::<f64>() * item.high_stock_threshold.max(1) as f64) as i64;
        item.min_acceptable_qty.max(item.desired_qty + extra)
    } else if rand::random::<f64>() < 0.12 {
        0
    } else {
        let ceiling = (item.desired_qty * 3).max(item.high_stock_threshold + 1);
        (rand::random::<f64>() * ceiling.max(1) as f64) as i64
    };

    let best_price = (item.target_price * smoothed).max(0.000_001);
    let best_raw = (best_price * 1_000_000.0).round() as i64;
    let mut list = Vec::new();
    for (price, quantity) in [
        (best_raw, stock),
        (
            (item.target_price * 1.06 * 1_000_000.0).round() as i64,
            ((stock as f64) * 0.6) as i64,
        ),
        (
            (item.target_price * 1.14 * 1_000_000.0).round() as i64,
            ((stock as f64) * 0.4) as i64,
        ),
    ] {
        if quantity > 0 {
            list.push(json!({ "price": price, "quantity": quantity }));
        }
    }

    json!({
        "item_id": item.item_id,
        "currency": "RLT",
        "item_type": item.item_type,
        "data": {
            "list": list,
            "allowedPriceMin": (best_raw / 2).max(1),
            "allowedPriceMax": best_raw * 6,
        }
    })
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
    tokio::spawn(run(config, event_tx, command_rx, stats));
    (handle, event_rx)
}

async fn run(
    config: MockConfig,
    events: mpsc::Sender<GatewayEvent>,
    mut commands: mpsc::Receiver<MockCommand>,
    stats: Arc<Mutex<MockStats>>,
) {
    let interval = config.interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS);
    set_status(&stats, &events, "connecting");
    config.logger.info("mock", "simulated market feed starting");
    tokio::time::sleep(Duration::from_millis(CONNECT_DELAY_MS)).await;
    {
        let mut latency = config.latency.lock().unwrap_or_else(|e| e.into_inner());
        latency.record(Channel::WsHandshake, CONNECT_DELAY_MS as f64, true, None);
        latency.record(Channel::WsPing, 24.0, true, None);
    }
    {
        let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
        stats.connected_at = Some(now_ms());
        stats.reconnects += 1;
        stats.last_ping_ms = Some(24.0);
    }
    set_status(&stats, &events, "online");
    config.logger.info("mock", "simulated market feed online");

    let mut ticks: u64 = 0;
    let mut seq: u64 = 0;
    let mut walk: HashMap<String, f64> = HashMap::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(interval));
    let mut ping = tokio::time::interval(Duration::from_secs(10));
    ticker.tick().await;
    ping.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                ticks += 1;
                let phase = ticks % 9;
                for item in (config.items)() {
                    if !item.active {
                        continue;
                    }
                    let value = market_frame(&item, phase, &mut walk);
                    emit(&events, &stats, "marketplace_orders_update", value);
                }
                if ticks.is_multiple_of(4) {
                    // A random id keeps the "unknown product" path exercised: it
                    // is counted by the item monitor but never named.
                    emit(&events, &stats, "marketplace_orders_update", json!({
                        "item_id": format!("65aa{:x}", rand::random::<u32>()),
                        "currency": "RLT",
                        "item_type": "mutation_component",
                        "data": { "list": [{ "price": 250_000, "quantity": 4 }] },
                    }));
                }
                if ticks.is_multiple_of(3) {
                    // Ambient marketplace traffic: ids that are deliberately not
                    // on the target fleet, so the item monitor has a real ranking
                    // to draw while the simulator runs.
                    let index = (rand::random::<u32>() as usize) % AMBIENT_ITEMS.len();
                    let (item_id, _, item_type, _) = AMBIENT_ITEMS[index];
                    let price = 120_000 + (rand::random::<u32>() % 400_000);
                    let quantity = 1 + (rand::random::<u32>() % 25);
                    emit(&events, &stats, "marketplace_orders_update", json!({
                        "item_id": item_id,
                        "currency": "RLT",
                        "item_type": item_type,
                        "data": {
                            "list": [{ "price": price, "quantity": quantity }],
                        },
                    }));
                }
                if ticks.is_multiple_of(6) {
                    emit(&events, &stats, "marketplace_activity", json!({
                        "item_id": format!("65aa{:x}", rand::random::<u32>()),
                        "currency": "RLT",
                        "data": { "list": [] },
                    }));
                }
            }
            _ = ping.tick() => {
                let ms = 18.0 + rand::random::<f64>() * 22.0;
                {
                    let mut latency = config.latency.lock().unwrap_or_else(|e| e.into_inner());
                    latency.record(Channel::WsPing, ms, true, None);
                }
                let mut guard = stats.lock().unwrap_or_else(|e| e.into_inner());
                guard.last_ping_ms = Some((ms * 10.0).round() / 10.0);
            }
            command = commands.recv() => {
                match command {
                    Some(MockCommand::Purchase { request, reply }) => {
                        seq += 1;
                        let order_id = format!("mock_{}_{}", now_ms(), seq);
                        let accepted = PurchaseResult {
                            ok: true,
                            status: 200,
                            order_id: Some(order_id.clone()),
                            body: None,
                            error: None,
                            unauthorized: false,
                            socket: crate::dns_probe::SocketMode::Unknown,
                            dns: None,
                            ..Default::default()
                        };
                        let _ = reply.send(accepted);

                        let event_tx = events.clone();
                        let delay_ms = 500 + (rand::random::<f64>() * 900.0) as u64;
                        let total = request.unit_price_raw * request.qty as f64;
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            let failed = rand::random::<f64>() < 0.08;
                            let value = if failed {
                                json!({
                                    "item_id": request.item_id,
                                    "order_id": order_id,
                                    "count": 0,
                                    "price": 0,
                                    "status": 2,
                                })
                            } else {
                                json!({
                                    "item_id": request.item_id,
                                    "order_id": order_id,
                                    "count": request.qty,
                                    "price": total.round() as i64,
                                    "status": 12,
                                })
                            };
                            // Never dropped: this is the fill the engine waits
                            // for to write the loot row.
                            let _ = event_tx
                                .send(GatewayEvent::Frame {
                                    raw: json!({ "cmd": "marketplace_orders_filled", "value": value }),
                                    at: now_ms(),
                                })
                                .await;
                        });
                    }
                    Some(MockCommand::Reconnect) => {
                        set_status(&stats, &events, "connecting");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        {
                            let mut guard = stats.lock().unwrap_or_else(|e| e.into_inner());
                            guard.reconnects += 1;
                            guard.connected_at = Some(now_ms());
                        }
                        set_status(&stats, &events, "online");
                    }
                    Some(MockCommand::Stop) | None => break,
                }
            }
        }
    }

    {
        let mut guard = stats.lock().unwrap_or_else(|e| e.into_inner());
        guard.connected_at = None;
    }
    set_status(&stats, &events, "stopped");
    config.logger.info("mock", "simulated market feed stopped");
}

/// Convenience for tests: a provider with one active item.
pub fn single_item_provider(item: MockItem) -> ItemProvider {
    Arc::new(move || vec![item.clone()])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> MockItem {
        MockItem {
            item_id: "6319f7baa8ce530569ed16b9".into(),
            label: "Hashboard".into(),
            item_type: "miner".into(),
            target_price: 0.1,
            desired_qty: 10,
            high_stock_threshold: 20,
            min_acceptable_qty: 1,
            active: true,
        }
    }

    fn config(interval_ms: u64) -> MockConfig {
        MockConfig {
            items: single_item_provider(item()),
            interval_ms,
            logger: Arc::new(Logger::new("error")),
            latency: Arc::new(Mutex::new(LatencyTracker::new(120, Arc::new(now_ms)))),
        }
    }

    #[tokio::test]
    async fn emits_market_frames_and_goes_online() {
        let (handle, mut events) = spawn(config(200));
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            let mut saw_market = false;
            let mut saw_online = false;
            while let Some(event) = events.recv().await {
                match event {
                    GatewayEvent::Status(status) => {
                        if status["status"] == "online" {
                            saw_online = true;
                        }
                    }
                    GatewayEvent::Frame { raw, .. } => {
                        if raw["cmd"] == "marketplace_orders_update" {
                            saw_market = true;
                            assert_eq!(raw["value"]["item_id"], "6319f7baa8ce530569ed16b9");
                        }
                    }
                }
                if saw_market && saw_online {
                    break;
                }
            }
            (saw_market, saw_online)
        })
        .await
        .expect("timed out");
        assert_eq!(outcome, (true, true));

        let stats = handle.stats_json();
        assert_eq!(stats["status"], "online");
        assert_eq!(stats["upstream"], "mock");
        assert!(stats["eventCount"].as_i64().unwrap() > 0);
        assert!(stats["marketEventCount"].as_i64().unwrap() > 0);
        handle.stop().await;
    }

    #[tokio::test]
    async fn purchase_is_accepted_and_emits_a_fill() {
        let (handle, mut events) = spawn(config(200));
        tokio::time::sleep(Duration::from_millis(500)).await;

        let result = handle
            .purchase(PurchaseRequest {
                item_id: "6319f7baa8ce530569ed16b9".into(),
                item_type: "miner".into(),
                qty: 10,
                unit_price_raw: 90_000.0,
            })
            .await;
        assert!(result.ok);
        assert!(result.order_id.is_some());

        let frame = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                if let GatewayEvent::Frame { raw, .. } = event {
                    if raw["cmd"] == "marketplace_orders_filled" {
                        return raw;
                    }
                }
            }
            Value::Null
        })
        .await
        .expect("timed out waiting for the fill");
        assert_eq!(frame["value"]["order_id"], result.order_id.unwrap());
        handle.stop().await;
    }

    #[tokio::test]
    async fn reconnect_bumps_the_counter() {
        let (handle, _events) = spawn(config(200));
        tokio::time::sleep(Duration::from_millis(500)).await;
        handle.reconnect("test").await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let stats = handle.stats_json();
        assert!(stats["reconnects"].as_i64().unwrap() >= 2);
        handle.stop().await;
    }
}
