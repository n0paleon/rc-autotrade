use super::*;

#[test]
fn settings_serialize_in_the_ui_shape() {
    let json = serde_json::to_value(Settings::default()).unwrap();
    assert_eq!(json["delayPerOrder"], 200);
    assert_eq!(json["maxOrdersPerMinute"], 60);
    assert_eq!(json["mode"], "live");
    assert_eq!(json["autoRefresh"], true);
    assert_eq!(json["totalPriceMarkupPct"], 0.0);
    // Legacy mirrors, read-only for the deck but present for old readers.
    assert_eq!(json["dryRun"], false);
    assert_eq!(json["paused"], false);
}

#[test]
fn a_legacy_settings_document_maps_to_one_mode() {
    // A document that says nothing about trading must not be read as
    // "armed for live": the safe reading of silence is shadow.
    let settings: Settings = serde_json::from_value(
        serde_json::json!({ "delayPerOrder": 200, "maxOrdersPerMinute": 60 }),
    )
    .unwrap();
    assert_eq!(settings.mode, Mode::Shadow, "silence is not live");
    assert!(settings.auto_refresh);
    assert_eq!(
        settings.total_price_markup_pct, 0.0,
        "an older document has no price headroom"
    );

    let paused: Settings = serde_json::from_value(serde_json::json!({
        "delayPerOrder": 200, "maxOrdersPerMinute": 60, "paused": true, "dryRun": true
    }))
    .unwrap();
    assert_eq!(paused.mode, Mode::Off, "the dead cell collapses to off");

    let dry: Settings = serde_json::from_value(serde_json::json!({
        "delayPerOrder": 200, "maxOrdersPerMinute": 60, "dryRun": true, "paused": false
    }))
    .unwrap();
    assert_eq!(dry.mode, Mode::Shadow);

    let live: Settings = serde_json::from_value(serde_json::json!({
        "delayPerOrder": 200, "maxOrdersPerMinute": 60, "dryRun": false, "paused": false
    }))
    .unwrap();
    assert_eq!(live.mode, Mode::Live, "live needs both flags to say so");
}

#[test]
fn an_explicit_mode_beats_the_legacy_flags() {
    let settings: Settings = serde_json::from_value(serde_json::json!({
        "delayPerOrder": 200, "maxOrdersPerMinute": 60,
        "mode": "shadow", "dryRun": false, "paused": false
    }))
    .unwrap();
    assert_eq!(settings.mode, Mode::Shadow);
}

#[test]
fn target_uses_camel_case_and_skips_absent_optionals() {
    let target = Target {
        item_id: "abc123".into(),
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
    let json = serde_json::to_value(&target).unwrap();
    assert_eq!(json["itemId"], "abc123");
    assert_eq!(json["targetPrice"], 0.1);
    assert!(json.get("rarityColor").is_none());
    assert!(json.get("createdAt").is_none());
}
