use serde::{Deserialize, Serialize};

use super::{Mode, default_true};

/// Runtime settings. `mode` is the only trading switch; the old `dryRun` and
/// `paused` keys are still accepted on input, echoed on output, and mirrored
/// into the two legacy columns so a pre-v8 reader still sees sane flags.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub delay_per_order: i64,
    pub max_orders_per_minute: i64,
    pub mode: Mode,
    pub auto_refresh: bool,
    /// Percentage of headroom added to the purchase request's `totalPrice`.
    /// It only changes the price ceiling sent upstream, never the plan, the
    /// quantity or the target-price gate.
    pub total_price_markup_pct: f64,
}

impl Default for Settings {
    fn default() -> Self {
        let defaults = crate::constants::DEFAULT_SETTINGS;
        Self {
            delay_per_order: defaults.delay_per_order,
            max_orders_per_minute: defaults.max_orders_per_minute,
            mode: Mode::default(),
            auto_refresh: defaults.auto_refresh,
            total_price_markup_pct: defaults.total_price_markup_pct,
        }
    }
}

impl Settings {
    /// Legacy columns written alongside `mode`, so a pre-v8 reader still sees a
    /// sane pair of flags.
    pub fn legacy_flags(&self) -> (bool, bool) {
        self.mode.legacy_flags()
    }
}

impl Serialize for Settings {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let (dry_run, paused) = self.legacy_flags();
        let mut map = serializer.serialize_map(Some(7))?;
        map.serialize_entry("delayPerOrder", &self.delay_per_order)?;
        map.serialize_entry("maxOrdersPerMinute", &self.max_orders_per_minute)?;
        map.serialize_entry("mode", &self.mode)?;
        map.serialize_entry("autoRefresh", &self.auto_refresh)?;
        map.serialize_entry("totalPriceMarkupPct", &self.total_price_markup_pct)?;
        // Read-only mirrors: they keep an older deck or backup reader working
        // and are ignored whenever `mode` is present.
        map.serialize_entry("dryRun", &dry_run)?;
        map.serialize_entry("paused", &paused)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for Settings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            delay_per_order: i64,
            max_orders_per_minute: i64,
            #[serde(default)]
            mode: Option<Mode>,
            #[serde(default)]
            dry_run: Option<bool>,
            #[serde(default)]
            paused: Option<bool>,
            #[serde(default = "default_true")]
            auto_refresh: bool,
            /// Added after schema v8; an older document simply means "no
            /// headroom", which is the pre-feature behaviour.
            #[serde(default)]
            total_price_markup_pct: f64,
        }

        let wire = Wire::deserialize(deserializer)?;
        // `mode` always wins. The legacy pair is only read when it is absent,
        // and it never infers Live from half a pair: an old document that did
        // not say "dry run is off" must not quietly arm real trading.
        let mode = match (wire.paused, wire.dry_run) {
            (Some(true), _) => Mode::Off,
            (_, Some(true)) => Mode::Shadow,
            (Some(false), Some(false)) => Mode::Live,
            (Some(false), None) | (None, Some(false)) | (None, None) => Mode::Shadow,
        };
        Ok(Settings {
            delay_per_order: wire.delay_per_order,
            max_orders_per_minute: wire.max_orders_per_minute,
            mode: wire.mode.unwrap_or(mode),
            auto_refresh: wire.auto_refresh,
            total_price_markup_pct: wire.total_price_markup_pct,
        })
    }
}
