use serde_json::{Value, json};

use crate::constants::{LOG_BUFFER_SIZE, ORDER_STATUS_FILLED};
use crate::engine::{Engine, EngineEvent, OrderIntent, PurchaseOutcome};
use crate::logger::Logger;
use crate::model::Target;
use crate::store::Store;
use crate::trend::Point;
use crate::validation;

use super::reconcile::RECONCILE_WINDOW_MS;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

mod fills;
mod market;
mod orders;
mod reconcile;
mod results;
mod series;

pub(super) struct Harness {
    pub(super) engine: Engine,
    pub(super) store: Store,
    pub(super) now: Arc<AtomicI64>,
    pub(super) dir: std::path::PathBuf,
    pub(super) logger: Arc<Logger>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(super) fn harness() -> Harness {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rc-rust-engine-{nanos}"));
    // `info` so `log_scan` lines land in the ring buffer the test reads.
    let logger = Arc::new(Logger::new("info"));
    let mut store = Store::open(dir.clone(), logger.clone()).expect("store");
    let target = Target {
        item_id: "6319f7baa8ce530569ed16b9".into(),
        label: "Hashboard".into(),
        item_type: "miner".into(),
        rarity_color: None,
        target_price: 0.1,
        desired_qty: 10,
        high_stock_threshold: 20,
        min_acceptable_qty: 1,
        allow_partial_order: true,
        active: true,
        created_at: None,
        updated_at: None,
    };
    store.upsert_config(target, None);
    let patch = validation::SettingsPatch {
        delay_per_order: Some(0),
        max_orders_per_minute: Some(60),
        ..Default::default()
    };
    store.patch_settings(patch).unwrap();

    let now = Arc::new(AtomicI64::new(1_000_000));
    let clock_now = now.clone();
    let engine = Engine::new(
        Arc::new(move || clock_now.load(Ordering::SeqCst)),
        logger.clone(),
    );
    Harness {
        engine,
        store,
        now,
        dir,
        logger,
    }
}

/// The most recent scan lines, joined, so a test can assert on the reason.
pub(super) fn scan_summary(harness: &Harness) -> String {
    harness
        .logger
        .recent(LOG_BUFFER_SIZE)
        .iter()
        .filter(|entry| entry["scope"] == "scan")
        .filter_map(|entry| entry["message"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn market_frame(price_raw: i64, quantity: i64) -> Value {
    json!({
        "cmd": "marketplace_orders_update",
        "value": {
            "item_id": "6319F7BAA8CE530569ED16B9",
            "currency": "RLT",
            "item_type": "miner",
            "data": { "list": [{ "price": price_raw, "quantity": quantity }] }
        }
    })
}

/// The same frame with the whole-book `totalCount` upstream also sends.
pub(super) fn market_frame_with_total(price_raw: i64, quantity: i64, total: i64) -> Value {
    json!({
        "cmd": "marketplace_orders_update",
        "value": {
            "item_id": "6319F7BAA8CE530569ED16B9",
            "currency": "RLT",
            "item_type": "miner",
            "data": {
                "list": [{ "price": price_raw, "quantity": quantity }],
                "totalCount": total
            }
        }
    })
}

/// Accepts an intent the way the wiring does and returns the settlement.
pub(super) fn accept(
    h: &mut Harness,
    intent: &OrderIntent,
    order_id: &str,
    at: i64,
) -> Vec<EngineEvent> {
    h.now.store(at, Ordering::SeqCst);
    h.engine.on_purchase_result(
        &mut h.store,
        intent,
        PurchaseOutcome {
            ok: true,
            order_id: Some(order_id.to_string()),
            accepted_at: Some(at),
            ..Default::default()
        },
    )
}

pub(super) fn fill_frame(order_id: &str, count: i64, price_raw: i64, status: i64) -> Value {
    json!({
        "cmd": "marketplace_orders_filled",
        "value": {
            "item_id": "6319f7baa8ce530569ed16b9",
            "order_id": order_id,
            "count": count,
            "price": price_raw,
            "status": status
        }
    })
}

pub(super) fn submissions(events: &[EngineEvent]) -> Vec<&OrderIntent> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::Submit(intent) => Some(intent.as_ref()),
            _ => None,
        })
        .collect()
}

/// Market ticks, serialised the way the control plane ships them.
pub(super) fn market_payloads(events: &[EngineEvent]) -> Vec<Value> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::Market(tick) => serde_json::to_value(tick.as_ref()).ok(),
            _ => None,
        })
        .collect()
}

/// Drives one order of `qty` through acceptance and a partial fill of
/// `got`, returning the follow-up intents the engine fired on its own.
pub(super) fn partial_fill(
    h: &mut Harness,
    qty: i64,
    got: i64,
    order_id: &str,
    base: i64,
) -> Vec<OrderIntent> {
    h.now.store(base, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, qty), base, true);
    let intent = submissions(&events)[0].clone();
    assert_eq!(intent.qty, qty);

    accept(h, &intent, order_id, base + 120);
    h.now.store(base + 400, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame(order_id, got, 90_000 * got, ORDER_STATUS_FILLED),
        base + 400,
        true,
    );
    submissions(&fill).into_iter().cloned().collect()
}
