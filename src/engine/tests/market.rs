use super::*;

/// The item monitor must rank every id the RLT market shows, not only the
/// ones on the target fleet: the whole point of the table is the discovery.
#[test]
fn counts_market_frames_for_ids_outside_the_target_fleet() {
    let mut h = harness();
    let stranger = json!({
        "cmd": "marketplace_orders_update",
        "value": {
            "item_id": "65aa0001d3e4f5061728394a",
            "currency": "RLT",
            "item_type": "rack",
            "data": { "list": [{ "price": 250_000, "quantity": 3 }] }
        }
    });
    // No target matches, so the engine returns early — the frame still has
    // to be counted.
    let events = h
        .engine
        .handle_frame(&mut h.store, &stranger, 1_000_100, true);
    assert!(events.is_empty(), "a stranger must not produce a tick");

    // A non-RLT marketplace is a different economy; counting it would rank
    // ids the RLT monitor can never serve.
    let other_currency = json!({
        "cmd": "marketplace_orders_update",
        "value": {
            "item_id": "65aa0002d3e4f5061728394b",
            "currency": "USDT",
            "data": { "list": [{ "price": 250_000, "quantity": 3 }] }
        }
    });
    h.engine
        .handle_frame(&mut h.store, &other_currency, 1_000_100, true);

    // The fleet target is counted too, with its type picked up from the
    // frame so the catalogue can be filtered by product category.
    h.engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_100, true);

    let report = h
        .store
        .item_activity(&crate::store::ItemActivityQuery::default(), 1_000_100);
    let ids: Vec<&str> = report.rows.iter().map(|row| row.item_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["6319f7baa8ce530569ed16b9", "65aa0001d3e4f5061728394a"]
    );
    assert_eq!(report.rows[0].item_type, "miner");
    assert_eq!(report.rows[1].item_type, "rack");
    // The catalogue now lists both, which is what lets the monitor name
    // them later without another frame.
    assert!(h.store.catalog_entry("65aa0001d3e4f5061728394a").is_some());
    assert!(h.store.find_config("65aa0001d3e4f5061728394a").is_none());
}

#[test]
fn market_ticks_are_skipped_while_no_deck_is_connected() {
    let mut h = harness();
    h.engine.set_broadcast_enabled(false);
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    assert!(
        market_payloads(&events).is_empty(),
        "a headless worker must not build market payloads"
    );
    // The order decision itself must be untouched.
    assert_eq!(submissions(&events).len(), 1);

    h.engine.set_broadcast_enabled(true);
    let resumed = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_002, true);
    assert_eq!(market_payloads(&resumed).len(), 1);
}

#[test]
fn orphan_runtimes_are_hidden_and_released_after_the_grace_period() {
    let mut h = harness();
    {
        let runtime = h.engine.runtime_for("deleted-item", 1);
        runtime.seen_at = Some(1_000_000);
        runtime.history.push(Point {
            t: 1_000_000,
            p: 0.5,
        });
    }
    assert!(
        h.engine
            .runtime_snapshot(&h.store)
            .get("deleted-item")
            .is_none(),
        "an orphan must never be serialized into a snapshot"
    );

    // Within the grace period the runtime is kept (delete + re-add shows
    // the same chart), after it the entry is released.
    assert_eq!(
        h.engine.prune_orphan_runtimes(&h.store, 1_000_000, 60_000),
        0
    );
    assert_eq!(
        h.engine.prune_orphan_runtimes(&h.store, 1_200_000, 60_000),
        1
    );
}

#[test]
fn market_broadcast_keeps_its_wire_shape() {
    let mut h = harness();
    let events = h
        .engine
        .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
    let mut market = market_payloads(&events);
    let market = market.remove(0);

    assert_eq!(market["itemId"], "6319f7baa8ce530569ed16b9");
    assert_eq!(market["currency"], "RLT");
    assert_eq!(market["itemType"], "miner");
    assert_eq!(market["stock"], 10);
    assert!((market["close"].as_f64().unwrap() - 0.09).abs() < 1e-12);
    assert_eq!(market["hot"], true);
    assert_eq!(market["mode"], "live");
    assert_eq!(market["bucket"], 960_000);
    // The trend is embedded as JSON, not as a stringified payload. The
    // pre-serialised `RawValue` must not survive `to_value` as a string.
    assert!(
        market["trend"].is_object(),
        "trend must stay an object, got {:?}",
        market["trend"]
    );
    assert!(market["trend"]["status"].is_string());
    assert!(market["fill"].is_object(), "fill: {:?}", market["fill"]);
}
