use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::*;
use crate::gateway::GatewayEvent;
use crate::latency::LatencyTracker;
use crate::logger::Logger;

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
            total_price_markup_pct: 0.0,
        })
        .await;
    assert!(result.ok);
    assert!(result.order_id.is_some());

    let frame = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if let GatewayEvent::Frame { raw, .. } = event
                && raw["cmd"] == "marketplace_orders_filled"
            {
                return raw;
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
