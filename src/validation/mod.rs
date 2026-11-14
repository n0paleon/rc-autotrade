//! Input validation shared by the HTTP API, the store and the backup importer.
//!
//! Errors are keyed by form field so the React deck can render them directly
//! under the corresponding input.

mod session;
mod settings;
mod target;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use serde_json::Value;

pub use self::session::{SessionPatch, jwt_expiry, validate_session_input};
pub use self::settings::{SettingsPatch, validate_settings};
pub use self::target::{normalize_item_id, validate_target, validate_targets};

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
