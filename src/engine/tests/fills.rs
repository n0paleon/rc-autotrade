use super::*;

use crate::constants::{ORDER_STATUS_FAILED, ORDER_STATUS_FILLED};
use crate::model::Mode;

#[test]
fn a_partial_fill_rearms_the_remainder_without_waiting_for_a_frame() {
    let mut h = harness();
    let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001);

    assert_eq!(
        follow.len(),
        1,
        "the remainder must be fired from the fill, not the next frame"
    );
    assert_eq!(follow[0].qty, 1);
    assert!(follow[0].optimistic);
    assert_eq!(follow[0].stock_at_trigger, Some(1));
    assert_eq!(
        follow[0].timings.signal_cmd.as_deref(),
        Some("marketplace_orders_filled")
    );
    assert_eq!(
        follow[0].timings.signal_at,
        Some(1_000_401.0),
        "the fill frame is the signal, so the timeline must start there"
    );
    assert_eq!(follow[0].item_id, "6319f7baa8ce530569ed16b9");
    assert_eq!(h.store.state.stats.orders_sent, 2);

    // The follow-up itself fills: the chain stops and the loot row says the
    // order was a local guess rather than a frame-driven one.
    accept(&mut h, &follow[0], "order-2", 1_000_450);
    h.now.store(1_000_700, Ordering::SeqCst);
    let done = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-2", 1, 90_000, ORDER_STATUS_FILLED),
        1_000_700,
        true,
    );
    assert!(submissions(&done).is_empty(), "nothing is left to chase");
    let entry = h.store.state.history.first().expect("filled row");
    assert_eq!(entry.status, "filled");
    assert_eq!(entry.note.as_deref(), Some("optimistic follow-up"));
}

#[test]
fn a_full_fill_does_not_rearm() {
    let mut h = harness();
    let follow = partial_fill(&mut h, 3, 3, "order-1", 1_000_001);
    assert!(follow.is_empty(), "nothing is left to chase");
}

/// `sent < ready`: the request never covered the best level, so the units a
/// frame would re-advertise one WS hop later are bought from the result
/// itself. The old rule only fired on `got < sent`, so this level was
/// abandoned until the next frame.
#[test]
fn a_full_fill_with_stock_left_rearms_early() {
    let mut h = harness();
    {
        let mut target = h.store.state.configs[0].clone();
        target.desired_qty = 3;
        h.store.upsert_config(target, None);
    }
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    assert_eq!(
        intent.qty, 3,
        "the request intentionally stops below the book"
    );

    accept(&mut h, &intent, "order-1", 1_000_120);
    h.now.store(1_000_400, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 3, 270_000, ORDER_STATUS_FILLED),
        1_000_400,
        true,
    );

    // `got == sent`, so there is no shortfall at all: the trigger is the
    // stock the level still shows, exactly like a fresh frame would see it.
    let follow = submissions(&fill);
    assert_eq!(follow.len(), 1, "a deep level is not abandoned for a frame");
    assert_eq!(follow[0].qty, 3);
    assert!(follow[0].optimistic);
    assert_eq!(follow[0].stock_at_trigger, Some(7));

    // The deck learns the settled book from the same event, not later.
    let broadcast = fill
        .iter()
        .find_map(|event| match event {
            EngineEvent::Broadcast {
                kind: "order",
                data,
            } if data["phase"] == "filled" => Some(data.clone()),
            _ => None,
        })
        .expect("filled broadcast");
    assert_eq!(broadcast["stockAfter"], 7);
}

#[test]
fn a_submitted_order_protects_its_units_before_the_fill_lands() {
    let mut h = harness();
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    accept(&mut h, &intent, "order-1", 1_000_120);

    // A frame generated before upstream applied the request still shows the
    // pre-purchase book. Ordering from it would commit ten units that the
    // request already took.
    h.now.store(1_000_200, Ordering::SeqCst);
    let stale = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_200, true);
    assert!(submissions(&stale).is_empty());
    let tick = market_payloads(&stale).remove(0);
    assert_eq!(tick["stock"], 10, "the deck keeps the raw upstream depth");
    assert_eq!(tick["effectiveStock"], 0);
    assert_eq!(tick["reconciling"], true);
}

/// The bug this guards: the early update used to rebuild its remaining
/// quantity from the trigger frame, so a frame or a follow-up that already
/// spent the leftover was handed it a second time when the result landed.
#[test]
fn the_early_update_never_spends_the_leftover_twice() {
    let mut h = harness();
    {
        let mut target = h.store.state.configs[0].clone();
        target.desired_qty = 3;
        h.store.upsert_config(target, None);
    }
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    accept(&mut h, &intent, "order-1", 1_000_120);

    // A frame lands before the result and already shows the level down to
    // the one unit the fill will leave. The request has those units
    // committed, so it must not buy them.
    h.now.store(1_000_200, Ordering::SeqCst);
    let caught_up = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 1), 1_000_200, true);
    assert!(submissions(&caught_up).is_empty());

    // The real event lands: two of three filled, one unit is known left.
    h.now.store(1_000_400, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
        1_000_400,
        true,
    );
    let follow = submissions(&fill);
    assert_eq!(follow.len(), 1);
    assert_eq!(follow[0].qty, 1, "only the unit the fill left behind");
    let follow = follow[0].clone();
    accept(&mut h, &follow, "order-2", 1_000_500);

    // A stale frame repeats the same depth. The follow-up owns that unit,
    // so no second order may be fired for it.
    h.now.store(1_000_600, Ordering::SeqCst);
    let stale = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 1), 1_000_600, true);
    assert!(submissions(&stale).is_empty());
    assert_eq!(h.store.state.stats.orders_sent, 2);
}

#[test]
fn a_late_fill_does_not_reopen_the_window() {
    let mut h = harness();
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    accept(&mut h, &intent, "order-1", 1_000_120);

    // The result arrives after the local window expired: the feed has been
    // authoritative for a while, so the old trigger must not be revived.
    let late = 1_000_120 + RECONCILE_WINDOW_MS + 1;
    h.now.store(late, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
        late,
        true,
    );
    assert!(submissions(&fill).is_empty(), "a late fill cannot rearm");
    let snapshot = h.engine.runtime_snapshot(&h.store);
    assert!(snapshot["6319f7baa8ce530569ed16b9"]["reconcile"].is_null());

    // A fresh frame is trusted and can trade the unit that is left.
    h.now.store(late + 100, Ordering::SeqCst);
    let fresh = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 1), late + 100, true);
    let tick = market_payloads(&fresh).remove(0);
    assert_eq!(tick["effectiveStock"], 1);
    assert_eq!(tick["reconciling"], false);
    assert_eq!(submissions(&fresh).len(), 1);
}

#[test]
fn a_failed_fill_gives_its_reservation_back() {
    let mut h = harness();
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    accept(&mut h, &intent, "order-1", 1_000_120);

    h.now.store(1_000_400, Ordering::SeqCst);
    let failed = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 0, 0, ORDER_STATUS_FAILED),
        1_000_400,
        true,
    );
    assert!(submissions(&failed).is_empty());
    let snapshot = h.engine.runtime_snapshot(&h.store);
    assert_eq!(
        snapshot["6319f7baa8ce530569ed16b9"]["reconcile"]["floor"], 10,
        "nothing was obtained, so the whole request returns to the book"
    );

    // The next frame shows the level intact; because the floor is back at
    // the pre-order depth the frame wins and the missed level is retryable.
    h.now.store(1_000_500, Ordering::SeqCst);
    let resumed = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_500, true);
    assert_eq!(market_payloads(&resumed)[0]["reconciling"], false);
    assert_eq!(submissions(&resumed).len(), 1);
}

#[test]
fn an_over_fill_cannot_inflate_the_book() {
    let mut h = harness();
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    accept(&mut h, &intent, "order-1", 1_000_120);

    // Upstream reports more units than were requested. The ledger must clamp
    // instead of going negative or offering a phantom remainder.
    h.now.store(1_000_400, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 5, 450_000, ORDER_STATUS_FILLED),
        1_000_400,
        true,
    );
    assert!(submissions(&fill).is_empty());
    let snapshot = h.engine.runtime_snapshot(&h.store);
    assert_eq!(
        snapshot["6319f7baa8ce530569ed16b9"]["reconcile"]["floor"],
        0
    );
}

#[test]
fn a_follow_up_respects_the_current_target_price() {
    let mut h = harness();
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    accept(&mut h, &intent, "order-1", 1_000_120);

    // The target price is lowered while the order is in flight: the
    // remainder is no longer worth chasing at the old price.
    let mut target = h.store.state.configs[0].clone();
    target.target_price = 0.05;
    h.store.upsert_config(target, None);

    h.now.store(1_000_400, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
        1_000_400,
        true,
    );
    assert!(
        submissions(&fill).is_empty(),
        "a follow-up buys at the price that triggered it, so the new ceiling must stop it"
    );
}

#[test]
fn a_follow_up_never_doubles_a_deep_book() {
    let mut h = harness();
    {
        let mut target = h.store.state.configs[0].clone();
        target.desired_qty = 3;
        h.store.upsert_config(target, None);
    }

    // 10 on the book, 3 bought, 2 filled: the level still holds 8, so the
    // early follow-up repeats the target's per-signal quantity (3) and is
    // never doubled just because the book looks deep.
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    assert_eq!(intent.qty, 3);
    assert!(!intent.doubled);

    accept(&mut h, &intent, "order-1", 1_000_120);
    h.now.store(1_000_400, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
        1_000_400,
        true,
    );
    let follow = submissions(&fill);
    assert_eq!(follow.len(), 1);
    assert_eq!(
        follow[0].qty, 3,
        "a deep level gets the normal per-signal quantity, not the intent remainder"
    );
    assert!(!follow[0].doubled);
    assert_eq!(follow[0].stock_at_trigger, Some(8));
}

#[test]
fn a_remainder_below_the_minimum_is_not_chased() {
    let mut h = harness();
    {
        let mut target = h.store.state.configs[0].clone();
        target.min_acceptable_qty = 2;
        h.store.upsert_config(target, None);
    }
    let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001);
    assert!(
        follow.is_empty(),
        "the configured minimum still applies to a follow-up"
    );
}

#[test]
fn off_mode_stops_the_local_rearm() {
    let mut h = harness();
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    accept(&mut h, &intent, "order-1", 1_000_120);
    h.store
        .patch_settings(validation::SettingsPatch {
            mode: Some(Mode::Off),
            ..Default::default()
        })
        .unwrap();

    h.now.store(1_000_400, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
        1_000_400,
        true,
    );
    assert!(
        submissions(&fill).is_empty(),
        "off must not fire anything, locally or otherwise"
    );
}

#[test]
fn a_follow_up_needs_a_linked_session() {
    let mut h = harness();
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    accept(&mut h, &intent, "order-1", 1_000_120);

    // The session disappears between the order and its fill.
    h.now.store(1_000_400, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
        1_000_400,
        false,
    );
    assert!(
        submissions(&fill).is_empty(),
        "without a session the follow-up would only burn a rate slot"
    );
}

#[test]
fn a_duplicate_fill_cannot_rearm_twice() {
    let mut h = harness();
    let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001);
    assert_eq!(follow.len(), 1);

    h.now.store(1_000_500, Ordering::SeqCst);
    let duplicate = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
        1_000_500,
        true,
    );
    assert!(duplicate.is_empty());
    assert_eq!(
        h.store.state.stats.orders_sent, 2,
        "one fill can only ever fire one follow-up"
    );
}

#[test]
fn a_fill_that_beats_the_http_response_still_rearms() {
    let mut h = harness();
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
    let intent = submissions(&events)[0].clone();

    // The fill frame lands before the purchase response is processed, so it
    // is buffered and replayed inside `on_purchase_result`.
    h.now.store(1_000_050, Ordering::SeqCst);
    let early = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-early", 2, 180_000, ORDER_STATUS_FILLED),
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
    let follow = submissions(&accepted);
    assert_eq!(
        follow.len(),
        1,
        "the replayed fill must still fire the remainder"
    );
    assert_eq!(follow[0].qty, 1);
    assert!(follow[0].optimistic);

    // The follow-up owns the in-flight slot; clearing it after the replay
    // would have let a third order through.
    let snapshot = h.engine.runtime_snapshot(&h.store);
    assert_eq!(snapshot["6319f7baa8ce530569ed16b9"]["inflight"], true);
}
