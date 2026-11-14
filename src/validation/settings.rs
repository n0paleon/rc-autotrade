use serde_json::Value;

use crate::constants::Limits;
use crate::model::Mode;

use super::{FieldErrors, merge_error, to_number};

/// Partial settings update; only the fields present in the request are touched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsPatch {
    pub delay_per_order: Option<i64>,
    pub max_orders_per_minute: Option<i64>,
    pub mode: Option<Mode>,
    pub auto_refresh: Option<bool>,
    pub total_price_markup_pct: Option<f64>,
}

impl SettingsPatch {
    /// True when the patch moves the worker out of `Off`.
    pub fn resumes(&self) -> bool {
        matches!(self.mode, Some(Mode::Shadow | Mode::Live))
    }
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

    // `mode` is the real switch. `dryRun` / `paused` remain accepted so an old
    // deck, an old backup, or a script keeps working; they are folded into one
    // mode instead of being stored as two independent flags.
    let mut legacy_dry_run: Option<bool> = None;
    let mut legacy_paused: Option<bool> = None;
    if let Some(raw) = source.get("mode") {
        match raw.as_str().filter(|value| !value.is_empty()) {
            Some(value) => match Mode::parse(value) {
                Some(mode) => patch.mode = Some(mode),
                None => merge_error(
                    &mut errors,
                    "mode",
                    "mode must be one of off, shadow or live.".to_string(),
                ),
            },
            None => merge_error(
                &mut errors,
                "mode",
                "mode must be one of off, shadow or live.".to_string(),
            ),
        }
    }
    for (key, target) in [
        ("dryRun", &mut legacy_dry_run),
        ("paused", &mut legacy_paused),
    ] {
        if let Some(raw) = source.get(key) {
            match raw.as_bool() {
                Some(value) => *target = Some(value),
                None => merge_error(&mut errors, key, format!("{key} must be true or false.")),
            }
        }
    }
    if patch.mode.is_none() {
        patch.mode = match (legacy_paused, legacy_dry_run) {
            (Some(true), _) => Some(Mode::Off),
            (_, Some(true)) => Some(Mode::Shadow),
            (Some(false), Some(false)) => Some(Mode::Live),
            _ => None,
        };
    }

    if let Some(raw) = source.get("autoRefresh") {
        match raw.as_bool() {
            Some(value) => patch.auto_refresh = Some(value),
            None => merge_error(
                &mut errors,
                "autoRefresh",
                "autoRefresh must be true or false.".to_string(),
            ),
        }
    }

    if let Some(raw) = source.get("totalPriceMarkupPct") {
        let scale = 10_f64.powf(Limits::MARKUP_DECIMALS);
        match to_number(raw) {
            Some(value)
                if (Limits::MARKUP_MIN..=Limits::MARKUP_MAX).contains(&value)
                    && (value * scale - (value * scale).round()).abs() < 1e-9 =>
            {
                patch.total_price_markup_pct = Some(value)
            }
            _ => merge_error(
                &mut errors,
                "totalPriceMarkupPct",
                format!(
                    "Total price markup must be between {} and {} % with at most {} decimals.",
                    Limits::MARKUP_MIN,
                    Limits::MARKUP_MAX,
                    Limits::MARKUP_DECIMALS
                ),
            ),
        }
    }

    if errors.is_empty() {
        Ok(patch)
    } else {
        Err(errors)
    }
}
