//! Numeric constants shared by the backend and the React deck.
//!
//! The UI mirrors the ones it needs in `ui/src/lib/constants.js`; the values
//! here are the source of truth because the server re-validates every request.

pub const APP_NAME: &str = "TURU.RC";
pub const APP_VERSION: &str = "1.0.0";

use crate::model::Mode;

/// Rollercoin reports marketplace prices in micro-RLT (1 RLT = 1_000_000).
pub const PRICE_SCALE: f64 = 1_000_000.0;

/// How many recent orders stay in memory for the live panel; SQLite keeps all.
pub const MAX_HISTORY: usize = 200;

/// Loot-log rows the browser receives by default (snapshot + one page).
pub const LOOT_LOG_PAGE_SIZE: usize = 50;

/// Hard cap for a single `/api/history` page.
pub const LOOT_LOG_MAX_PAGE_SIZE: usize = 200;

/// Upper bound for the persistent per-product profit report.
pub const MAX_REPORT_PRODUCTS: usize = 500;

/// How many log lines are buffered for clients that connect late.
pub const LOG_BUFFER_SIZE: usize = 250;

/// How many 1-minute price points the chart needs (24h).
pub const PRICE_HISTORY_LIMIT: usize = 1440;

/// Raw 1-minute price points are kept this long before the prune job drops them.
pub const PRICE_RETENTION_DAYS: i64 = 14;

/// At most one `market_snapshot` write per item per window.
pub const MARKET_SNAPSHOT_THROTTLE_MS: i64 = 2_000;

/// Current-minute price point is rewritten at most this often.
pub const PRICE_POINT_WRITE_INTERVAL_MS: i64 = 15_000;

/// Market frames are counted in 5-minute buckets. Every chart timeframe
/// (5m, 15m, 30m, 1h, 2h) divides this evenly, so the coarser zoom levels are
/// sums of the same stored rows instead of separate series.
pub const EVENT_BUCKET_MS: i64 = 5 * 60_000;

/// Event history kept in SQLite (and hydrated into memory on boot). One week of
/// chart plus a day of slack so the oldest visible bucket is never truncated.
pub const EVENT_RETENTION_DAYS: i64 = 8;

/// Widest window `/api/events` will serve.
pub const EVENT_HISTORY_DAYS: i64 = 7;

/// Dirty event buckets are written to SQLite in one transaction this often.
/// Counting stays in memory, so a busy feed never pays a write per frame.
pub const EVENT_FLUSH_INTERVAL_MS: u64 = 10_000;

/// How many upstream events we keep as a rolling sample for the feed.
pub const EVENT_SAMPLE_SIZE: usize = 60;

/// Runtime defaults, used when the database has no settings row yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultSettings {
    pub delay_per_order: i64,
    pub max_orders_per_minute: i64,
    pub mode: Mode,
    pub auto_refresh: bool,
}

pub const DEFAULT_SETTINGS: DefaultSettings = DefaultSettings {
    delay_per_order: 200,
    max_orders_per_minute: 60,
    mode: Mode::Live,
    auto_refresh: true,
};

pub struct Limits;

impl Limits {
    pub const TARGET_PRICE_MAX: f64 = 1_000_000.0;
    pub const QTY_MAX: i64 = 10_000;
    pub const STOCK_THRESHOLD_MAX: i64 = 1_000_000;
    pub const DELAY_MIN: i64 = 50;
    pub const DELAY_MAX: i64 = 60_000;
    pub const ORDERS_PER_MINUTE_MIN: i64 = 1;
    pub const ORDERS_PER_MINUTE_MAX: i64 = 600;
    pub const LABEL_MAX: usize = 48;
    pub const CONFIGS_MAX: usize = 100;
    pub const BODY_BYTES: usize = 256 * 1024;
    pub const HISTORY_MAX: usize = MAX_HISTORY;
}

/// Order status codes used by the upstream fill frames.
pub const ORDER_STATUS_FILLED: i64 = 12;
pub const ORDER_STATUS_FAILED: i64 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe_and_match_the_javascript_worker() {
        assert_eq!(DEFAULT_SETTINGS.delay_per_order, 200);
        assert_eq!(DEFAULT_SETTINGS.max_orders_per_minute, 60);
        assert_eq!(DEFAULT_SETTINGS.mode, Mode::Live);
        assert!(DEFAULT_SETTINGS.auto_refresh);
        assert_eq!(PRICE_SCALE, 1_000_000.0);
        assert_eq!(MAX_HISTORY, 200);
    }

    #[test]
    fn mode_capabilities_are_explicit() {
        assert!(!Mode::Off.plans() && !Mode::Off.submits() && !Mode::Off.probes());
        assert!(Mode::Shadow.plans() && !Mode::Shadow.submits() && Mode::Shadow.probes());
        assert!(Mode::Live.plans() && Mode::Live.submits() && Mode::Live.probes());
    }

    /// The one property that matters for money: only live can submit, and an
    /// off row never mirrors into a pair that reads as "armed, not dry".
    #[test]
    fn legacy_flags_never_describe_off_as_live_trading() {
        for mode in [Mode::Off, Mode::Shadow, Mode::Live] {
            let (dry_run, paused) = mode.legacy_flags();
            assert_eq!(
                Mode::from_legacy(dry_run, paused),
                mode,
                "legacy mirror must round-trip {mode:?}"
            );
        }
        let (dry_run, paused) = Mode::Off.legacy_flags();
        assert!(
            dry_run && paused,
            "off must stay visibly stopped to old readers"
        );
    }
}
