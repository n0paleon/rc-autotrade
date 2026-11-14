use base64::Engine as _;
use serde_json::Value;

use super::{FieldErrors, merge_error, strip_bearer};

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

    if let Some(raw) = source.get("refreshToken")
        && !raw.is_null()
    {
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

    if let Some(raw) = source.get("cookie")
        && !raw.is_null()
    {
        match raw.as_str() {
            None => merge_error(&mut errors, "cookie", "Cookie must be text."),
            Some(text) if text.len() > 8192 => {
                merge_error(&mut errors, "cookie", "Cookie header is too long.")
            }
            Some(text) => patch.cookie = Some(text.trim().to_string()),
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
