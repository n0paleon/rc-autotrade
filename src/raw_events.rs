//! Raw marketplace event snapshots kept with loot rows.
//!
//! Large frames are stored as an explicit preview so an unusually deep book
//! cannot turn the permanent loot log into an unbounded event archive.

use serde_json::{Map, Value, json};

const MAX_EVENT_BYTES: usize = 256 * 1024;
const MAX_PREVIEW_CHARS: usize = 8 * 1024;

fn snapshot_payload(value: &Value) -> Option<Value> {
    if !value.is_object() {
        return None;
    }
    let json = serde_json::to_string(value).ok()?;
    if json.len() > MAX_EVENT_BYTES {
        let preview: String = json.chars().take(MAX_PREVIEW_CHARS).collect();
        return Some(json!({
            "__truncatedForStorage": true,
            "originalBytes": json.len(),
            "preview": preview
        }));
    }
    Some(value.clone())
}

/// Keeps the purchase request body, market signal and optional result frame as
/// JSON-safe snapshots.
pub fn sanitize(raw: &Value) -> Option<Value> {
    if !raw.is_object() {
        return None;
    }
    let request = raw.get("purchaseRequest").and_then(snapshot_payload);
    let price_event = raw.get("priceEvent").and_then(snapshot_payload);
    let result_event = raw.get("resultEvent").and_then(snapshot_payload);
    if request.is_none() && price_event.is_none() && result_event.is_none() {
        return None;
    }
    let mut out = Map::new();
    out.insert(
        "purchaseRequest".to_string(),
        request.unwrap_or(Value::Null),
    );
    out.insert("priceEvent".to_string(), price_event.unwrap_or(Value::Null));
    out.insert(
        "resultEvent".to_string(),
        result_event.unwrap_or(Value::Null),
    );
    Some(Value::Object(out))
}

/// Serializes raw event snapshots for the `orders.raw_events` TEXT column.
pub fn encode(raw: &Value) -> Option<String> {
    sanitize(raw).and_then(|events| serde_json::to_string(&events).ok())
}

/// Reads the TEXT column (or an already-parsed object) into safe snapshots.
pub fn decode(value: Option<&str>) -> Option<Value> {
    let text = value?;
    if text.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_str(text).ok()?;
    sanitize(&parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_both_frames() {
        let raw = json!({
            "priceEvent": { "cmd": "marketplace_orders_update" },
            "resultEvent": { "cmd": "marketplace_orders_filled" }
        });
        let sanitized = sanitize(&raw).unwrap();
        assert_eq!(sanitized["priceEvent"]["cmd"], "marketplace_orders_update");
        assert_eq!(sanitized["resultEvent"]["cmd"], "marketplace_orders_filled");
    }

    #[test]
    fn keeps_the_purchase_request_body() {
        let raw = json!({
            "purchaseRequest": {
                "action": "marketplace",
                "itemId": "6319f7baa8ce530569ed16b9",
                "totalCount": 3
            },
            "priceEvent": { "cmd": "marketplace_orders_update" }
        });
        let sanitized = sanitize(&raw).unwrap();
        assert_eq!(sanitized["purchaseRequest"]["action"], "marketplace");
        assert_eq!(sanitized["purchaseRequest"]["totalCount"], 3);
        assert!(sanitized["resultEvent"].is_null());
    }

    #[test]
    fn emits_nulls_for_the_missing_side() {
        let sanitized = sanitize(&json!({ "priceEvent": { "a": 1 } })).unwrap();
        assert!(sanitized["resultEvent"].is_null());
    }

    #[test]
    fn returns_none_when_there_is_nothing_to_keep() {
        assert!(sanitize(&json!({})).is_none());
        assert!(sanitize(&json!({ "priceEvent": "not an object" })).is_none());
    }

    #[test]
    fn truncates_oversized_frames_with_a_preview() {
        let huge = json!({ "priceEvent": { "blob": "x".repeat(300 * 1024) } });
        let sanitized = sanitize(&huge).unwrap();
        let price = &sanitized["priceEvent"];
        assert_eq!(price["__truncatedForStorage"], true);
        assert!(price["originalBytes"].as_u64().unwrap() > 256 * 1024);
        assert_eq!(
            price["preview"].as_str().unwrap().chars().count(),
            MAX_PREVIEW_CHARS
        );
    }
}
