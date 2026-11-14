use serde::{Deserialize, Serialize};

use super::{default_item_type, default_true};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub item_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_item_type")]
    pub item_type: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rarity_color: Option<String>,
    pub target_price: f64,
    pub desired_qty: i64,
    pub high_stock_threshold: i64,
    pub min_acceptable_qty: i64,
    #[serde(default = "default_true")]
    pub allow_partial_order: bool,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub updated_at: Option<i64>,
}

impl Target {
    pub fn plan_input(&self) -> crate::rules::PlanInput {
        crate::rules::PlanInput {
            active: self.active,
            target_price: self.target_price,
            desired_qty: self.desired_qty,
            high_stock_threshold: self.high_stock_threshold,
            min_acceptable_qty: self.min_acceptable_qty,
            allow_partial_order: self.allow_partial_order,
        }
    }
}

/// What the worker is allowed to do with a passing signal.
///
/// This replaces the old pair of independent booleans (`paused` + `dryRun`),
/// which could express four states even though one of them — paused with
/// dry-run on — behaved exactly like paused alone.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Stop deciding: no plan is made, no cooldown or rate slot is spent, the
    /// warm HTTP probe stops, and the market feed keeps streaming.
    Off,
    /// Decide and log exactly as live would, but never submit; dry-run orders
    /// still consume cooldown and the per-minute cap so the budget is honest.
    Shadow,
    /// Decide and submit to Rollercoin. Requires a linked session.
    #[default]
    Live,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Shadow => "shadow",
            Mode::Live => "live",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Mode::Off),
            "shadow" => Some(Mode::Shadow),
            "live" => Some(Mode::Live),
            _ => None,
        }
    }

    /// Off makes `plan_order` short-circuit; shadow and live plan normally.
    pub fn plans(self) -> bool {
        !matches!(self, Mode::Off)
    }

    /// Only live mode talks to the purchase endpoint.
    pub fn submits(self) -> bool {
        matches!(self, Mode::Live)
    }

    /// Keeps the upstream edge warm. Off stops probing on purpose, shadow keeps
    /// probing so a dry-run session still exercises the same network path.
    pub fn probes(self) -> bool {
        !matches!(self, Mode::Off)
    }

    /// Legacy mirror for the `settings` row. Off deliberately writes
    /// `dry_run = 1` so an older reader that unpauses lands in dry-run, never
    /// straight into live trading.
    pub fn legacy_flags(self) -> (bool, bool) {
        match self {
            Mode::Off => (true, true),
            Mode::Shadow => (true, false),
            Mode::Live => (false, false),
        }
    }

    /// Best-effort recovery for rows written before schema v8, or for an
    /// unrecognised `mode` string.
    pub fn from_legacy(dry_run: bool, paused: bool) -> Self {
        if paused {
            Mode::Off
        } else if dry_run {
            Mode::Shadow
        } else {
            Mode::Live
        }
    }
}
