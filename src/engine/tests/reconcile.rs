use super::*;

use crate::constants::ORDER_STATUS_FILLED;

#[test]
fn a_stale_frame_cannot_resurrect_stock_a_fill_already_consumed() {
    let mut h = harness();
    let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001)[0].clone();
    assert_eq!(follow.qty, 1);
    // Accept the follow-up so `inflight` is no longer what blocks the frame.
    accept(&mut h, &follow, "order-2", 1_000_550);

    // A frame generated before our purchase was applied still shows the
    // original three units. Trusting it would buy goods that are gone.
    h.now.store(1_000_600, Ordering::SeqCst);
    let stale = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_600, true);
    assert!(
        submissions(&stale).is_empty(),
        "the stale frame must not re-order stock the fill already consumed"
    );

    let tick = market_payloads(&stale).remove(0);
    assert_eq!(tick["stock"], 3, "the deck keeps the raw upstream depth");
    assert_eq!(
        tick["effectiveStock"], 0,
        "the decision stock already spent both units it saw"
    );
    assert_eq!(tick["reconciling"], true);

    // The raw runtime view is untouched too: it is upstream's number.
    let snapshot = h.engine.runtime_snapshot(&h.store);
    assert_eq!(
        snapshot["6319f7baa8ce530569ed16b9"]["lastStock"], 3,
        "persisted/displayed stock must stay honest"
    );
}

#[test]
fn a_frame_that_shows_at_most_the_floor_closes_the_window_early() {
    let mut h = harness();
    {
        let mut target = h.store.state.configs[0].clone();
        target.min_acceptable_qty = 2;
        h.store.upsert_config(target, None);
    }

    // 3 on the book, 2 bought: the 1-unit remainder is below the minimum,
    // so the follow-up is (correctly) not fired and the floor stays open.
    let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001);
    assert!(follow.is_empty());

    h.now.store(1_000_500, Ordering::SeqCst);
    let caught_up = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 1), 1_000_500, true);
    let snapshot = h.engine.runtime_snapshot(&h.store);
    assert!(
        snapshot["6319f7baa8ce530569ed16b9"]["reconcile"].is_null(),
        "a frame at or below the floor cannot be a pre-fill snapshot"
    );
    let tick = market_payloads(&caught_up).remove(0);
    assert_eq!(tick["stock"], 1);
    assert_eq!(tick["effectiveStock"], 1);
    assert_eq!(tick["reconciling"], false);
}

#[test]
fn an_expired_window_lets_the_frame_win_again() {
    let mut h = harness();
    {
        let mut target = h.store.state.configs[0].clone();
        target.min_acceptable_qty = 2;
        h.store.upsert_config(target, None);
    }
    let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001);
    assert!(follow.is_empty());

    // Beyond the window the next frame is assumed fresh: a genuine restock
    // must not stay invisible forever.
    let at = 1_000_400 + RECONCILE_WINDOW_MS + 1;
    h.now.store(at, Ordering::SeqCst);
    let fresh = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), at, true);
    let tick = market_payloads(&fresh).remove(0);
    assert_eq!(tick["effectiveStock"], 3);
    assert_eq!(tick["reconciling"], false);
    assert_eq!(
        submissions(&fresh).len(),
        1,
        "a fresh frame with enough stock may trade again"
    );
}

#[test]
fn an_optimistic_follow_up_rejection_does_not_back_off_the_target() {
    let mut h = harness();
    let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001)[0].clone();

    h.now.store(1_000_600, Ordering::SeqCst);
    let rejected = h.engine.on_purchase_result(
        &mut h.store,
        &follow,
        PurchaseOutcome {
            ok: false,
            error: Some("no items".into()),
            ..Default::default()
        },
    );
    assert!(rejected.iter().any(|event| matches!(
        event,
        EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "failed"
    )));

    let snapshot = h.engine.runtime_snapshot(&h.store);
    let runtime = &snapshot["6319f7baa8ce530569ed16b9"];
    assert_eq!(
        runtime["blockedUntil"], 0,
        "a wrong guess about the remainder must not freeze the target"
    );
    assert_eq!(runtime["failureStreak"], 0);
    assert_eq!(
        h.store.state.stats.failed, 1,
        "the failed attempt is still recorded"
    );

    // The failure row says why it will not back off.
    let entry = h.store.state.history.first().expect("failure row");
    assert!(
        entry.note.as_deref().unwrap_or("").contains("optimistic"),
        "note: {:?}",
        entry.note
    );

    // Even so, the window is not abandoned for a guess that just failed:
    // until it expires the floor still protects the book.
    h.now.store(1_000_700, Ordering::SeqCst);
    let guarded = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_700, true);
    assert!(submissions(&guarded).is_empty());
}

#[test]
fn a_frame_fuller_than_the_book_we_bought_from_is_trusted() {
    let mut h = harness();
    let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001)[0].clone();
    accept(&mut h, &follow, "order-2", 1_000_550);

    // The seller listed more than the pre-purchase book ever held. A stale
    // snapshot cannot show that, so the frame is new information.
    h.now.store(1_000_600, Ordering::SeqCst);
    let restock = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 6), 1_000_600, true);
    let tick = market_payloads(&restock).remove(0);
    assert_eq!(tick["reconciling"], false);
    assert_eq!(tick["effectiveStock"], 6);
}

#[test]
fn a_moved_best_price_closes_the_window() {
    let mut h = harness();
    // Buy the whole 3-unit level; the floor is therefore 0.
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    accept(&mut h, &intent, "order-1", 1_000_120);
    h.now.store(1_000_400, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 3, 270_000, ORDER_STATUS_FILLED),
        1_000_400,
        true,
    );
    assert!(submissions(&fill).is_empty(), "the whole level was taken");

    // A cheaper listing appears on another level. The clamp is about the
    // level we cleared, so this must be tradable immediately.
    h.now.store(1_000_500, Ordering::SeqCst);
    let next = h
        .engine
        .handle_frame(&mut h.store, &market_frame(80_000, 2), 1_000_500, true);
    let tick = market_payloads(&next).remove(0);
    assert_eq!(tick["reconciling"], false);
    assert_eq!(tick["effectiveStock"], 2);
    let follow = submissions(&next);
    assert_eq!(follow.len(), 1);
    assert_eq!(follow[0].qty, 2);
    assert!(!follow[0].optimistic);
}

#[test]
fn a_stale_frame_at_the_cleared_level_stays_clamped_after_a_full_fill() {
    let mut h = harness();
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
    let intent = submissions(&events)[0].clone();
    accept(&mut h, &intent, "order-1", 1_000_120);
    h.now.store(1_000_400, Ordering::SeqCst);
    h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 3, 270_000, ORDER_STATUS_FILLED),
        1_000_400,
        true,
    );

    // Same level, same depth as before the purchase: indistinguishable from
    // a pre-fill snapshot, so it waits out the window.
    h.now.store(1_000_500, Ordering::SeqCst);
    let stale = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_500, true);
    assert!(submissions(&stale).is_empty());
    let tick = market_payloads(&stale).remove(0);
    assert_eq!(tick["stock"], 3);
    assert_eq!(tick["effectiveStock"], 0);
    assert_eq!(tick["reconciling"], true);
}

#[test]
fn a_frame_whose_total_count_dropped_closes_the_window() {
    let mut h = harness();
    h.now.store(1_000_001, Ordering::SeqCst);
    let events = h.engine.handle_frame(
        &mut h.store,
        &market_frame_with_total(90_000, 5, 20),
        1_000_001,
        true,
    );
    let intent = submissions(&events)[0].clone();
    assert_eq!(intent.total_count_at_trigger, Some(20));
    accept(&mut h, &intent, "order-1", 1_000_120);

    h.now.store(1_000_400, Ordering::SeqCst);
    let fill = h.engine.handle_frame(
        &mut h.store,
        &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
        1_000_400,
        true,
    );
    let follow = submissions(&fill)[0].clone();
    assert_eq!(follow.qty, 3);
    accept(&mut h, &follow, "order-2", 1_000_500);

    // The best level looks deeper than our floor, but the whole book lost
    // exactly the two units we bought: this frame has applied the fill.
    h.now.store(1_000_600, Ordering::SeqCst);
    let caught_up = h.engine.handle_frame(
        &mut h.store,
        &market_frame_with_total(90_000, 4, 18),
        1_000_600,
        true,
    );
    let tick = market_payloads(&caught_up).remove(0);
    assert_eq!(tick["reconciling"], false);
    assert_eq!(tick["effectiveStock"], 4);

    // The old window is gone. The frame bought 4 units of its own, so a new
    // ledger legitimately opens for that order — with `bought` back at zero
    // and the frame's depth as its trigger.
    let follow_now = submissions(&caught_up);
    assert_eq!(follow_now.len(), 1);
    assert_eq!(follow_now[0].qty, 4);
    let snapshot = h.engine.runtime_snapshot(&h.store);
    let reconcile = &snapshot["6319f7baa8ce530569ed16b9"]["reconcile"];
    assert_eq!(reconcile["triggerStock"], 4);
    assert_eq!(reconcile["bought"], 0);
    assert_eq!(reconcile["floor"], 0, "the new order committed all four");
}
