use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::clock::now_ms;
use crate::events::is_marketplace_cmd;
use crate::gateway::GatewayEvent;
use crate::lock::lock;

use super::{MockItem, MockStats};

pub(super) fn status_payload(stats: &Arc<Mutex<MockStats>>) -> Value {
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
        "commands": [],
        "upstream": "mock",
        "uptimeMs": stats.connected_at.map(|at| now_ms() - at).unwrap_or(0),
        "lastPingMs": stats.last_ping_ms,
    })
}

pub(super) fn set_status(
    stats: &Arc<Mutex<MockStats>>,
    events: &mpsc::Sender<GatewayEvent>,
    status: &str,
) {
    {
        let mut stats = lock(stats);
        stats.status = status.to_string();
    }
    let _ = events.try_send(GatewayEvent::Status(status_payload(stats)));
}

pub(super) fn record_frame(stats: &Arc<Mutex<MockStats>>, cmd: &str) {
    let mut stats = lock(stats);
    stats.event_count += 1;
    if is_marketplace_cmd(cmd) {
        stats.market_event_count += 1;
    }
    stats.last_message_at = Some(now_ms());
    *stats.cmd_counts.entry(cmd.to_string()).or_insert(0) += 1;
}

pub(super) fn emit(
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
        let mut guard = lock(stats);
        guard.dropped += 1;
    }
}

/// Builds one `marketplace_orders_update` frame the way the simulator does.
pub(super) fn market_frame(item: &MockItem, phase: u64, walk: &mut HashMap<String, f64>) -> Value {
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
