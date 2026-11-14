//! Background health sampler.

use std::sync::Arc;

use serde_json::Value;

use super::AppState;
use crate::lock::lock;

/// Cadence of the background health sampler. Broadcasts live in the control
/// socket now; this task only samples and logs health transitions so a
/// struggling worker still leaves a trace with no deck connected.
pub const HEALTH_TICK_MS: u64 = 5_000;

/// Logs health transitions.
pub(super) fn track_health(state: &Arc<AppState>, health_level: &mut String) {
    let system = lock(&state.system).snapshot();
    let level = system
        .pointer("/health/level")
        .and_then(Value::as_str)
        .unwrap_or("ok")
        .to_string();
    if level == *health_level {
        return;
    }
    let reasons = system
        .pointer("/health/reasons")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    match level.as_str() {
        "critical" => state
            .logger
            .warn("system", format!("worker health critical — {reasons}")),
        "warn" => state
            .logger
            .warn("system", format!("worker health degraded — {reasons}")),
        _ if *health_level == "warn" || *health_level == "critical" => {
            state.logger.info("system", "worker health recovered")
        }
        _ => {}
    }
    *health_level = level;
}
