use serde_json::Value;

use crate::constants::Limits;
use crate::model::Target;
use crate::rules::round_price;

use super::{FieldErrors, merge_error, to_number, valid_hex_color, valid_item_id, valid_item_type};

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
            if let Some(desired) = desired_qty.filter(|d| d.fract() == 0.0)
                && qty < desired
            {
                merge_error(
                    &mut errors,
                    "highStockThreshold",
                    "High-stock threshold must be greater than or equal to desired quantity.",
                );
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
            if let Some(desired) = desired_qty.filter(|d| d.fract() == 0.0)
                && qty > desired
            {
                merge_error(
                    &mut errors,
                    "minAcceptableQty",
                    "Minimum acceptable quantity cannot exceed desired quantity.",
                );
            }
        }
        _ => merge_error(
            &mut errors,
            "minAcceptableQty",
            "Minimum acceptable quantity must be a whole number of at least 1.",
        ),
    }

    let mut label = String::new();
    if let Some(raw_label) = input.get("label")
        && !raw_label.is_null()
    {
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
