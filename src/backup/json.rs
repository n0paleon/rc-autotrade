use serde_json::Value;

const MAX_WARNINGS: usize = 20;

pub(super) fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

pub(super) fn text(value: Option<&Value>, max: usize) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(max)
        .collect()
}

pub(super) fn optional_text(value: Option<&Value>, max: usize) -> Option<String> {
    let text = value.and_then(Value::as_str)?;
    if text.is_empty() {
        None
    } else {
        Some(text.chars().take(max).collect())
    }
}

pub(super) fn positive_time(value: Option<&Value>) -> Option<i64> {
    let value = number(value)?;
    (value.is_finite() && value > 0.0).then_some(value as i64)
}

pub(super) fn time_or(value: Option<&Value>, fallback: i64) -> i64 {
    positive_time(value).unwrap_or(fallback)
}

pub(super) fn whole(value: Option<&Value>, fallback: i64, max: i64) -> i64 {
    match number(value) {
        Some(value) if value.is_finite() => (value.trunc() as i64).clamp(0, max),
        _ => fallback,
    }
}

pub(super) fn signed(value: Option<&Value>, fallback: f64) -> f64 {
    number(value)
        .filter(|value| value.is_finite())
        .unwrap_or(fallback)
}

pub(super) fn non_negative(value: Option<&Value>, fallback: f64) -> f64 {
    signed(value, fallback).max(0.0)
}

pub(super) fn warnings_finish(warnings: &mut Vec<String>) {
    while warnings.len() > MAX_WARNINGS {
        warnings.pop();
    }
}
