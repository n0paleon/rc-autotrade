use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;

use crate::api::PurchaseResult;
use crate::clock::now_ms;
use crate::gateway::GatewayEvent;
use crate::latency::Channel;
use crate::lock::lock;

use super::frames::{emit, market_frame, set_status};
use super::{
    AMBIENT_ITEMS, CONNECT_DELAY_MS, MAX_INTERVAL_MS, MIN_INTERVAL_MS, MockCommand, MockConfig,
    MockStats,
};

pub(super) async fn run(
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
        let mut latency = lock(&config.latency);
        latency.record(Channel::WsHandshake, CONNECT_DELAY_MS as f64, true, None);
        latency.record(Channel::WsPing, 24.0, true, None);
    }
    {
        let mut stats = lock(&stats);
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
                    let mut latency = lock(&config.latency);
                    latency.record(Channel::WsPing, ms, true, None);
                }
                let mut guard = lock(&stats);
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
                            let mut guard = lock(&stats);
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
        let mut guard = lock(&stats);
        guard.connected_at = None;
    }
    set_status(&stats, &events, "stopped");
    config.logger.info("mock", "simulated market feed stopped");
}
