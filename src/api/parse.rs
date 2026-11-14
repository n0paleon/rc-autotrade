use serde_json::Value;

use super::BODY_PREVIEW_CHARS;

pub(super) fn trim_body(text: &str) -> String {
    let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() > BODY_PREVIEW_CHARS {
        let mut out: String = clean.chars().take(BODY_PREVIEW_CHARS).collect();
        out.push('…');
        out
    } else {
        clean
    }
}

pub(super) fn html_hint(text: &str) -> bool {
    text.trim_start().starts_with('<')
}

pub(super) async fn read_json(response: reqwest::Response) -> (Option<Value>, String) {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let parsed = if text.is_empty() {
        None
    } else {
        serde_json::from_str::<Value>(&text).ok()
    };
    let _ = status;
    (parsed, text)
}

pub(super) fn message_from(body: Option<&Value>, text: &str, fallback: &str) -> String {
    if let Some(body) = body {
        if let Some(message) = body.get("message").and_then(Value::as_str)
            && !message.trim().is_empty()
        {
            return message.to_string();
        }
        if let Some(error) = body.get("error").and_then(Value::as_str)
            && !error.trim().is_empty()
        {
            return error.to_string();
        }
    }
    if html_hint(text) {
        return fallback.to_string();
    }
    let trimmed = trim_body(text);
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed
    }
}
