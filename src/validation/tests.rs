use super::{jwt_expiry, validate_session_input, validate_settings, validate_target};
use crate::model::Mode;
use serde_json::{Value, json};

fn valid_target() -> Value {
    json!({
        "itemId": "6319f7baa8ce530569ed16b9",
        "label": "Hashboard",
        "targetPrice": 0.1,
        "desiredQty": 10,
        "highStockThreshold": 20,
        "minAcceptableQty": 1
    })
}

#[test]
fn accepts_a_normal_target() {
    let target = validate_target(&valid_target(), &[], None).unwrap();
    assert_eq!(target.item_id, "6319f7baa8ce530569ed16b9");
    assert_eq!(target.item_type, "mutation_component");
    assert!(target.allow_partial_order);
    assert!(target.active);
}

#[test]
fn extracts_the_id_from_a_marketplace_url() {
    let mut payload = valid_target();
    payload["itemId"] = json!("https://rollercoin.com/marketplace/6319f7baa8ce530569ed16b9?x=1");
    let target = validate_target(&payload, &[], None).unwrap();
    assert_eq!(target.item_id, "6319f7baa8ce530569ed16b9");
}

#[test]
fn rejects_duplicate_ids_but_allows_editing_the_same_row() {
    let existing = vec!["6319f7baa8ce530569ed16b9".to_string()];
    let errors = validate_target(&valid_target(), &existing, None).unwrap_err();
    assert!(errors.contains_key("itemId"));
    assert!(validate_target(&valid_target(), &existing, Some("6319f7baa8ce530569ed16b9")).is_ok());
}

#[test]
fn rejects_stock_threshold_below_desired_quantity() {
    let mut payload = valid_target();
    payload["highStockThreshold"] = json!(5);
    let errors = validate_target(&payload, &[], None).unwrap_err();
    assert!(errors["highStockThreshold"].contains("greater than or equal"));
}

#[test]
fn validates_settings_ranges() {
    let patch = validate_settings(&json!({ "delayPerOrder": 500, "mode": "shadow" })).unwrap();
    assert_eq!(patch.delay_per_order, Some(500));
    assert_eq!(patch.mode, Some(Mode::Shadow));
    assert!(validate_settings(&json!({ "delayPerOrder": 10 })).is_err());
}

#[test]
fn validates_the_total_price_markup() {
    let patch = validate_settings(&json!({ "totalPriceMarkupPct": 10 })).unwrap();
    assert_eq!(patch.total_price_markup_pct, Some(10.0));
    let patch = validate_settings(&json!({ "totalPriceMarkupPct": 7.25 })).unwrap();
    assert_eq!(patch.total_price_markup_pct, Some(7.25));
    // 0 is meaningful: it keeps the exact listing total.
    let patch = validate_settings(&json!({ "totalPriceMarkupPct": 0 })).unwrap();
    assert_eq!(patch.total_price_markup_pct, Some(0.0));

    for invalid in [
        json!({ "totalPriceMarkupPct": -0.5 }),
        json!({ "totalPriceMarkupPct": 100.01 }),
        json!({ "totalPriceMarkupPct": 1.234 }),
        json!({ "totalPriceMarkupPct": "abc" }),
        json!({ "totalPriceMarkupPct": null }),
    ] {
        let errors = validate_settings(&invalid).unwrap_err();
        assert!(
            errors.contains_key("totalPriceMarkupPct"),
            "accepted {invalid}"
        );
    }
}

#[test]
fn rejects_an_unknown_mode() {
    let errors = validate_settings(&json!({ "mode": "armed" })).unwrap_err();
    assert!(errors["mode"].contains("off, shadow or live"));
    assert!(validate_settings(&json!({ "mode": true })).is_err());
}

#[test]
fn folds_the_legacy_flags_into_one_mode() {
    let cases = [
        (json!({ "paused": true }), Mode::Off),
        (json!({ "paused": true, "dryRun": false }), Mode::Off),
        (json!({ "dryRun": true, "paused": false }), Mode::Shadow),
        (json!({ "dryRun": false, "paused": false }), Mode::Live),
    ];
    for (body, expected) in cases {
        assert_eq!(
            validate_settings(&body).unwrap().mode,
            Some(expected),
            "{body}"
        );
    }
    // One flag alone is not enough to claim the worker is armed for live.
    assert_eq!(
        validate_settings(&json!({ "dryRun": false })).unwrap().mode,
        None
    );
    assert_eq!(
        validate_settings(&json!({ "paused": false })).unwrap().mode,
        None
    );
}

#[test]
fn an_explicit_mode_wins_over_the_legacy_flags() {
    let patch =
        validate_settings(&json!({ "mode": "shadow", "paused": true, "dryRun": false })).unwrap();
    assert_eq!(patch.mode, Some(Mode::Shadow));
}

#[test]
fn strips_bearer_prefix_and_clears_on_request() {
    let patch = validate_session_input(&json!({
        "accessToken": "Bearer abcdefghijkl",
        "clearCookie": true
    }))
    .unwrap();
    assert_eq!(patch.access_token.as_deref(), Some("abcdefghijkl"));
    assert!(patch.clear_cookie);
}

#[test]
fn reads_jwt_expiry_without_verifying_the_signature() {
    // {"exp":1700000000} base64url without padding
    let token = "x.eyJleHAiOjE3MDAwMDAwMDB9.y";
    assert_eq!(jwt_expiry(token), Some(1_700_000_000_000));
    assert_eq!(jwt_expiry("not-a-jwt"), None);
}
