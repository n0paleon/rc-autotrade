//! Input validation, ported from `src/shared/validation.js`.
//!
//! The API must reject the same inputs the Bun worker rejects and return the
//! same field-keyed error map, because the React deck renders those messages
//! directly under each form field.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde_json::Value;

use crate::constants::Limits;
use crate::model::Target;
use crate::rules::round_price;

pub type FieldErrors = BTreeMap<String, String>;

fn to_number(value: &Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return number.is_finite().then_some(number);
    }
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        return trimmed.parse::<f64>().ok().filter(|n| n.is_finite());
    }
    None
}

fn merge_error(errors: &mut FieldErrors, field: &str, message: impl Into<String>) {
    errors.insert(field.to_string(), message.into());
}

/// Strips control characters and trims, matching the JS `CONTROL_CHARS_RE`.
pub fn clean_text(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\t')
        .collect::<String>()
        .trim()
        .to_string()
}

fn is_item_id_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn valid_item_id(value: &str) -> bool {
    (6..=64).contains(&value.len()) && value.bytes().all(is_item_id_char)
}

fn valid_item_type(value: &str) -> bool {
    let bytes = value.as_bytes();
    if !(2..=40).contains(&bytes.len()) {
        return false;
    }
    if !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

fn valid_hex_color(value: &str) -> bool {
    let digits = value.strip_prefix('#').unwrap_or(value);
    digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit())
}

fn strip_bearer(value: &str) -> &str {
    let lower = value.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("bearer ") {
        return &value[value.len() - rest.len()..];
    }
    value
}

/// Accepts a raw item id or a full Rollercoin marketplace URL.
pub fn normalize_item_id(raw: &Value) -> Option<String> {
    let mut value = raw.as_str()?.trim().to_string();
    if value.is_empty() {
        return None;
    }

    if value.to_ascii_lowercase().starts_with("http://")
        || value.to_ascii_lowercase().starts_with("https://")
    {
        // The id is the last non-empty path segment; query/fragment are dropped.
        let without_scheme = value
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&value);
        let without_host = without_scheme
            .split_once('/')
            .map(|(_, rest)| rest)
            .unwrap_or("");
        let path = without_host
            .split(['?', '#'])
            .next()
            .unwrap_or(without_host);
        value = path
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .unwrap_or("")
            .to_string();
    }

    value = value
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();

    valid_item_id(&value).then_some(value)
}

/// Validates and normalises one target configuration.
pub fn validate_target(
    input: &Value,
    existing_ids: &[String],
    original_id: Option<&str>,
) -> Result<Target, FieldErrors> {
    let mut errors = FieldErrors::new();
    let source = input.as_object();

    let item_id = normalize_item_id(input.get("itemId").unwrap_or(&Value::Null));
    match &item_id {
        None => merge_error(
            &mut errors,
            "itemId",
            "Item ID must be 6–64 letters, numbers, dashes or underscores (a marketplace URL also works).",
        ),
        Some(id) => {
            let lower = id.to_ascii_lowercase();
            let duplicate = existing_ids
                .iter()
                .any(|existing| existing.to_ascii_lowercase() == lower);
            if duplicate && original_id.map(|o| o.to_ascii_lowercase()) != Some(lower) {
                merge_error(
                    &mut errors,
                    "itemId",
                    "That item is already on the target list.",
                );
            }
        }
    }

    let target_price = input.get("targetPrice").and_then(to_number);
    match target_price {
        None => merge_error(
            &mut errors,
            "targetPrice",
            "Target price must be greater than 0 RLT.",
        ),
        Some(price) if price <= 0.0 => merge_error(
            &mut errors,
            "targetPrice",
            "Target price must be greater than 0 RLT.",
        ),
        Some(price) if price > Limits::TARGET_PRICE_MAX => merge_error(
            &mut errors,
            "targetPrice",
            format!(
                "Target price cannot exceed {} RLT.",
                Limits::TARGET_PRICE_MAX as i64
            ),
        ),
        Some(price) if (price * 1e6 - (price * 1e6).round()).abs() > 1e-6 => merge_error(
            &mut errors,
            "targetPrice",
            "Target price supports at most 6 decimal places.",
        ),
        Some(_) => {}
    }

    let desired_qty = input.get("desiredQty").and_then(to_number);
    match desired_qty {
        Some(qty) if qty >= 1.0 && qty.fract() == 0.0 && qty <= Limits::QTY_MAX as f64 => {}
        Some(qty) if qty >= 1.0 && qty.fract() == 0.0 => merge_error(
            &mut errors,
            "desiredQty",
            format!("Desired quantity cannot exceed {}.", Limits::QTY_MAX),
        ),
        _ => merge_error(
            &mut errors,
            "desiredQty",
            "Desired quantity must be a whole number of at least 1.",
        ),
    }

    let high_stock = input.get("highStockThreshold").and_then(to_number);
    match high_stock {
        Some(qty)
            if qty >= 1.0 && qty.fract() == 0.0 && qty <= Limits::STOCK_THRESHOLD_MAX as f64 =>
        {
            if let Some(desired) = desired_qty.filter(|d| d.fract() == 0.0) {
                if qty < desired {
                    merge_error(
                        &mut errors,
                        "highStockThreshold",
                        "High-stock threshold must be greater than or equal to desired quantity.",
                    );
                }
            }
        }
        Some(qty) if qty >= 1.0 && qty.fract() == 0.0 => merge_error(
            &mut errors,
            "highStockThreshold",
            format!(
                "High-stock threshold cannot exceed {}.",
                Limits::STOCK_THRESHOLD_MAX
            ),
        ),
        _ => merge_error(
            &mut errors,
            "highStockThreshold",
            "High-stock threshold must be a whole number of at least 1.",
        ),
    }

    let min_qty = input.get("minAcceptableQty").and_then(to_number);
    match min_qty {
        Some(qty) if qty >= 1.0 && qty.fract() == 0.0 => {
            if let Some(desired) = desired_qty.filter(|d| d.fract() == 0.0) {
                if qty > desired {
                    merge_error(
                        &mut errors,
                        "minAcceptableQty",
                        "Minimum acceptable quantity cannot exceed desired quantity.",
                    );
                }
            }
        }
        _ => merge_error(
            &mut errors,
            "minAcceptableQty",
            "Minimum acceptable quantity must be a whole number of at least 1.",
        ),
    }

    let mut label = String::new();
    if let Some(raw_label) = input.get("label") {
        if !raw_label.is_null() {
            match raw_label.as_str() {
                None => merge_error(&mut errors, "label", "Label must be text."),
                Some(text) => {
                    label = text
                        .chars()
                        .filter(|ch| !ch.is_control())
                        .collect::<String>()
                        .trim()
                        .chars()
                        .take(Limits::LABEL_MAX)
                        .collect();
                }
            }
        }
    }

    let mut item_type = "mutation_component".to_string();
    if let Some(raw_type) = input.get("itemType") {
        if let Some(text) = raw_type.as_str() {
            if !text.trim().is_empty() {
                if valid_item_type(text.trim()) {
                    item_type = text.trim().to_ascii_lowercase();
                } else {
                    merge_error(
                        &mut errors,
                        "itemType",
                        "Item type must be 2–40 letters, numbers, dashes or underscores.",
                    );
                }
            }
        } else if !raw_type.is_null() {
            merge_error(
                &mut errors,
                "itemType",
                "Item type must be 2–40 letters, numbers, dashes or underscores.",
            );
        }
    }

    let mut rarity_color = None;
    if let Some(raw_color) = input.get("rarityColor") {
        if let Some(text) = raw_color.as_str() {
            if !text.trim().is_empty() {
                if valid_hex_color(text.trim()) {
                    rarity_color = Some(text.trim().trim_start_matches('#').to_ascii_lowercase());
                } else {
                    merge_error(
                        &mut errors,
                        "rarityColor",
                        "Rarity color must be a 6-digit hex color.",
                    );
                }
            }
        } else if !raw_color.is_null() {
            merge_error(
                &mut errors,
                "rarityColor",
                "Rarity color must be a 6-digit hex color.",
            );
        }
    }

    let allow_partial_order = input
        .get("allowPartialOrder")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let active = input.get("active").and_then(Value::as_bool).unwrap_or(true);

    if !errors.is_empty() {
        return Err(errors);
    }

    let _ = source;
    Ok(Target {
        item_id: item_id.expect("checked above"),
        label,
        item_type,
        rarity_color,
        target_price: round_price(target_price.expect("checked above")),
        desired_qty: desired_qty.expect("checked above").trunc() as i64,
        high_stock_threshold: high_stock.expect("checked above").trunc() as i64,
        min_acceptable_qty: min_qty.expect("checked above").trunc() as i64,
        allow_partial_order,
        active,
        created_at: None,
        updated_at: None,
    })
}

/// Validates a whole target list; errors are keyed `"<index>.<field>"`.
pub fn validate_targets(
    input: &Value,
    existing_ids: &[String],
) -> Result<Vec<Target>, FieldErrors> {
    let Some(entries) = input.as_array() else {
        let mut errors = FieldErrors::new();
        merge_error(&mut errors, "configs", "Targets must be an array.");
        return Err(errors);
    };
    if entries.len() > Limits::CONFIGS_MAX {
        let mut errors = FieldErrors::new();
        merge_error(
            &mut errors,
            "configs",
            format!("At most {} targets are allowed.", Limits::CONFIGS_MAX),
        );
        return Err(errors);
    }

    let mut errors = FieldErrors::new();
    let mut values = Vec::new();
    let mut seen: Vec<String> = existing_ids.to_vec();

    for (index, entry) in entries.iter().enumerate() {
        match validate_target(entry, &seen, None) {
            Ok(target) => {
                seen.push(target.item_id.to_ascii_lowercase());
                values.push(target);
            }
            Err(field_errors) => {
                for (field, message) in field_errors {
                    errors.insert(format!("{index}.{field}"), message);
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(values)
    } else {
        Err(errors)
    }
}

/// Partial settings update; only the fields present in the request are touched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsPatch {
    pub delay_per_order: Option<i64>,
    pub max_orders_per_minute: Option<i64>,
    pub dry_run: Option<bool>,
    pub auto_refresh: Option<bool>,
    pub paused: Option<bool>,
}

pub fn validate_settings(input: &Value) -> Result<SettingsPatch, FieldErrors> {
    let mut errors = FieldErrors::new();
    let mut patch = SettingsPatch::default();
    let Some(source) = input.as_object() else {
        merge_error(&mut errors, "settings", "Settings must be an object.");
        return Err(errors);
    };

    if let Some(raw) = source.get("delayPerOrder") {
        match to_number(raw) {
            Some(value)
                if value.fract() == 0.0
                    && (Limits::DELAY_MIN as f64..=Limits::DELAY_MAX as f64).contains(&value) =>
            {
                patch.delay_per_order = Some(value as i64)
            }
            _ => merge_error(
                &mut errors,
                "delayPerOrder",
                format!(
                    "Delay must be a whole number between {} and {} ms.",
                    Limits::DELAY_MIN,
                    Limits::DELAY_MAX
                ),
            ),
        }
    }

    if let Some(raw) = source.get("maxOrdersPerMinute") {
        match to_number(raw) {
            Some(value)
                if value.fract() == 0.0
                    && (Limits::ORDERS_PER_MINUTE_MIN as f64
                        ..=Limits::ORDERS_PER_MINUTE_MAX as f64)
                        .contains(&value) =>
            {
                patch.max_orders_per_minute = Some(value as i64)
            }
            _ => merge_error(
                &mut errors,
                "maxOrdersPerMinute",
                format!(
                    "Order cap must be between {} and {} per minute.",
                    Limits::ORDERS_PER_MINUTE_MIN,
                    Limits::ORDERS_PER_MINUTE_MAX
                ),
            ),
        }
    }

    for (key, target) in [
        ("dryRun", &mut patch.dry_run),
        ("autoRefresh", &mut patch.auto_refresh),
        ("paused", &mut patch.paused),
    ] {
        if let Some(raw) = source.get(key) {
            match raw.as_bool() {
                Some(value) => *target = Some(value),
                None => merge_error(&mut errors, key, format!("{key} must be true or false.")),
            }
        }
    }

    if errors.is_empty() {
        Ok(patch)
    } else {
        Err(errors)
    }
}

/// Normalised session input: trimmed tokens, strip `Bearer`, honour clear flags.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionPatch {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub cookie: Option<String>,
    pub source: Option<String>,
    pub clear_access_token: bool,
    pub clear_refresh_token: bool,
    pub clear_cookie: bool,
}

const TOKEN_MIN: usize = 8;
const TOKEN_MAX: usize = 4096;

pub fn validate_session_input(input: &Value) -> Result<SessionPatch, FieldErrors> {
    let mut errors = FieldErrors::new();
    let mut patch = SessionPatch::default();
    let Some(source) = input.as_object() else {
        merge_error(&mut errors, "session", "Session input must be an object.");
        return Err(errors);
    };

    if let Some(raw) = source.get("accessToken") {
        match raw.as_str() {
            None => merge_error(&mut errors, "accessToken", "Access token must be text."),
            Some(text) => {
                let token = strip_bearer(text.trim());
                if token.trim().len() < TOKEN_MIN {
                    merge_error(&mut errors, "accessToken", "Access token looks too short.");
                } else if token.trim().len() > TOKEN_MAX {
                    merge_error(&mut errors, "accessToken", "Access token is too long.");
                } else {
                    patch.access_token = Some(token.trim().to_string());
                }
            }
        }
    }

    if let Some(raw) = source.get("refreshToken") {
        if !raw.is_null() {
            match raw.as_str() {
                None => merge_error(&mut errors, "refreshToken", "Refresh token must be text."),
                Some(text) => {
                    let token = text.trim();
                    if !token.is_empty() && !(TOKEN_MIN..=TOKEN_MAX).contains(&token.len()) {
                        merge_error(
                            &mut errors,
                            "refreshToken",
                            "Refresh token has an unexpected length.",
                        );
                    } else {
                        patch.refresh_token = Some(token.to_string());
                    }
                }
            }
        }
    }

    if let Some(raw) = source.get("cookie") {
        if !raw.is_null() {
            match raw.as_str() {
                None => merge_error(&mut errors, "cookie", "Cookie must be text."),
                Some(text) if text.len() > 8192 => {
                    merge_error(&mut errors, "cookie", "Cookie header is too long.")
                }
                Some(text) => patch.cookie = Some(text.trim().to_string()),
            }
        }
    }

    if let Some(raw) = source.get("source").and_then(Value::as_str) {
        patch.source = Some(raw.chars().take(40).collect());
    }

    for (key, target) in [
        ("clearAccessToken", &mut patch.clear_access_token),
        ("clearRefreshToken", &mut patch.clear_refresh_token),
        ("clearCookie", &mut patch.clear_cookie),
    ] {
        if let Some(raw) = source.get(key) {
            match raw.as_bool() {
                Some(value) => *target = value,
                None => merge_error(&mut errors, key, format!("{key} must be true or false.")),
            }
        }
    }

    if errors.is_empty() {
        Ok(patch)
    } else {
        Err(errors)
    }
}

/// `exp` claim from a JWT as epoch milliseconds, without verifying the
/// signature — the token is upstream's, not ours.
pub fn jwt_expiry(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    let exp = value.get("exp")?.as_f64()?;
    Some((exp * 1000.0) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        payload["itemId"] =
            json!("https://rollercoin.com/marketplace/6319f7baa8ce530569ed16b9?x=1");
        let target = validate_target(&payload, &[], None).unwrap();
        assert_eq!(target.item_id, "6319f7baa8ce530569ed16b9");
    }

    #[test]
    fn rejects_duplicate_ids_but_allows_editing_the_same_row() {
        let existing = vec!["6319f7baa8ce530569ed16b9".to_string()];
        let errors = validate_target(&valid_target(), &existing, None).unwrap_err();
        assert!(errors.contains_key("itemId"));
        assert!(
            validate_target(&valid_target(), &existing, Some("6319f7baa8ce530569ed16b9")).is_ok()
        );
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
        let patch = validate_settings(&json!({ "delayPerOrder": 500, "paused": true })).unwrap();
        assert_eq!(patch.delay_per_order, Some(500));
        assert_eq!(patch.paused, Some(true));
        assert!(validate_settings(&json!({ "delayPerOrder": 10 })).is_err());
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
}
