use super::*;

use crate::constants::{ORDER_STATUS_FAILED, ORDER_STATUS_FILLED};
use serde_json::json;

#[test]
fn accepted_purchase_then_fill_records_history_and_stats() {
    let mut h = harness();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();

    h.now.store(1_000_120, Ordering::SeqCst);
    let accepted = h.engine.on_purchase_result(
        &mut h.store,
        &intent,
        PurchaseOutcome {
            ok: true,
            order_id: Some("order-1".into()),
            accepted_at: Some(1_000_120),
            ..Default::default()
        },
    );
    assert!(accepted.iter().any(|event| matches!(
        event,
        EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "accepted"
    )));
    assert_eq!(h.engine.pending_snapshot().len(), 1);

    h.now.store(1_000_400, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 10, 900_000, ORDER_STATUS_FILLED),
        1_000_400,
        true,
    );
    assert!(fill.iter().any(|event| matches!(
        event,
        EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "filled"
    )));

    assert_eq!(h.store.state.stats.filled, 1);
    assert_eq!(h.store.state.stats.bought, 10);
    assert!((h.store.state.stats.spent_rlt - 0.9).abs() < 1e-9);
    assert!(h.store.has_order("order-1"));
    assert!(h.engine.pending_snapshot().is_empty());
}

#[test]
fn purchase_request_payload_is_kept_on_the_filled_row() {
    let mut h = harness();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    let payload = json!({
        "challenge": "",
        "action": "marketplace",
        "itemId": "6319f7baa8ce530569ed16b9",
        "itemType": "mutation_component",
        "totalCount": 10,
        "currency": "RLT",
        "totalPrice": 900_000.0
    });

    h.now.store(1_000_120, Ordering::SeqCst);
    h.engine.on_purchase_result(
        &mut h.store,
        &intent,
        PurchaseOutcome {
            ok: true,
            order_id: Some("order-payload".into()),
            accepted_at: Some(1_000_120),
            request_payload: Some(payload.clone()),
            ..Default::default()
        },
    );

    h.now.store(1_000_400, Ordering::SeqCst);
    h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-payload", 10, 900_000, ORDER_STATUS_FILLED),
        1_000_400,
        true,
    );

    let entry = h.store.state.history.first().expect("fill row");
    let raw = entry.raw_events.as_ref().expect("raw events");
    assert_eq!(raw["purchaseRequest"], payload);
}

#[test]
fn purchase_request_payload_is_kept_on_the_rejected_row() {
    let mut h = harness();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    let payload = json!({
        "action": "marketplace",
        "itemId": "6319f7baa8ce530569ed16b9",
        "totalCount": 10,
        "currency": "RLT",
        "totalPrice": 900_000.0
    });

    h.now.store(1_000_200, Ordering::SeqCst);
    h.engine.on_purchase_result(
        &mut h.store,
        &intent,
        PurchaseOutcome {
            ok: false,
            error: Some("marketplace rejected the order".into()),
            request_payload: Some(payload.clone()),
            ..Default::default()
        },
    );

    let entry = h.store.state.history.first().expect("failure row");
    let raw = entry.raw_events.as_ref().expect("raw events");
    assert_eq!(raw["purchaseRequest"], payload);
}

#[test]
fn purchase_outcome_carries_the_http_marks_into_the_timeline() {
    let mut h = harness();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    // The engine wrote a provisional sendAt; the HTTP mark must win.
    assert!(intent.timings.send_at.is_some());

    h.now.store(1_000_120, Ordering::SeqCst);
    let accepted = h.engine.on_purchase_result(
        &mut h.store,
        &intent,
        PurchaseOutcome {
            ok: true,
            order_id: Some("order-ttfb".into()),
            send_at: Some(1_000_050),
            response_at: Some(1_000_110),
            complete_at: Some(1_000_111),
            ttfb_ms: Some(60.5),
            accepted_at: Some(1_000_120),
            ..Default::default()
        },
    );

    let timings = accepted
        .iter()
        .find_map(|event| match event {
            EngineEvent::Broadcast {
                kind: "order",
                data,
            } if data["phase"] == "accepted" => Some(data["timings"].clone()),
            _ => None,
        })
        .expect("accepted broadcast carries timings");
    assert_eq!(timings["sendAt"], 1_000_050.0);
    assert_eq!(timings["responseAt"], 1_000_110.0);
    assert_eq!(timings["ttfbMs"], 60.5);
}

#[test]
fn rejected_purchase_records_a_failure_and_backs_off() {
    let mut h = harness();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();

    h.now.store(1_000_200, Ordering::SeqCst);
    let rejected = h.engine.on_purchase_result(
        &mut h.store,
        &intent,
        PurchaseOutcome {
            ok: false,
            error: Some("marketplace rejected the order".into()),
            ..Default::default()
        },
    );
    assert!(rejected.iter().any(|event| matches!(
        event,
        EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "failed"
    )));
    assert_eq!(h.store.state.stats.failed, 1);

    // Backoff blocks the next frame inside the 5 s window.
    h.now.store(1_002_000, Ordering::SeqCst);
    let blocked = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_002_000, true);
    assert!(submissions(&blocked).is_empty());

    // After the window it may try again.
    h.now.store(1_006_500, Ordering::SeqCst);
    let retry = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_006_500, true);
    assert_eq!(submissions(&retry).len(), 1);
}

#[test]
fn no_items_failure_keeps_the_attempted_order_details() {
    let mut h = harness();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();

    h.now.store(1_000_120, Ordering::SeqCst);
    h.engine.on_purchase_result(
        &mut h.store,
        &intent,
        PurchaseOutcome {
            ok: true,
            order_id: Some("order-empty".into()),
            accepted_at: Some(1_000_120),
            ..Default::default()
        },
    );

    h.now.store(1_000_400, Ordering::SeqCst);
    let failed = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-empty", 0, 0, ORDER_STATUS_FAILED),
        1_000_400,
        true,
    );
    assert!(failed.iter().any(|event| matches!(
        event,
        EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "failed"
    )));

    let entry = h.store.state.history.first().expect("failure row");
    assert_eq!(entry.status, "failed");
    // Got = fulfillment count (0 for failed); Sent keeps the attempted qty.
    assert_eq!(entry.qty, 0);
    assert_eq!(entry.requested_qty, Some(intent.qty));
    assert!((entry.unit_price - intent.unit_price).abs() < 1e-12);
    assert!((entry.target_price - intent.target_price).abs() < 1e-12);
    // `latencyMs` spans dispatch → result, not the instant the failure row
    // was written.
    assert_eq!(entry.latency_ms, Some(1_000_400 - intent.at));
}

#[test]
fn duplicate_fill_confirmation_is_ignored() {
    let mut h = harness();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    h.engine.on_purchase_result(
        &mut h.store,
        &intent,
        PurchaseOutcome {
            ok: true,
            order_id: Some("order-9".into()),
            ..Default::default()
        },
    );
    h.now.store(1_000_500, Ordering::SeqCst);
    h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-9", 5, 450_000, ORDER_STATUS_FILLED),
        1_000_500,
        true,
    );
    let before = h.store.state.stats.filled;

    h.now.store(1_000_600, Ordering::SeqCst);
    let duplicate = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-9", 5, 450_000, ORDER_STATUS_FILLED),
        1_000_600,
        true,
    );
    assert!(duplicate.is_empty());
    assert_eq!(h.store.state.stats.filled, before);
}

#[test]
fn an_early_result_is_replayed_once_the_purchase_is_accepted() {
    let mut h = harness();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();

    // The result frame arrives before the HTTP response.
    h.now.store(1_000_050, Ordering::SeqCst);
    let early = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-early", 10, 900_000, ORDER_STATUS_FILLED),
        1_000_050,
        true,
    );
    assert!(
        early.is_empty(),
        "no pending order yet, so it must be buffered"
    );

    h.now.store(1_000_120, Ordering::SeqCst);
    let accepted = h.engine.on_purchase_result(
        &mut h.store,
        &intent,
        PurchaseOutcome {
            ok: true,
            order_id: Some("order-early".into()),
            ..Default::default()
        },
    );
    assert!(accepted.iter().any(|event| matches!(
        event,
        EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "filled"
    )));
    assert!(h.store.has_order("order-early"));
}
