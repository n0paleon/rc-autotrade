//! Matching + execution engine.
//!
//! The engine never performs I/O and never holds a lock across an await.
//! `handle_frame` returns a list of [`EngineEvent`]s; the wiring (feed task /
//! HTTP handlers) owns the store lock, broadcasts the events and spawns the
//! actual purchase call. When that call settles it feeds a [`PurchaseOutcome`]
//! back with `on_purchase_result`.
//!
//! That keeps the decision path pure, makes the engine testable with a fake
//! clock, and keeps the "one order in flight per target" invariant explicit.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::logger::Logger;
use crate::model::Target;

mod failure;
mod fill;
mod market;
mod minute;
mod order;
mod persistence;
mod reconcile;
mod result;
mod runtime;
mod trend;
mod types;
mod view;

#[cfg(test)]
mod tests;

pub use crate::clock::{Clock, wall_clock};
pub use types::{EngineEvent, MarketTick, OrderIntent, PriceSample, PurchaseOutcome};

use result::PendingOrder;
use runtime::{BufferedResult, Runtime};

const PENDING_TIMEOUT_MS: i64 = 120_000;
const MAX_UNMATCHED_RESULTS: usize = 128;
const SCAN_LOG_INTERVAL_MS: i64 = 12_000;
const TREND_REFRESH_MS: i64 = 10_000;

/// Logs at most once a minute for one slot, so a persistent write failure on
/// the frame path cannot flood the log.
fn warn_throttled(logger: &Logger, slot: &mut i64, now: i64, scope: &str, message: String) {
    if now.saturating_sub(*slot) >= 60_000 {
        *slot = now;
        logger.warn(scope, message);
    }
}

fn label_for(config: Option<&Target>, item_id: &str) -> String {
    match config {
        Some(config) if !config.label.is_empty() => config.label.clone(),
        _ => {
            let tail: String = item_id
                .chars()
                .rev()
                .take(6)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>()
                .to_uppercase();
            format!("ITEM {tail}")
        }
    }
}

pub struct Engine {
    clock: Clock,
    logger: Arc<Logger>,
    runtime: HashMap<String, Runtime>,
    pending: HashMap<String, PendingOrder>,
    unmatched: VecDeque<(String, BufferedResult)>,
    order_stamps: Vec<i64>,
    last_order_at: i64,
    last_no_token_warn_at: i64,
    sample_error_at: i64,
    /// Throttles the warnings for a failed stats/loot write.
    last_flush_error_at: i64,
    last_history_error_at: i64,
    /// Whether a Rollercoin session is linked. Refreshed from the wiring on
    /// every frame so a follow-up fired from the fill path can honour the same
    /// gate as a frame-driven order.
    has_token: bool,
    /// Set by the wiring from the number of connected control sockets.
    broadcast_market: bool,
}

impl Engine {
    pub fn new(clock: Clock, logger: Arc<Logger>) -> Self {
        Self {
            clock,
            logger,
            runtime: HashMap::new(),
            pending: HashMap::new(),
            unmatched: VecDeque::new(),
            order_stamps: Vec::new(),
            last_order_at: 0,
            last_no_token_warn_at: 0,
            sample_error_at: 0,
            last_flush_error_at: 0,
            last_history_error_at: 0,
            has_token: false,
            broadcast_market: true,
        }
    }

    /// Enables or disables market-tick publication. The wiring turns this off
    /// while no control socket is connected so a headless worker stays cheap.
    pub fn set_broadcast_enabled(&mut self, enabled: bool) {
        self.broadcast_market = enabled;
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn runtime_for(&mut self, item_id: &str, desired_qty: i64) -> &mut Runtime {
        let key = item_id.to_ascii_lowercase();
        self.runtime
            .entry(key)
            .or_insert_with(|| Runtime::new(desired_qty))
    }
}
