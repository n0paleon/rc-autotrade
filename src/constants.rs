//! Numeric constants shared by the backend and the React deck.
//!
//! The UI mirrors the ones it needs in `ui/src/lib/constants.js`; the values
//! here are the source of truth because the server re-validates every request.

pub const APP_NAME: &str = "TURU.RC";
pub const APP_VERSION: &str = "1.0.0";

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

/// How many upstream events we keep as a rolling sample for the feed.
pub const EVENT_SAMPLE_SIZE: usize = 60;

/// Runtime defaults, used when the database has no settings row yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultSettings {
    pub delay_per_order: i64,
    pub max_orders_per_minute: i64,
    pub dry_run: bool,
    pub auto_refresh: bool,
    pub paused: bool,
}

pub const DEFAULT_SETTINGS: DefaultSettings = DefaultSettings {
    delay_per_order: 200,
    max_orders_per_minute: 60,
    dry_run: false,
    auto_refresh: true,
    paused: false,
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
    fn defaults_match_the_javascript_worker() {
        assert_eq!(DEFAULT_SETTINGS.delay_per_order, 200);
        assert_eq!(DEFAULT_SETTINGS.max_orders_per_minute, 60);
        assert!(!DEFAULT_SETTINGS.dry_run);
        assert!(DEFAULT_SETTINGS.auto_refresh);
        assert!(!DEFAULT_SETTINGS.paused);
        assert_eq!(PRICE_SCALE, 1_000_000.0);
        assert_eq!(MAX_HISTORY, 200);
    }
}
