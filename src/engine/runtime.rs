use std::sync::Arc;

use serde_json::Value;
use serde_json::value::RawValue;

use crate::depth::Fill;
use crate::trend::{Point, TrendSignal};

use super::minute::MinuteAgg;
use super::reconcile::{RECONCILE_WINDOW_MS, Reconcile, price_key};

#[derive(Debug, Clone)]
pub(super) struct Runtime {
    pub(super) last_price: Option<f64>,
    pub(super) last_stock: Option<i64>,
    pub(super) last_seen_at: Option<i64>,
    /// Newest live frame seen in this process. Hydrated rows never set it, so
    /// the sampler cannot paper over the downtime that preceded a restart.
    pub(super) live_seen_at: Option<i64>,
    pub(super) seen_at: Option<i64>,
    pub(super) stale: bool,
    pub(super) item_type: Option<String>,
    pub(super) allowed_price_min: Option<f64>,
    pub(super) allowed_price_max: Option<f64>,
    pub(super) allowed_at: Option<i64>,
    pub(super) history: Vec<Point>,
    /// Robust close for the minute currently being built.
    pub(super) minute: Option<MinuteAgg>,
    pub(super) fill_history: Vec<Point>,
    pub(super) fill_qty_for: i64,
    pub(super) last_fill: Option<Fill>,
    pub(super) trend: Option<TrendSignal>,
    /// Serialised [`TrendSignal`], refreshed with the model (every
    /// `TREND_REFRESH_MS`). Reusing it keeps the flattened trend out of the
    /// per-frame serialisation path.
    pub(super) trend_json: Option<Arc<RawValue>>,
    pub(super) trend_at: i64,
    pub(super) trend_target: f64,
    pub(super) persisted_at: i64,
    pub(super) point_write_at: i64,
    pub(super) last_point_bucket: Option<i64>,
    pub(super) persist_error_at: i64,
    pub(super) hydrated_at: Option<i64>,
    pub(super) inflight: bool,
    /// Set while a booked fill has not yet been confirmed by a fresher frame.
    /// See [`Reconcile`]. Never persisted: it is a within-process view.
    pub(super) reconcile: Option<Reconcile>,
    pub(super) orders: i64,
    pub(super) fills: i64,
    pub(super) failures: i64,
    pub(super) last_result: Option<Value>,
    pub(super) failure_streak: i64,
    pub(super) blocked_until: i64,
    pub(super) last_log_at: i64,
    pub(super) last_order_at: i64,
    pub(super) last_in_range_at: i64,
}

impl Runtime {
    pub(super) fn new(desired_qty: i64) -> Self {
        Self {
            last_price: None,
            last_stock: None,
            last_seen_at: None,
            live_seen_at: None,
            seen_at: None,
            stale: false,
            item_type: None,
            allowed_price_min: None,
            allowed_price_max: None,
            allowed_at: None,
            history: Vec::new(),
            minute: None,
            fill_history: Vec::new(),
            fill_qty_for: desired_qty,
            last_fill: None,
            trend: None,
            trend_json: None,
            trend_at: 0,
            trend_target: 0.0,
            persisted_at: 0,
            point_write_at: 0,
            last_point_bucket: None,
            persist_error_at: 0,
            hydrated_at: None,
            inflight: false,
            reconcile: None,
            orders: 0,
            fills: 0,
            failures: 0,
            last_result: None,
            failure_streak: 0,
            blocked_until: 0,
            last_log_at: 0,
            last_order_at: 0,
            last_in_range_at: 0,
        }
    }

    /// Stock the decision path is allowed to believe for this frame.
    ///
    /// With no open window this is simply the frame's own depth. While an order
    /// is outstanding or a fill is being reconciled, a frame may not raise the
    /// book above what we know we still have, because it was most likely
    /// generated before upstream applied our order. Four proofs close the
    /// window early and hand authority straight back to the frame:
    ///
    /// * its best price moved — a different level is new information;
    /// * it shows a *fuller* best level than the one we bought from, which a
    ///   pre-purchase snapshot cannot contain;
    /// * it shows at most our floor — it cannot be a pre-fill snapshot of a
    ///   deeper book;
    /// * the whole-book `totalCount` dropped by at least what we bought.
    pub(super) fn effective_stock(
        &mut self,
        now: i64,
        raw: i64,
        price: f64,
        frame_total: Option<i64>,
    ) -> i64 {
        let Some(reconcile) = self.reconcile else {
            return raw;
        };
        if now >= reconcile.until {
            self.reconcile = None;
            return raw;
        }
        if price_key(price) != reconcile.price_key
            || raw > reconcile.trigger_stock
            || raw <= reconcile.floor
        {
            self.reconcile = None;
            return raw;
        }
        // The total proof needs a booked fill to compare against: before any
        // result lands `bought` is 0, and "dropped by at least zero" would
        // close the window on the very first frame instead of proving anything.
        if reconcile.bought > 0
            && let Some(frame_total) = frame_total
            && let Some(at_trigger) = reconcile.total_at_trigger
            && frame_total <= at_trigger.saturating_sub(reconcile.bought)
        {
            self.reconcile = None;
            return raw;
        }
        reconcile.floor
    }

    /// Books one order result against the local ledger of the level it bought.
    ///
    /// The whole request was reserved when the order was submitted, so a fill
    /// only has to (a) return the units upstream did not take and (b) record
    /// the units it did take. Nothing is recomputed from the trigger frame: any
    /// order the engine placed in the meantime already spent its own share out
    /// of the floor, and recomputing would hand those units to this fill as
    /// well — the double-buy that made an early rearm fire for goods another
    /// order had already committed.
    ///
    /// A result that arrives after the reservation expired does not open a new
    /// window. The feed has been authoritative for longer than the window, so
    /// rebuilding a floor from the old trigger frame could only resurrect stock
    /// that is already gone. Such a fill is booked and leaves the next frame in
    /// charge. Returns the free units left on the level, when a ledger exists.
    pub(super) fn settle_fill(
        &mut self,
        now: i64,
        sent: i64,
        got: i64,
        total_at_trigger: Option<i64>,
        price: f64,
    ) -> Option<i64> {
        let key = price_key(price);
        match self.reconcile {
            None => return None,
            Some(window) if !window.live(now) => {
                // A result that arrives after the reservation expired is booked
                // but never reopens a window: the feed has been authoritative
                // for longer than the window, so rebuilding a floor from the
                // old trigger frame could only resurrect stock that is gone.
                self.reconcile = None;
                return None;
            }
            // A fill at another level belongs to a price the ledger no longer
            // tracks; the best level's own reservation stays untouched.
            Some(window) if window.price_key != key => return None,
            Some(_) => {}
        }
        let window = self.reconcile.as_mut().expect("matched above");
        // `saturating_sub` clamps at `i64::MIN`, not at zero, so an over-fill
        // has to be floored explicitly: it never owes the book units.
        window.release(sent.saturating_sub(got).max(0));
        window.bought = window.bought.saturating_add(got).min(window.trigger_stock);
        // A fill is fresh evidence that upstream applied the order, so the
        // distrust window restarts from the result, exactly as before.
        window.until = now + RECONCILE_WINDOW_MS;
        if window.total_at_trigger.is_none() {
            window.total_at_trigger = total_at_trigger;
        }
        Some(window.floor)
    }

    /// Gives the reservation of an order that obtained nothing back to the book.
    ///
    /// Used when the result frame itself says the order was not filled. An
    /// HTTP rejection deliberately does **not** release: the request may still
    /// have reached the matching engine, so until the window closes the units
    /// stay distrusted exactly like the ones a fill consumed. Keeping the
    /// window (instead of dropping it) also preserves a follow-up's own
    /// reservation in the same floor.
    pub(super) fn release_reservation(&mut self, now: i64, qty: i64, price: f64) {
        let key = price_key(price);
        if let Some(window) = self.reconcile.as_mut()
            && window.live(now)
            && window.price_key == key
        {
            window.release(qty);
        }
    }
}

/// A result frame whose purchase had not been accepted yet, kept briefly so it
/// can be replayed once the HTTP response names its order.
#[derive(Debug, Clone)]
pub(super) struct BufferedResult {
    pub(super) event: crate::events::UpstreamEvent,
    pub(super) raw: Value,
    pub(super) received_at: i64,
    pub(super) buffered_at: i64,
}
