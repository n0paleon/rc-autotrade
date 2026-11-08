//! Per-order latency marks, ported from `src/shared/timings.js`.
//!
//! Stored as JSON in `orders.timings`; every timestamp is an absolute epoch
//! millisecond on the worker clock. The shape is versioned so old rows keep
//! loading after future additions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

const TIME_KEYS: [&str; 11] = [
    "signalAt",
    "handleAt",
    "orderAt",
    "dispatchAt",
    "sendAt",
    "responseAt",
    "completeAt",
    "acceptedAt",
    "fillAt",
    "fillHandledAt",
    "failedAt",
];

fn version() -> u8 {
    1
}

fn round_ms(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// The sanitised timings object exactly as the UI and the debugger expect it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    #[serde(default = "version")]
    pub v: u8,
    #[serde(rename = "signalAt", skip_serializing_if = "Option::is_none", default)]
    pub signal_at: Option<f64>,
    #[serde(rename = "handleAt", skip_serializing_if = "Option::is_none", default)]
    pub handle_at: Option<f64>,
    #[serde(rename = "orderAt", skip_serializing_if = "Option::is_none", default)]
    pub order_at: Option<f64>,
    #[serde(
        rename = "dispatchAt",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub dispatch_at: Option<f64>,
    #[serde(rename = "sendAt", skip_serializing_if = "Option::is_none", default)]
    pub send_at: Option<f64>,
    #[serde(
        rename = "responseAt",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub response_at: Option<f64>,
    #[serde(
        rename = "completeAt",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub complete_at: Option<f64>,
    /// Monotonic dispatch → first response byte (TTFB). Unlike the
    /// `sendAt`/`responseAt` delta this is immune to wall-clock changes and
    /// excludes any 401 retry: it is the TTFB of the attempt that responded.
    #[serde(rename = "ttfbMs", skip_serializing_if = "Option::is_none", default)]
    pub ttfb_ms: Option<f64>,
    #[serde(
        rename = "acceptedAt",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub accepted_at: Option<f64>,
    #[serde(rename = "fillAt", skip_serializing_if = "Option::is_none", default)]
    pub fill_at: Option<f64>,
    #[serde(
        rename = "fillHandledAt",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub fill_handled_at: Option<f64>,
    #[serde(rename = "failedAt", skip_serializing_if = "Option::is_none", default)]
    pub failed_at: Option<f64>,
    #[serde(rename = "dnsMs", skip_serializing_if = "Option::is_none", default)]
    pub dns_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub attempts: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub unauthorized: Option<bool>,
    #[serde(rename = "signalCmd", skip_serializing_if = "Option::is_none", default)]
    pub signal_cmd: Option<String>,
    #[serde(rename = "dnsHost", skip_serializing_if = "Option::is_none", default)]
    pub dns_host: Option<String>,
    #[serde(rename = "dnsMode", skip_serializing_if = "Option::is_none", default)]
    pub dns_mode: Option<String>,
    #[serde(
        rename = "dnsAddress",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub dns_address: Option<String>,
    #[serde(rename = "dnsFamily", skip_serializing_if = "Option::is_none", default)]
    pub dns_family: Option<u8>,
    #[serde(rename = "dnsError", skip_serializing_if = "Option::is_none", default)]
    pub dns_error: Option<String>,
    /// `new` (fresh connection), `reused` (pooled keep-alive socket) or
    /// `unknown` (IP upstream / not observed).
    #[serde(
        rename = "socketMode",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub socket_mode: Option<String>,
}

fn number(raw: &Value, key: &str) -> Option<f64> {
    raw.get(key)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
}

fn trimmed(raw: &Value, key: &str, max: usize) -> Option<String> {
    let text = raw.get(key)?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(max).collect())
}

impl Timings {
    pub fn has_time(&self) -> bool {
        self.signal_at.is_some()
            || self.handle_at.is_some()
            || self.order_at.is_some()
            || self.dispatch_at.is_some()
            || self.send_at.is_some()
            || self.response_at.is_some()
            || self.complete_at.is_some()
            || self.accepted_at.is_some()
            || self.fill_at.is_some()
            || self.fill_handled_at.is_some()
            || self.failed_at.is_some()
    }
}

/// Keeps only sane numeric marks. Returns `None` when nothing useful remains,
/// so old loot rows without a timings column stay untouched.
pub fn sanitize(raw: &Value) -> Option<Timings> {
    if !raw.is_object() {
        return None;
    }

    let mut out = Timings {
        v: 1,
        ..Default::default()
    };

    out.signal_at = number(raw, "signalAt").filter(|v| *v > 0.0).map(round_ms);
    out.handle_at = number(raw, "handleAt").filter(|v| *v > 0.0).map(round_ms);
    out.order_at = number(raw, "orderAt").filter(|v| *v > 0.0).map(round_ms);
    out.dispatch_at = number(raw, "dispatchAt").filter(|v| *v > 0.0).map(round_ms);
    out.send_at = number(raw, "sendAt").filter(|v| *v > 0.0).map(round_ms);
    out.response_at = number(raw, "responseAt").filter(|v| *v > 0.0).map(round_ms);
    out.complete_at = number(raw, "completeAt").filter(|v| *v > 0.0).map(round_ms);
    out.ttfb_ms = number(raw, "ttfbMs").filter(|v| *v >= 0.0).map(round_ms);
    out.accepted_at = number(raw, "acceptedAt").filter(|v| *v > 0.0).map(round_ms);
    out.fill_at = number(raw, "fillAt").filter(|v| *v > 0.0).map(round_ms);
    out.fill_handled_at = number(raw, "fillHandledAt")
        .filter(|v| *v > 0.0)
        .map(round_ms);
    out.failed_at = number(raw, "failedAt").filter(|v| *v > 0.0).map(round_ms);
    out.dns_ms = number(raw, "dnsMs").filter(|v| *v >= 0.0).map(round_ms);

    out.attempts = number(raw, "attempts")
        .filter(|v| *v > 0.0)
        .map(|v| (v.trunc() as u8).min(10));
    if raw.get("unauthorized").and_then(Value::as_bool) == Some(true) {
        out.unauthorized = Some(true);
    }
    out.signal_cmd = trimmed(raw, "signalCmd", 64);
    out.dns_host = trimmed(raw, "dnsHost", 128);
    out.dns_mode = trimmed(raw, "dnsMode", 16);
    out.dns_address = trimmed(raw, "dnsAddress", 64);
    out.dns_family = number(raw, "dnsFamily").and_then(|v| match v as i64 {
        4 => Some(4),
        6 => Some(6),
        _ => None,
    });
    out.dns_error = trimmed(raw, "dnsError", 120);
    out.socket_mode = trimmed(raw, "socketMode", 16)
        .filter(|mode| matches!(mode.as_str(), "new" | "reused" | "unknown"));

    if out.has_time() { Some(out) } else { None }
}

/// Serializes a timings object for the `orders.timings` TEXT column.
pub fn encode(raw: &Value) -> Option<String> {
    sanitize(raw).and_then(|timings| serde_json::to_string(&timings).ok())
}

/// Reads the TEXT column (or an already-parsed object) back into a clean value.
pub fn decode(value: Option<&str>) -> Option<Timings> {
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
    use serde_json::json;

    #[test]
    fn keeps_time_marks_and_drops_noise() {
        let raw = json!({
            "signalAt": 1_700_000_000_123i64,
            "sendAt": 1_700_000_000_125.5,
            "attempts": 2,
            "signalCmd": "marketplace_orders_update",
            "junk": "ignored"
        });
        let timings = sanitize(&raw).unwrap();
        assert_eq!(timings.v, 1);
        assert_eq!(timings.signal_at, Some(1_700_000_000_123.0));
        assert_eq!(timings.send_at, Some(1_700_000_000_125.5));
        assert_eq!(timings.attempts, Some(2));
        assert_eq!(
            timings.signal_cmd.as_deref(),
            Some("marketplace_orders_update")
        );
        let json = serde_json::to_value(&timings).unwrap();
        assert!(json.get("junk").is_none());
    }

    #[test]
    fn returns_none_without_a_single_time_mark() {
        assert!(sanitize(&json!({ "attempts": 3 })).is_none());
        assert!(sanitize(&json!({})).is_none());
    }

    #[test]
    fn round_trips_through_the_text_column() {
        let raw = json!({ "signalAt": 10, "responseAt": 25, "dnsMs": 0.1 });
        let encoded = encode(&raw).unwrap();
        let decoded = decode(Some(&encoded)).unwrap();
        assert_eq!(decoded.signal_at, Some(10.0));
        assert_eq!(decoded.response_at, Some(25.0));
        assert_eq!(decoded.dns_ms, Some(0.1));
    }

    #[test]
    fn keeps_the_monotonic_ttfb() {
        let timings = sanitize(&json!({ "signalAt": 10, "ttfbMs": 123.456 })).unwrap();
        assert_eq!(timings.ttfb_ms, Some(123.456));

        // A missing or negative duration must never look like a fast response.
        assert!(
            sanitize(&json!({ "signalAt": 10, "ttfbMs": -1 }))
                .and_then(|t| t.ttfb_ms)
                .is_none()
        );
    }

    #[test]
    fn caps_attempts_and_ignores_bad_families() {
        let timings = sanitize(&json!({ "signalAt": 5, "attempts": 99, "dnsFamily": 7 })).unwrap();
        assert_eq!(timings.attempts, Some(10));
        assert_eq!(timings.dns_family, None);
    }

    #[test]
    fn keeps_only_known_socket_modes() {
        let reused = sanitize(&json!({ "signalAt": 5, "socketMode": "reused" })).unwrap();
        assert_eq!(reused.socket_mode.as_deref(), Some("reused"));

        let junk = sanitize(&json!({ "signalAt": 5, "socketMode": "sideways" })).unwrap();
        assert_eq!(junk.socket_mode, None);
    }
}
