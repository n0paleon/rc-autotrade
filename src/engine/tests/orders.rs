use super::*;

use crate::model::Mode;

#[test]
fn submits_an_order_when_the_price_is_below_target() {
    let mut h = harness();
    let frame = market_frame(90_000, 10);
    let events = h.engine.handle_frame(&mut h.store, &frame, 1_000_001, true);

    let intents = submissions(&events);
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].qty, 10);
    assert!((intents[0].unit_price - 0.09).abs() < 1e-12);
    assert_eq!(h.store.state.stats.orders_sent, 1);
    assert!(
        h.engine
            .runtime_snapshot(&h.store)
            .get("6319f7baa8ce530569ed16b9")
            .is_some()
    );
    // A market tick must be emitted before the submit intent.
    assert_eq!(market_payloads(&events).len(), 1);
}

#[test]
fn applies_the_total_price_markup_to_the_submitted_order() {
    let mut h = harness();
    h.store
        .patch_settings(validation::SettingsPatch {
            total_price_markup_pct: Some(10.0),
            ..Default::default()
        })
        .unwrap();

    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let intents = submissions(&events);
    assert_eq!(intents.len(), 1);
    // 10 × 0.09 RLT = 900_000 micro-RLT, plus 10% headroom.
    assert_eq!(intents[0].total_price_markup_pct, 10.0);
    assert_eq!(intents[0].total_price_raw, 990_000.0);
    // The expected spend (and every profit calculation downstream) stays
    // on the listing price; only the request ceiling is inflated.
    assert!((intents[0].notional - 0.9).abs() < 1e-12);
}

#[test]
fn off_mode_blocks_submission_but_keeps_the_market_tick() {
    let mut h = harness();
    h.store
        .patch_settings(validation::SettingsPatch {
            mode: Some(Mode::Off),
            ..Default::default()
        })
        .unwrap();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    assert!(submissions(&events).is_empty());
    assert_eq!(market_payloads(&events).len(), 1);
    // No budget is spent while off: a later live order is not delayed.
    assert_eq!(h.engine.order_stamps_len(), 0);
}

/// While off the log and the `hot` flag must still describe the real plan,
/// not a blanket "paused". A frame that is genuinely too thin has to say so.
#[test]
fn off_mode_still_reports_the_real_reason() {
    let mut h = harness();
    let mut target = h.store.state.configs[0].clone();
    target.min_acceptable_qty = 5;
    h.store.upsert_config(target, None);
    h.store
        .patch_settings(validation::SettingsPatch {
            mode: Some(Mode::Off),
            ..Default::default()
        })
        .unwrap();

    // Price is inside the ceiling, but 2 units is below the minimum of 5.
    // The honest reason is `stock-below-minimum`, never `paused`.
    let _ = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 2), 1_000_001, true);
    let summary = scan_summary(&h);
    assert!(
        summary.contains("stock-below-minimum"),
        "unexpected scan log: {summary:?}"
    );
    assert!(
        !summary.contains("paused"),
        "off must not fake the reason: {summary:?}"
    );

    // A hot frame still advertises hotness even though nothing is submitted.
    let mut target = h.store.state.configs[0].clone();
    target.min_acceptable_qty = 1;
    h.store.upsert_config(target, None);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_002, true);
    assert!(submissions(&events).is_empty());
    assert!(
        market_payloads(&events)
            .iter()
            .any(|data| data["hot"] == true)
    );
    assert_eq!(h.engine.order_stamps_len(), 0, "off spends no budget");
}

#[test]
fn shadow_mode_broadcasts_the_plan_without_submitting() {
    let mut h = harness();
    h.store
        .patch_settings(validation::SettingsPatch {
            mode: Some(Mode::Shadow),
            ..Default::default()
        })
        .unwrap();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    assert!(submissions(&events).is_empty());
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "dry"
    )));
    // Shadow spends the budget on purpose so it measures live throughput.
    assert_eq!(h.engine.order_stamps_len(), 1);
}

#[test]
fn shadow_mode_needs_no_session_but_live_mode_does() {
    let mut h = harness();
    h.store
        .patch_settings(validation::SettingsPatch {
            mode: Some(Mode::Shadow),
            ..Default::default()
        })
        .unwrap();
    let shadow = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, false);
    assert!(shadow.iter().any(|event| matches!(
        event,
        EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "dry"
    )));

    h.store
        .patch_settings(validation::SettingsPatch {
            mode: Some(Mode::Live),
            ..Default::default()
        })
        .unwrap();
    h.now.store(1_000_500, Ordering::SeqCst);
    let live = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_500, false);
    assert!(
        submissions(&live).is_empty(),
        "live must not submit without a linked session"
    );
}

#[test]
fn live_without_a_session_does_not_spend_the_rate_budget() {
    let mut h = harness();
    h.store
        .patch_settings(validation::SettingsPatch {
            mode: Some(Mode::Live),
            ..Default::default()
        })
        .unwrap();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, false);
    assert!(submissions(&events).is_empty());
    assert_eq!(
        h.engine.order_stamps_len(),
        0,
        "a missing session must not burn the per-minute budget"
    );
}

#[test]
fn cooldown_blocks_a_second_order_inside_the_delay_window() {
    let mut h = harness();
    h.store
        .patch_settings(validation::SettingsPatch {
            delay_per_order: Some(5_000),
            ..Default::default()
        })
        .unwrap();
    let first = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    assert_eq!(submissions(&first).len(), 1);

    h.now.store(1_002_000, Ordering::SeqCst);
    let second = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_002_000, true);
    assert!(submissions(&second).is_empty());
}
