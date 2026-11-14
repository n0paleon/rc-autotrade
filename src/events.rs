//! Upstream frame normaliser.
//!
//! The Rollercoin socket is unofficial: field names drift between
//! `item_id`/`itemId`, `list`/`orders`, etc. This module turns whichever shape
//! arrives into one stable `UpstreamEvent` so the engine never has to guess.

use serde::Serialize;
use serde_json::Value;

use crate::constants::PRICE_SCALE;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub price_raw: f64,
    pub quantity: i64,
    pub unit_price: f64,
}

impl Listing {
    pub fn from_raw(price_raw: f64, quantity: f64) -> Option<Self> {
        if !price_raw.is_finite() || price_raw <= 0.0 {
            return None;
        }
        if !quantity.is_finite() || quantity < 1.0 {
            return None;
        }
        Some(Self {
            price_raw,
            quantity: quantity.floor() as i64,
            unit_price: price_raw / PRICE_SCALE,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Market,
    Fill,
    Other,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Market => "market",
            Kind::Fill => "fill",
            Kind::Other => "other",
        }
    }
}

/// Upstream channels owned by the marketplace.
///
/// The socket also carries prediction-market, reward-statistic and exchange
/// rate messages. They share the same wire but are not market activity, so the
/// frame counters must filter on the command prefix instead of counting every
/// envelope that arrives.
pub fn is_marketplace_cmd(cmd: &str) -> bool {
    cmd.starts_with("marketplace_")
}

/// True when a raw upstream frame belongs to a marketplace channel.
pub fn is_marketplace_frame(frame: &Value) -> bool {
    frame
        .get("cmd")
        .and_then(Value::as_str)
        .is_some_and(is_marketplace_cmd)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FillEvent {
    pub order_id: Option<String>,
    pub count: i64,
    pub price_raw: f64,
    pub unit_price: f64,
    pub status: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamEvent {
    pub cmd: String,
    pub item_id: Option<String>,
    pub currency: Option<String>,
    pub item_type: Option<String>,
    pub kind: Kind,
    pub allowed_price_min: Option<f64>,
    pub allowed_price_max: Option<f64>,
    pub rest: Option<Listing>,
    pub total_count: Option<i64>,
    pub listings: Vec<Listing>,
    pub fill: Option<FillEvent>,
}

/// `X !== undefined && X !== null && X !== ""`
fn defined(value: Option<&Value>) -> Option<&Value> {
    match value {
        Some(Value::Null) | None => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(other) => Some(other),
    }
}

fn to_number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if let Some(number) = value.as_f64() {
        return number.is_finite().then_some(number);
    }
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        return trimmed
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite());
    }
    None
}

/// `String(value)` for the scalar types the feed actually uses.
fn to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer.to_string()
            } else if let Some(unsigned) = number.as_u64() {
                unsigned.to_string()
            } else {
                number.to_string()
            }
        }
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}

fn parse_listing(raw: &Value) -> Option<Listing> {
    let source = raw.as_object()?;
    let price_raw = to_number(
        defined(source.get("price"))
            .or_else(|| defined(source.get("unit_price")))
            .or_else(|| defined(source.get("unitPrice"))),
    )?;
    let quantity = to_number(
        defined(source.get("quantity"))
            .or_else(|| defined(source.get("count")))
            .or_else(|| defined(source.get("qty")))
            .or_else(|| defined(source.get("amount")))
            .or_else(|| defined(source.get("total_count"))),
    )?;
    Listing::from_raw(price_raw, quantity)
}

fn parse_allowed_price(raw: Option<&Value>) -> Option<f64> {
    let value = to_number(raw)?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some(value / PRICE_SCALE)
}

fn parse_rest(raw: Option<&Value>) -> Option<Listing> {
    parse_listing(raw?)
}

/// Normalises the handful of envelopes the worker subscribes to.
pub fn normalize_upstream_event(frame: &Value) -> UpstreamEvent {
    let message = frame.as_object();
    let cmd = message
        .and_then(|object| object.get("cmd"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let empty = Value::Object(Default::default());
    let value = message
        .and_then(|object| object.get("value"))
        .filter(|value| value.is_object())
        .unwrap_or(&empty);
    let value_object = value.as_object().expect("checked above");
    // Borrow the payload instead of cloning it: a market frame carries the whole
    // listing ladder, and copying that tree only to read three fields cost up to
    // ~90% of this function on a deep book (measured, 40 levels).
    let data = value_object
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(&empty);
    let data_object = data.as_object().expect("checked above");

    let raw_item_id = defined(value_object.get("item_id"))
        .or_else(|| defined(value_object.get("itemId")))
        .or_else(|| defined(data_object.get("item_id")))
        .or_else(|| defined(data_object.get("itemId")))
        .or_else(|| defined(value_object.get("item").and_then(|item| item.get("id"))))
        .or_else(|| defined(value_object.get("item").and_then(|item| item.get("_id"))))
        .or_else(|| defined(data_object.get("item").and_then(|item| item.get("id"))));

    let item_id = raw_item_id.map(|value| to_text(value).to_lowercase());
    let currency = defined(value_object.get("currency"))
        .or_else(|| defined(data_object.get("currency")))
        .map(|value| to_text(value).to_uppercase());
    let item_type = defined(value_object.get("item_type"))
        .or_else(|| defined(value_object.get("itemType")))
        .or_else(|| defined(data_object.get("item_type")))
        .or_else(|| defined(data_object.get("itemType")))
        .map(to_text);

    let raw_list = defined(data_object.get("list"))
        .or_else(|| defined(value_object.get("list")))
        .or_else(|| defined(data_object.get("orders")))
        .or_else(|| defined(value_object.get("orders")));

    let mut listings: Vec<Listing> = raw_list
        .and_then(Value::as_array)
        .map(|entries| entries.iter().filter_map(parse_listing).collect())
        .unwrap_or_default();
    listings.sort_by(|a, b| {
        a.price_raw
            .partial_cmp(&b.price_raw)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.quantity.cmp(&a.quantity))
    });

    let allowed_price_min = parse_allowed_price(
        defined(data_object.get("allowedPriceMin"))
            .or_else(|| defined(value_object.get("allowedPriceMin")))
            .or_else(|| defined(data_object.get("allowed_price_min")))
            .or_else(|| defined(value_object.get("allowed_price_min"))),
    );
    let allowed_price_max = parse_allowed_price(
        defined(data_object.get("allowedPriceMax"))
            .or_else(|| defined(value_object.get("allowedPriceMax")))
            .or_else(|| defined(data_object.get("allowed_price_max")))
            .or_else(|| defined(value_object.get("allowed_price_max"))),
    );

    let rest =
        parse_rest(defined(data_object.get("rest")).or_else(|| defined(value_object.get("rest"))));

    let total_count = to_number(
        defined(data_object.get("totalCount"))
            .or_else(|| defined(data_object.get("total_count")))
            .or_else(|| defined(value_object.get("totalCount")))
            .or_else(|| defined(value_object.get("total_count"))),
    )
    .filter(|value| *value >= 0.0)
    .map(|value| value.floor() as i64);

    let order_id_raw = defined(value_object.get("order_id"))
        .or_else(|| defined(value_object.get("orderId")))
        .or_else(|| defined(data_object.get("order_id")))
        .or_else(|| defined(data_object.get("orderId")));
    let status = to_number(
        defined(value_object.get("status")).or_else(|| defined(data_object.get("status"))),
    );
    let count = to_number(
        defined(value_object.get("count"))
            .or_else(|| defined(data_object.get("count")))
            .or_else(|| defined(value_object.get("totalCount")))
            .or_else(|| defined(value_object.get("total_count"))),
    )
    .unwrap_or(f64::NAN);
    let price_raw = to_number(
        defined(value_object.get("price"))
            .or_else(|| defined(data_object.get("price")))
            .or_else(|| defined(value_object.get("totalPrice")))
            .or_else(|| defined(value_object.get("total_price"))),
    )
    .unwrap_or(f64::NAN);

    let has_fill_shape = order_id_raw.is_some() && status.is_some();
    let kind = if !listings.is_empty() {
        Kind::Market
    } else if has_fill_shape {
        Kind::Fill
    } else {
        Kind::Other
    };

    let fill = if kind == Kind::Fill {
        let count_floor = if count.is_finite() {
            count.floor()
        } else {
            0.0
        };
        Some(FillEvent {
            order_id: order_id_raw.map(to_text),
            count: count_floor as i64,
            price_raw: if price_raw.is_finite() {
                price_raw
            } else {
                0.0
            },
            unit_price: if price_raw.is_finite() && count > 0.0 {
                price_raw / PRICE_SCALE / count
            } else {
                0.0
            },
            status: status.unwrap_or(f64::NAN),
        })
    } else {
        None
    };

    UpstreamEvent {
        cmd,
        item_id,
        currency,
        item_type,
        kind,
        allowed_price_min,
        allowed_price_max,
        rest,
        total_count,
        listings,
        fill,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalises_a_marketplace_update_and_sorts_listings() {
        let event = normalize_upstream_event(&json!({
            "cmd": "marketplace_orders_update",
            "value": {
                "item_id": "6571AA39CA8BFEF34ADCD9E9",
                "currency": "RLT",
                "item_type": "mutation_component",
                "data": {
                    "list": [
                        { "price": 120_000, "quantity": 4 },
                        { "price": 100_000, "quantity": 7 },
                        { "price": 0, "quantity": 9 }
                    ]
                }
            }
        }));

        assert_eq!(event.kind, Kind::Market);
        assert_eq!(event.item_id.as_deref(), Some("6571aa39ca8bfef34adcd9e9"));
        assert_eq!(event.currency.as_deref(), Some("RLT"));
        assert_eq!(event.listings.len(), 2);
        assert_eq!(event.listings[0].unit_price, 0.1);
        assert_eq!(event.listings[0].quantity, 7);
    }

    #[test]
    fn normalises_the_allowed_price_band() {
        let event = normalize_upstream_event(&json!({
            "cmd": "marketplace_orders_update",
            "value": {
                "currency": "RLT",
                "item_type": "rack",
                "item_id": "64490fc1547cfab9a2759638",
                "data": {
                    "allowedPriceMin": 367_500,
                    "allowedPriceMax": 666_225_000,
                    "list": [{ "price": 735_000, "quantity": 7 }]
                }
            }
        }));

        assert_eq!(event.kind, Kind::Market);
        assert_eq!(event.allowed_price_min, Some(0.3675));
        assert_eq!(event.allowed_price_max, Some(666.225));
    }

    #[test]
    fn leaves_the_band_null_when_the_frame_omits_it() {
        let event = normalize_upstream_event(&json!({
            "cmd": "marketplace_orders_update",
            "value": {
                "item_id": "64490fc1547cfab9a2759638",
                "currency": "RLT",
                "data": { "list": [{ "price": 735_000, "quantity": 7 }] }
            }
        }));

        assert_eq!(event.allowed_price_min, None);
        assert_eq!(event.allowed_price_max, None);
    }

    #[test]
    fn normalises_a_fill_with_a_total_price() {
        let event = normalize_upstream_event(&json!({
            "cmd": "marketplace_orders_filled",
            "value": {
                "item_id": "abc12345",
                "order_id": "order-7",
                "count": 5,
                "price": 500_000,
                "status": 12
            }
        }));

        assert_eq!(event.kind, Kind::Fill);
        let fill = event.fill.expect("fill");
        assert_eq!(fill.order_id.as_deref(), Some("order-7"));
        assert_eq!(fill.count, 5);
        assert_eq!(fill.unit_price, 0.1);
        assert_eq!(fill.status, 12.0);
    }

    #[test]
    fn ignores_frames_without_a_market_or_fill_shape() {
        let event = normalize_upstream_event(&json!({
            "cmd": "notification",
            "value": { "message": "hello" }
        }));

        assert_eq!(event.kind, Kind::Other);
        assert_eq!(event.item_id, None);
        assert!(event.listings.is_empty());
        assert!(event.fill.is_none());
    }

    #[test]
    fn accepts_camel_case_and_rest_tails() {
        let event = normalize_upstream_event(&json!({
            "cmd": "marketplace_orders_update",
            "value": {
                "itemId": "abc12345",
                "currency": "rlt",
                "data": {
                    "orders": [{ "unitPrice": 250_000, "qty": 3 }],
                    "rest": { "price": 300_000, "quantity": 500 }
                }
            }
        }));

        assert_eq!(event.item_id.as_deref(), Some("abc12345"));
        assert_eq!(event.currency.as_deref(), Some("RLT"));
        assert_eq!(event.listings[0].unit_price, 0.25);
        assert_eq!(event.rest.map(|rest| rest.quantity), Some(500));
    }

    #[test]
    fn marketplace_counters_ignore_other_socket_channels() {
        assert!(is_marketplace_cmd("marketplace_orders_update"));
        assert!(is_marketplace_cmd("marketplace_orders_filled"));
        assert!(is_marketplace_cmd("marketplace_activity"));
        assert!(!is_marketplace_cmd("prediction_market_live_price"));
        assert!(!is_marketplace_cmd("pe_reward_statistic_update"));
        assert!(!is_marketplace_cmd("exchange_rates_updated"));
        assert!(!is_marketplace_cmd(""));

        assert!(is_marketplace_frame(&json!({
            "cmd": "marketplace_orders_update",
            "value": { "item_id": "abc123" }
        })));
        assert!(is_marketplace_frame(&json!({
            "cmd": "marketplace_orders_filled",
            "value": { "order_id": "order-1" }
        })));
        assert!(!is_marketplace_frame(&json!({
            "cmd": "prediction_market_updates",
            "value": {}
        })));
        assert!(!is_marketplace_frame(&json!({ "value": {} })));
    }
}
