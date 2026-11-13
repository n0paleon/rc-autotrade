//! Matching + execution engine, ported from `src/worker/engine.js`.
//!
//! Design difference from the JavaScript: the engine never performs I/O and
//! never holds a lock across an await. `handle_frame` returns a list of
//! [`EngineEvent`]s; the wiring (feed task / HTTP handlers) owns the store lock,
//! broadcasts the events and spawns the actual purchase call. When that call
//! settles it feeds a [`PurchaseOutcome`] back with `on_purchase_result`.
//!
//! That keeps the decision path pure, makes the engine testable with a fake
//! clock, and keeps the "one order in flight per target" invariant explicit.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};

use crate::constants::{
    MARKET_SNAPSHOT_THROTTLE_MS, ORDER_STATUS_FAILED, ORDER_STATUS_FILLED, PRICE_BUCKET_MS,
    PRICE_HISTORY_LIMIT, PRICE_POINT_WRITE_INTERVAL_MS, PRICE_SCALE, PRICE_SETTLE_DWELL_MS,
};
use crate::depth::{Fill, estimate_fill_price_sorted};
use crate::events::{Kind, UpstreamEvent, normalize_upstream_event};
use crate::logger::Logger;
use crate::model::{MarketSnapshot, Mode, OrderEntry, PriceBand, Target};
use crate::rules::{Action, Plan, plan_order, round_price};
use crate::store::{Store, now_ms};
use crate::timings::Timings;
use crate::trend::{FillRef, Point, TrendOptions, TrendSignal, build_trend};

const PENDING_TIMEOUT_MS: i64 = 120_000;
const MAX_UNMATCHED_RESULTS: usize = 128;
const SCAN_LOG_INTERVAL_MS: i64 = 12_000;
const TREND_REFRESH_MS: i64 = 10_000;

/// How long the engine distrusts the stock in a market frame after it has
/// already booked a fill.
///
/// Upstream frames are unversioned (no `updatedAt`, no sequence number), so a
/// frame that was generated just before our purchase was applied can arrive
/// just after it — and it still shows the pre-purchase depth. Trusting it would
/// resurrect stock we have already bought and could trigger a second order for
/// goods that are gone. For this window the decision stock is therefore floored
/// at what we know locally; afterwards the frame wins again, so a genuine
/// restock is only ever missed for at most this long.
const RECONCILE_WINDOW_MS: i64 = 1_500;

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Everything the engine wants the outside world to do, in order.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    Broadcast {
        kind: &'static str,
        data: Value,
    },
    /// A market tick that has not been turned into a `Value` yet. The control
    /// plane serialises it straight into the socket envelope, which avoids
    /// building (and then walking) a JSON tree per frame.
    Market(Box<MarketTick>),
    Submit(Box<OrderIntent>),
}

/// One market tick as the control socket receives it.
///
/// Built as a typed struct rather than a `json!` literal because the trend is
/// pre-serialised once per model rebuild ([`RawValue`] is copied verbatim).
/// Serialising the flattened 34-field `TrendSignal` on every frame measured at
/// ~59% of the frame budget with nothing in return: the deck cannot show more
/// than a few ticks per second anyway.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketTick {
    pub item_id: String,
    pub label: String,
    /// Live best ask, kept raw for the price readout and order decisions.
    pub price: f64,
    /// Duration-weighted median of the minute so far. The deck plots this one
    /// so a transient bait price never bends the live line.
    pub close: f64,
    /// Lowest and highest price that held during this minute.
    pub low: f64,
    pub high: f64,
    /// Lowest and highest print seen this minute, dwell guard included.
    pub raw_low: f64,
    pub raw_high: f64,
    /// Book depth exactly as the upstream frame reported it. Kept honest even
    /// while a fill is being reconciled, so the deck shows what the market
    /// actually said.
    pub stock: i64,
    /// Stock the decision path used. It differs from [`Self::stock`] only while
    /// `reconciling` is true, when a frame that predates our fill must not be
    /// allowed to raise the book.
    pub effective_stock: i64,
    /// True while a booked fill has not been confirmed by a fresher frame.
    pub reconciling: bool,
    pub target_price: f64,
    pub desired_qty: i64,
    pub allowed_price_min: Option<f64>,
    pub allowed_price_max: Option<f64>,
    pub allowed_at: Option<i64>,
    pub trend: Option<Arc<RawValue>>,
    pub fill: Option<Fill>,
    pub currency: String,
    pub item_type: String,
    pub bucket: i64,
    pub hot: bool,
    pub mode: Mode,
    pub at: i64,
}

#[derive(Debug, Clone)]
pub struct OrderIntent {
    pub item_id: String,
    pub item_type: String,
    pub label: String,
    pub qty: i64,
    /// Stock available when the signal triggered (the market frame that
    /// produced this intent). Carried into the loot row as
    /// `OrderEntry::stock_at_trigger` on both the filled and failed paths.
    pub stock_at_trigger: Option<i64>,
    /// Whole-book depth (`totalCount`) at trigger time. Used as the early-close
    /// proof for the reconciliation window: a later frame whose total dropped
    /// by at least what we bought has clearly applied our fill.
    pub total_count_at_trigger: Option<i64>,
    pub unit_price_raw: f64,
    pub unit_price: f64,
    pub target_price: f64,
    pub notional: f64,
    pub doubled: bool,
    /// Set on a follow-up order the engine fired locally from a partial fill,
    /// without waiting for the next market frame. Such an order is a guess about
    /// the remainder: a rejection must not trip the failure backoff, because
    /// being wrong there is expected and would otherwise freeze the target for
    /// far longer than the frame wait it replaced.
    pub optimistic: bool,
    pub at: i64,
    pub timings: Timings,
    pub raw_price_event: Value,
}

/// Result of the purchase call, fed back by the wiring.
#[derive(Debug, Clone, Default)]
pub struct PurchaseOutcome {
    pub ok: bool,
    pub order_id: Option<String>,
    pub error: Option<String>,
    /// Epoch ms when the HTTP request actually left the client. Falls back to
    /// the provisional engine mark when the caller has no HTTP sample (mock).
    pub send_at: Option<i64>,
    pub response_at: Option<i64>,
    pub complete_at: Option<i64>,
    /// Monotonic dispatch → first response byte (TTFB) for the attempt that
    /// responded.
    pub ttfb_ms: Option<f64>,
    pub accepted_at: Option<i64>,
    /// Exact JSON body sent to the purchase endpoint, kept for the loot log.
    pub request_payload: Option<Value>,
}

#[derive(Debug, Clone)]
struct PendingOrder {
    item_id: String,
    item_type: String,
    label: String,
    qty: i64,
    stock_at_trigger: Option<i64>,
    total_count_at_trigger: Option<i64>,
    optimistic: bool,
    unit_price: f64,
    target_price: f64,
    at: i64,
    timings: Timings,
    raw_price_event: Value,
    request_payload: Option<Value>,
}

/// Locally committed book state for one target while an order is outstanding or
/// its fill is being reconciled against the live feed.
///
/// The engine cannot tell a stale frame (generated before upstream applied our
/// order) from a genuine restock: the frames carry no version at all. So it
/// keeps a floor for a short window instead of guessing: frames may not raise
/// the decision stock past [`Reconcile::floor`] until the window closes or one
/// of the proofs below shows the frame has caught up with us.
///
/// The ledger opens when the order is **submitted**, not when its fill lands,
/// so the units a request commits are never offered to a second order in the
/// gap between the HTTP response and the result frame. Scaling the ledger by
/// `floor` rather than by `stockAtTrigger - got` is what keeps the early rearm
/// honest: every path that spends from the book goes through the same floor.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct Reconcile {
    /// Units the engine believes it can still buy at this level. Opening the
    /// window reserves the whole request out of it, a fill returns the part
    /// upstream did not take, and a result that obtained nothing returns the
    /// whole request, so a stale frame or a follow-up can never be handed the
    /// same units twice.
    floor: i64,
    /// Wall-clock deadline. Past it the next frame is assumed fresh.
    until: i64,
    /// Whole-book depth when the signal fired (`totalCount`). Lets a frame prove
    /// freshness by its total dropping by at least [`Reconcile::bought`].
    total_at_trigger: Option<i64>,
    /// Units booked fills have actually taken from the level.
    bought: i64,
    /// Best-level depth when the signal fired. A frame showing *more* than this
    /// has new listings the pre-purchase snapshot cannot contain.
    trigger_stock: i64,
    /// Best-level price when the signal fired, as a micro-RLT key. A frame
    /// whose best price moved is a different level and carries new information.
    price_key: i64,
}

impl Reconcile {
    /// Opens a ledger for the level a frame just advertised.
    fn open(now: i64, stock: i64, price: f64, total: Option<i64>) -> Self {
        Self {
            floor: stock,
            until: now + RECONCILE_WINDOW_MS,
            total_at_trigger: total,
            bought: 0,
            trigger_stock: stock,
            price_key: price_key(price),
        }
    }

    /// True while the frame path must still defer to the local ledger.
    fn live(&self, now: i64) -> bool {
        now < self.until
    }

    /// Removes the units a just-submitted order committed.
    fn reserve(&mut self, qty: i64) {
        self.floor = self.floor.saturating_sub(qty).max(0);
    }

    /// Returns units an order did not actually take to the book.
    fn release(&mut self, qty: i64) {
        self.floor = self
            .floor
            .saturating_add(qty.max(0))
            .min(self.trigger_stock);
    }
}

#[derive(Debug, Clone)]
struct BufferedResult {
    event: UpstreamEvent,
    raw: Value,
    received_at: i64,
    buffered_at: i64,
}

/// Micro-RLT key used to group equal prices without comparing floats.
fn price_key(price: f64) -> i64 {
    (price * PRICE_SCALE).round() as i64
}

/// One minute's robust close.
///
/// The stored series used to take the last write of a minute as its close, so a
/// single transient print — a one-unit bait listing, a cancelled ask — redrew
/// the whole chart and then propagated into every coarser bucket. This
/// aggregator instead weighs each price by how long it actually held and
/// reports the duration-weighted median: a price seen for less than half the
/// minute cannot move the close, while a real move wins as soon as it holds.
///
/// A [`PRICE_SETTLE_DWELL_MS`] debounce runs first, so a print that vanishes
/// within a few seconds never enters the weights at all. The guard only shapes
/// the chart and the sampler; order decisions still read the raw best ask.
#[derive(Debug, Clone)]
struct MinuteAgg {
    bucket: i64,
    /// Price that currently rules the minute.
    settled: f64,
    /// Instant up to which `settled` has been accrued into `weights`.
    accrued_to: i64,
    /// A different price waiting for the dwell guard to accept it.
    candidate: Option<(f64, i64)>,
    /// Micro-RLT price -> milliseconds that price held.
    weights: BTreeMap<i64, i64>,
    /// Lowest and highest price that actually held this minute. Transient
    /// prints rejected by the dwell guard are excluded on purpose.
    low: f64,
    high: f64,
    /// Lowest and highest print seen this minute, before the dwell guard. This
    /// is the raw band: it does include one-frame bait, so the chart can show
    /// it only when the viewer asks for it.
    raw_low: f64,
    raw_high: f64,
}

impl MinuteAgg {
    fn start(bucket: i64, price: f64, at: i64) -> Self {
        Self {
            bucket,
            settled: price,
            accrued_to: at.clamp(bucket, bucket + PRICE_BUCKET_MS),
            candidate: None,
            weights: BTreeMap::new(),
            low: price,
            high: price,
            raw_low: price,
            raw_high: price,
        }
    }

    /// Records the price seen at `at` and returns the running minute close.
    fn observe(&mut self, at: i64, price: f64) -> f64 {
        let at = at.clamp(self.bucket, self.bucket + PRICE_BUCKET_MS);
        self.accrue(at);
        // The raw band sees every print, including a candidate that may never
        // pass the dwell guard.
        self.raw_low = self.raw_low.min(price);
        self.raw_high = self.raw_high.max(price);
        if price_key(price) != price_key(self.settled) {
            match self.candidate {
                Some((candidate, since)) if price_key(candidate) == price_key(price) => {
                    if at - since >= PRICE_SETTLE_DWELL_MS {
                        self.settled = price;
                        self.candidate = None;
                        self.low = self.low.min(price);
                        self.high = self.high.max(price);
                    }
                }
                _ => self.candidate = Some((price, at)),
            }
        } else {
            self.candidate = None;
        }
        self.median()
    }

    /// Accrues the settled price up to `until`, but never past the bucket end.
    fn accrue(&mut self, until: i64) {
        let until = until.clamp(self.bucket, self.bucket + PRICE_BUCKET_MS);
        if until > self.accrued_to {
            *self.weights.entry(price_key(self.settled)).or_insert(0) += until - self.accrued_to;
            self.accrued_to = until;
        }
    }

    /// Cheapest price whose duration reaches half the bucket. On an exact tie
    /// the lower price wins, so a close is never biased upward by rounding.
    fn median(&self) -> f64 {
        let total: i64 = self.weights.values().sum();
        if total <= 0 {
            return self.settled;
        }
        let mut cumulative = 0i64;
        for (key, duration) in &self.weights {
            cumulative += *duration;
            // Reaching half is enough, so an exact tie resolves to the cheaper
            // price instead of rounding the close upward.
            if cumulative * 2 >= total {
                return *key as f64 / PRICE_SCALE;
            }
        }
        self.settled
    }

    /// Final close for the bucket, including the tail after the last frame.
    fn close(&mut self) -> f64 {
        self.accrue(self.bucket + PRICE_BUCKET_MS);
        self.median()
    }

    /// Running close without accepting a new price, for the quiet-minute
    /// sampler: it carries the settled level instead of inventing a tick.
    fn value(&mut self, at: i64) -> f64 {
        self.accrue(at);
        self.median()
    }
}

#[derive(Debug, Clone)]
struct Runtime {
    last_price: Option<f64>,
    last_stock: Option<i64>,
    last_seen_at: Option<i64>,
    /// Newest live frame seen in this process. Hydrated rows never set it, so
    /// the sampler cannot paper over the downtime that preceded a restart.
    live_seen_at: Option<i64>,
    seen_at: Option<i64>,
    stale: bool,
    item_type: Option<String>,
    allowed_price_min: Option<f64>,
    allowed_price_max: Option<f64>,
    allowed_at: Option<i64>,
    history: Vec<Point>,
    /// Robust close for the minute currently being built.
    minute: Option<MinuteAgg>,
    fill_history: Vec<Point>,
    fill_qty_for: i64,
    last_fill: Option<Fill>,
    trend: Option<TrendSignal>,
    /// Serialised [`TrendSignal`], refreshed with the model (every
    /// `TREND_REFRESH_MS`). Reusing it keeps the flattened trend out of the
    /// per-frame serialisation path.
    trend_json: Option<Arc<RawValue>>,
    trend_at: i64,
    trend_target: f64,
    persisted_at: i64,
    point_write_at: i64,
    last_point_bucket: Option<i64>,
    persist_error_at: i64,
    hydrated_at: Option<i64>,
    inflight: bool,
    /// Set while a booked fill has not yet been confirmed by a fresher frame.
    /// See [`Reconcile`]. Never persisted: it is a within-process view.
    reconcile: Option<Reconcile>,
    orders: i64,
    fills: i64,
    failures: i64,
    last_result: Option<Value>,
    failure_streak: i64,
    blocked_until: i64,
    last_log_at: i64,
    last_order_at: i64,
    last_in_range_at: i64,
}

impl Runtime {
    fn new(desired_qty: i64) -> Self {
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
    fn effective_stock(&mut self, now: i64, raw: i64, price: f64, frame_total: Option<i64>) -> i64 {
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
    fn settle_fill(
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
    fn release_reservation(&mut self, now: i64, qty: i64, price: f64) {
        let key = price_key(price);
        if let Some(window) = self.reconcile.as_mut()
            && window.live(now)
            && window.price_key == key
        {
            window.release(qty);
        }
    }
}

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

/// One sampled close, handed back to the wiring so the deck can append it to
/// the live history without waiting for the item's next real frame.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceSample {
    pub item_id: String,
    pub bucket: i64,
    pub price: f64,
    pub low: f64,
    pub high: f64,
    pub raw_low: f64,
    pub raw_high: f64,
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

    // ── frame entry point ───────────────────────────────────────────────────

    pub fn handle_frame(
        &mut self,
        store: &mut Store,
        raw: &Value,
        received_at: i64,
        has_token: bool,
    ) -> Vec<EngineEvent> {
        self.has_token = has_token;
        let event = normalize_upstream_event(raw);
        match event.kind {
            Kind::Market => {
                let Some(item_id) = event.item_id.clone() else {
                    return Vec::new();
                };
                // The item monitor ranks every id the RLT market shows, not
                // just the ones on the target fleet, so the frame is counted
                // before the target lookup below can return early.
                if event
                    .currency
                    .as_deref()
                    .is_none_or(|currency| currency == "RLT")
                {
                    store.note_item_event(&item_id, event.item_type.as_deref(), received_at);
                }
                self.handle_market(store, &event, &item_id, raw, received_at)
            }
            Kind::Fill => self.handle_fill(store, &event, raw, received_at),
            Kind::Other => Vec::new(),
        }
    }

    fn handle_market(
        &mut self,
        store: &mut Store,
        event: &UpstreamEvent,
        item_id: &str,
        raw: &Value,
        received_at: i64,
    ) -> Vec<EngineEvent> {
        let Some(config) = store.find_config(item_id).cloned() else {
            return Vec::new();
        };
        if let Some(currency) = &event.currency {
            if currency != "RLT" {
                return Vec::new();
            }
        }

        if let Some(item_type) = &event.item_type {
            if config.item_type != *item_type {
                store.update_item_type(&config.item_id, item_type);
            }
        }

        let Some(best) = event.listings.first().copied() else {
            return Vec::new();
        };

        let now = self.now();
        let signal_at = received_at;
        let handle_at = now;
        let desired_qty = config.desired_qty;
        let label = label_for(Some(&config), &config.item_id);
        let currency = event.currency.as_deref().unwrap_or("RLT");
        let item_type = event
            .item_type
            .clone()
            .unwrap_or_else(|| config.item_type.clone());

        let allowed_min = event.allowed_price_min;
        let allowed_max = event.allowed_price_max;
        let bucket = now.div_euclid(60_000) * 60_000;
        let listings: Vec<crate::depth::Listing> = event
            .listings
            .iter()
            .map(|listing| crate::depth::Listing {
                price: listing.unit_price,
                quantity: listing.quantity,
            })
            .collect();
        let rest = event.rest.map(|rest| crate::depth::Listing {
            price: rest.unit_price,
            quantity: rest.quantity,
        });
        // The normaliser already sorted the ladder cheapest-first, so the depth
        // model can consume it directly instead of copying and sorting again.
        let fill = estimate_fill_price_sorted(&listings, rest, desired_qty);

        let mut events = Vec::new();
        let (
            trend_json,
            allowed_min,
            allowed_max,
            allowed_at,
            close,
            low,
            high,
            raw_low,
            raw_high,
            finalized,
        ) = {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            runtime.last_price = Some(best.unit_price);
            // Raw upstream depth. Kept untouched by fills on purpose: this field
            // feeds the snapshot and the trend's book size, and both must show
            // what the market reported, not a local prediction.
            runtime.last_stock = Some(best.quantity);
            runtime.last_seen_at = Some(now);
            runtime.live_seen_at = Some(now);
            runtime.seen_at = Some(now);
            runtime.stale = false;
            runtime.item_type = Some(item_type.clone());
            if allowed_min.is_some() {
                runtime.allowed_price_min = allowed_min;
            }
            if allowed_max.is_some() {
                runtime.allowed_price_max = allowed_max;
            }
            if allowed_min.is_some() || allowed_max.is_some() {
                runtime.allowed_at = Some(now);
            }

            // The stored close is the duration-weighted median of this minute,
            // not the last frame. Rolling into a new minute finalises the one
            // that just ended so its final write is exact.
            let (close, low, high, raw_low, raw_high, finalized) = match runtime.minute.take() {
                Some(mut agg) if agg.bucket == bucket => {
                    let close = agg.observe(now, best.unit_price);
                    let (low, high) = (agg.low, agg.high);
                    let (raw_low, raw_high) = (agg.raw_low, agg.raw_high);
                    runtime.minute = Some(agg);
                    (close, low, high, raw_low, raw_high, None)
                }
                Some(mut agg) if agg.bucket < bucket => {
                    let old_bucket = agg.bucket;
                    let old_close = agg.close();
                    let finalized = (
                        old_bucket,
                        old_close,
                        agg.low,
                        agg.high,
                        agg.raw_low,
                        agg.raw_high,
                    );
                    // The settled level held from the bucket boundary until
                    // this frame, so the new minute starts there.
                    let mut next = MinuteAgg::start(bucket, agg.settled, bucket);
                    let close = next.observe(now, best.unit_price);
                    let (low, high) = (next.low, next.high);
                    let (raw_low, raw_high) = (next.raw_low, next.raw_high);
                    runtime.minute = Some(next);
                    (close, low, high, raw_low, raw_high, Some(finalized))
                }
                _ => {
                    let mut agg = MinuteAgg::start(bucket, best.unit_price, now);
                    let close = agg.observe(now, best.unit_price);
                    let (low, high) = (agg.low, agg.high);
                    let (raw_low, raw_high) = (agg.raw_low, agg.raw_high);
                    runtime.minute = Some(agg);
                    (close, low, high, raw_low, raw_high, None)
                }
            };
            if let Some((old_bucket, old_close, _, _, _, _)) = finalized
                && let Some(point) = runtime
                    .history
                    .iter_mut()
                    .find(|point| point.t == old_bucket)
            {
                point.p = old_close;
            }
            match runtime.history.last_mut() {
                Some(point) if point.t == bucket => point.p = close,
                _ => {
                    runtime.history.push(Point {
                        t: bucket,
                        p: close,
                    });
                    if runtime.history.len() > PRICE_HISTORY_LIMIT {
                        let excess = runtime.history.len() - PRICE_HISTORY_LIMIT;
                        runtime.history.drain(0..excess);
                    }
                }
            }

            if runtime.fill_qty_for != desired_qty {
                runtime.fill_history.clear();
                runtime.fill_qty_for = desired_qty;
                runtime.last_fill = None;
            }
            runtime.last_fill = fill;
            if let Some(fill) = fill {
                let last = runtime.fill_history.last().copied();
                match last {
                    Some(point) if point.t >= bucket => {
                        if let Some(point) = runtime.fill_history.last_mut() {
                            point.p = fill.price;
                        }
                    }
                    _ => {
                        runtime.fill_history.push(Point {
                            t: bucket,
                            p: fill.price,
                        });
                        if runtime.fill_history.len() > PRICE_HISTORY_LIMIT {
                            let excess = runtime.fill_history.len() - PRICE_HISTORY_LIMIT;
                            runtime.fill_history.drain(0..excess);
                        }
                    }
                }
            }

            // Rebuilds the model only when it has expired; the serialised form
            // is cached next to it, so the frame path only clones a string.
            let _ = Self::compute_trend(&config, runtime, now, false);
            (
                runtime.trend_json.clone(),
                runtime.allowed_price_min,
                runtime.allowed_price_max,
                runtime.allowed_at,
                close,
                low,
                high,
                raw_low,
                raw_high,
                finalized,
            )
        };

        self.persist_market(
            store,
            &config,
            now,
            close,
            low,
            high,
            raw_low,
            raw_high,
            fill,
            bucket,
            event.allowed_price_min.is_some() || event.allowed_price_max.is_some(),
            finalized,
        );

        // The frame is the only source of truth for the book — except while a
        // fill is being reconciled, when a frame generated before our purchase
        // was applied must not be allowed to resurrect the stock we already
        // bought. The raw depth still goes to the deck and to the snapshot.
        let (effective_stock, reconciling) = {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            let effective =
                runtime.effective_stock(now, best.quantity, best.unit_price, event.total_count);
            (effective, runtime.reconcile.is_some())
        };

        let settings = store.state.settings;
        let blocked = !settings.mode.plans();
        let plan = plan_order(
            config.plan_input(),
            best.unit_price,
            effective_stock as f64,
            blocked,
            Some(config.target_price),
        );
        let hot = plan.action == Action::Buy;
        // While off, report the plan the engine *would* have made. The gate
        // above still short-circuits before any budget is spent, but the log
        // line and the `hot` flag stay honest instead of blaming "paused" for
        // a frame that was really too expensive.
        let observed = if blocked {
            plan_order(
                config.plan_input(),
                best.unit_price,
                effective_stock as f64,
                false,
                Some(config.target_price),
            )
        } else {
            plan
        };

        // A headless worker (no control socket connected) must not pay to build
        // a payload nobody reads; the wiring flips this off every frame.
        if self.broadcast_market {
            events.push(EngineEvent::Market(Box::new(MarketTick {
                item_id: config.item_id.clone(),
                label: label.clone(),
                price: best.unit_price,
                close,
                low,
                high,
                raw_low,
                raw_high,
                stock: best.quantity,
                effective_stock,
                reconciling,
                target_price: config.target_price,
                desired_qty,
                allowed_price_min: allowed_min,
                allowed_price_max: allowed_max,
                allowed_at,
                trend: trend_json.clone(),
                fill,
                currency: currency.to_string(),
                item_type: item_type.clone(),
                bucket,
                hot: observed.action == Action::Buy,
                mode: settings.mode,
                at: now,
            })));
        }

        if observed.action == Action::Buy {
            self.runtime_for(&config.item_id, desired_qty)
                .last_in_range_at = now;
        }

        self.log_scan(&config, best.unit_price, effective_stock, &observed, now);

        if !hot {
            return events;
        }
        events.extend(self.place_order(
            store,
            &config,
            &label,
            &item_type,
            plan.qty.unwrap_or(1),
            plan.doubled.unwrap_or(false),
            best.price_raw,
            best.unit_price,
            Some(best.quantity),
            event.total_count,
            signal_at,
            handle_at,
            &event.cmd,
            raw.clone(),
            false,
        ));
        events
    }

    /// Builds, broadcasts and (in live mode) submits one order.
    ///
    /// Every safety gate lives here instead of in `handle_market`, so the
    /// follow-up fired from the fill path cannot skip one: mode, one order in
    /// flight per target, failure backoff, the inter-order cooldown (opt-out
    /// for a follow-up), a linked session and the per-minute rate budget.
    #[allow(clippy::too_many_arguments)]
    fn place_order(
        &mut self,
        store: &mut Store,
        config: &Target,
        label: &str,
        item_type: &str,
        qty: i64,
        doubled: bool,
        price_raw: f64,
        unit_price: f64,
        stock_at_trigger: Option<i64>,
        total_count_at_trigger: Option<i64>,
        signal_at: i64,
        handle_at: i64,
        signal_cmd: &str,
        raw_price_event: Value,
        follow_up: bool,
    ) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        let settings = store.state.settings;
        if !settings.mode.plans() {
            return events;
        }
        let now = self.now();
        let desired_qty = config.desired_qty;
        {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            if runtime.inflight {
                return events;
            }
            if runtime.blocked_until > now {
                return events;
            }
        }
        // A follow-up extends a fill the engine has just booked rather than
        // opening a new signal, so the inter-order cooldown does not apply to
        // it. It still pays for a rate slot below.
        if !follow_up && now - self.last_order_at < settings.delay_per_order {
            self.logger
                .debug("engine", format!("{label} in range, waiting for cooldown"));
            return events;
        }
        // Checked before the rate slot on purpose: a live worker without a
        // session cannot place the order anyway, so spending its per-minute
        // budget would only make the next frames report a misleading
        // "order cap reached" instead of the real cause.
        if settings.mode.submits() && !self.has_token {
            if now - self.last_no_token_warn_at > 30_000 {
                self.logger.warn(
                    "engine",
                    "signal found but no Rollercoin session is linked; open the Link tab",
                );
                self.last_no_token_warn_at = now;
            }
            return events;
        }
        if !self.take_rate_slot(now, settings.max_orders_per_minute) {
            let should_log = {
                let runtime = self.runtime_for(&config.item_id, desired_qty);
                if now - runtime.last_log_at > 10_000 {
                    runtime.last_log_at = now;
                    true
                } else {
                    false
                }
            };
            if should_log {
                self.logger.warn(
                    "engine",
                    format!(
                        "order cap reached ({}/min); {} skipped",
                        settings.max_orders_per_minute, label
                    ),
                );
            }
            return events;
        }

        let qty = qty.max(1);
        let notional = round_price(unit_price * qty as f64);
        let timings = Timings {
            v: 1,
            signal_at: Some(signal_at as f64),
            handle_at: Some(handle_at as f64),
            order_at: Some(now as f64),
            signal_cmd: Some(signal_cmd.to_string()),
            ..Default::default()
        };

        self.last_order_at = now;
        {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            runtime.last_order_at = now;
            runtime.orders += 1;
        }

        let intent = OrderIntent {
            item_id: config.item_id.clone(),
            item_type: item_type.to_string(),
            label: label.to_string(),
            qty,
            stock_at_trigger,
            total_count_at_trigger,
            unit_price_raw: price_raw,
            unit_price,
            target_price: config.target_price,
            notional,
            doubled,
            optimistic: follow_up,
            at: now,
            timings,
            raw_price_event,
        };

        if !settings.mode.submits() {
            self.runtime_for(&config.item_id, desired_qty).last_result = Some(json!({
                "status": "dry", "at": now, "qty": qty, "price": unit_price
            }));
            self.logger.info(
                "order",
                format!("SHADOW — would buy {label} ×{qty} @ {unit_price:.6} RLT"),
            );
            events.push(EngineEvent::Broadcast {
                kind: "order",
                data: json!({
                    "phase": "dry",
                    "itemId": intent.item_id,
                    "itemType": intent.item_type,
                    "label": intent.label,
                    "qty": intent.qty,
                    "unitPriceRaw": intent.unit_price_raw,
                    "unitPrice": intent.unit_price,
                    "targetPrice": intent.target_price,
                    "notional": intent.notional,
                    "at": intent.at,
                    "timings": intent.timings,
                    "reason": "dry-run",
                    "mode": settings.mode,
                    "doubled": intent.doubled,
                    "optimistic": intent.optimistic,
                }),
            });
            return events;
        }

        store.state.stats.orders_sent += 1;
        // Only the counters changed; rewriting the whole target list here made
        // one order cost an upsert per configured item.
        if let Err(error) = store.flush_stats() {
            warn_throttled(
                &self.logger,
                &mut self.last_flush_error_at,
                now,
                "store",
                format!("could not persist the order counter: {error}"),
            );
        }
        {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            runtime.inflight = true;
            runtime.last_result = Some(json!({
                "status": "submitted", "at": now, "qty": qty, "price": unit_price
            }));
            // The units are committed the moment the request goes out, so the
            // ledger opens here rather than waiting for the result frame. A
            // stale frame carrying the pre-purchase book can no longer order
            // them a second time, and the early rearm reads a floor that already
            // has every outstanding order reserved out of it.
            let key = price_key(unit_price);
            let live = matches!(runtime.reconcile, Some(window) if window.live(now) && window.price_key == key);
            if live {
                if let Some(window) = runtime.reconcile.as_mut() {
                    window.reserve(qty);
                }
            } else if let Some(stock) = stock_at_trigger {
                let mut window = Reconcile::open(now, stock, unit_price, total_count_at_trigger);
                window.reserve(qty);
                runtime.reconcile = Some(window);
            }
        }

        let mut submitted = intent;
        submitted.timings.dispatch_at = Some(self.now() as f64);
        // Provisional: the wiring replaces this with the real HTTP dispatch
        // time when it has one, so live broadcasts still carry a mark.
        submitted.timings.send_at = Some(self.now() as f64);

        events.push(EngineEvent::Broadcast {
            kind: "order",
            data: json!({
                "phase": "submitted",
                "itemId": submitted.item_id,
                "itemType": submitted.item_type,
                "label": submitted.label,
                "qty": submitted.qty,
                "unitPriceRaw": submitted.unit_price_raw,
                "unitPrice": submitted.unit_price,
                "targetPrice": submitted.target_price,
                "notional": submitted.notional,
                "at": submitted.at,
                "timings": submitted.timings,
                "optimistic": submitted.optimistic,
            }),
        });
        let marker = if submitted.optimistic {
            " [LOCAL REARM]"
        } else if submitted.doubled {
            " [HIGH STOCK ×2]"
        } else {
            ""
        };
        self.logger.info(
            "order",
            format!(
                "SEND {} ×{} @ {:.6} RLT ({:.6} total){}",
                label, qty, submitted.unit_price, submitted.notional, marker
            ),
        );

        events.push(EngineEvent::Submit(Box::new(submitted)));
        events
    }

    // ── purchase result ─────────────────────────────────────────────────────

    pub fn on_purchase_result(
        &mut self,
        store: &mut Store,
        intent: &OrderIntent,
        outcome: PurchaseOutcome,
    ) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        let now = self.now();
        let mut timings = intent.timings.clone();
        // Overwrite the provisional engine mark with the real HTTP dispatch.
        if let Some(send_at) = outcome.send_at {
            timings.send_at = Some(send_at as f64);
        }
        timings.response_at = outcome.response_at.map(|value| value as f64);
        timings.complete_at = outcome.complete_at.map(|value| value as f64);
        timings.ttfb_ms = outcome.ttfb_ms;

        let accepted = outcome.ok && outcome.order_id.is_some();
        // Released before the result is handled: a fill that beat the HTTP
        // response can fire a follow-up from the replay below, and that
        // follow-up needs the slot. Clearing it afterwards would also mark the
        // follow-up as not in flight, which would let a third order through.
        self.runtime_for(&intent.item_id, intent.qty.max(1))
            .inflight = false;
        if accepted {
            timings.accepted_at = Some(now as f64);
            let order_id = outcome.order_id.clone().unwrap_or_default();
            self.pending.insert(
                order_id.clone(),
                PendingOrder {
                    item_id: intent.item_id.clone(),
                    item_type: intent.item_type.clone(),
                    label: intent.label.clone(),
                    qty: intent.qty,
                    stock_at_trigger: intent.stock_at_trigger,
                    total_count_at_trigger: intent.total_count_at_trigger,
                    optimistic: intent.optimistic,
                    unit_price: intent.unit_price,
                    target_price: intent.target_price,
                    at: intent.at,
                    timings: timings.clone(),
                    raw_price_event: intent.raw_price_event.clone(),
                    request_payload: outcome.request_payload.clone(),
                },
            );

            {
                let runtime = self.runtime_for(&intent.item_id, intent.qty.max(1));
                runtime.failure_streak = 0;
                runtime.blocked_until = 0;
                runtime.last_result = Some(json!({
                    "status": "accepted", "at": now, "orderId": order_id, "qty": intent.qty
                }));
            }

            self.logger.info(
                "order",
                format!(
                    "ACCEPTED {} ×{} (order {})",
                    intent.label, intent.qty, order_id
                ),
            );
            events.push(EngineEvent::Broadcast {
                kind: "order",
                data: json!({
                    "phase": "accepted",
                    "itemId": intent.item_id,
                    "itemType": intent.item_type,
                    "label": intent.label,
                    "qty": intent.qty,
                    "unitPriceRaw": intent.unit_price_raw,
                    "unitPrice": intent.unit_price,
                    "targetPrice": intent.target_price,
                    "notional": intent.notional,
                    "at": intent.at,
                    "timings": timings,
                    "orderId": order_id,
                }),
            });

            // The upstream result can beat the purchase HTTP response.
            if let Some(index) = self.unmatched.iter().position(|(id, _)| id == &order_id) {
                if let Some((_, buffered)) = self.unmatched.remove(index) {
                    events.extend(self.handle_fill(
                        store,
                        &buffered.event,
                        &buffered.raw,
                        buffered.received_at,
                    ));
                }
            }
        } else {
            let reason = outcome
                .error
                .clone()
                .unwrap_or_else(|| "marketplace rejected the order".to_string());
            timings.failed_at = Some(now as f64);
            let (streak, backoff) = {
                let runtime = self.runtime_for(&intent.item_id, intent.qty.max(1));
                if intent.optimistic {
                    // An optimistic follow-up is a guess about the remainder of
                    // a partial fill, so a rejection is an expected outcome.
                    // Backing off here would freeze the whole target for up to
                    // a minute — far longer than the frame wait it replaced.
                    // The reconcile window is deliberately left in place: the
                    // guess failed, which is no evidence that the next frame is
                    // fresh, and the window bounds the blindness to 1.5 s.
                    runtime.last_result = Some(json!({
                        "status": "rejected", "at": now, "reason": reason,
                        "optimistic": true
                    }));
                    (runtime.failure_streak, 0)
                } else {
                    runtime.failure_streak += 1;
                    let backoff = (5_000_i64
                        * 2_i64.pow((runtime.failure_streak - 1).min(3) as u32))
                    .min(60_000);
                    runtime.blocked_until = now + backoff;
                    runtime.last_result = Some(json!({
                        "status": "rejected", "at": now, "reason": reason
                    }));
                    (runtime.failure_streak, backoff)
                }
            };
            if backoff > 0 {
                self.logger.warn(
                    "order",
                    format!(
                        "REJECTED {} ×{}: {} — backing off {}s ({})",
                        intent.label,
                        intent.qty,
                        reason,
                        backoff / 1000,
                        streak
                    ),
                );
            } else {
                self.logger.warn(
                    "order",
                    format!(
                        "REJECTED {} ×{}: {} — optimistic follow-up, not backing off",
                        intent.label, intent.qty, reason
                    ),
                );
            }
            events.extend(self.record_failure(
                store,
                FailureMeta {
                    item_id: intent.item_id.clone(),
                    label: intent.label.clone(),
                    qty: intent.qty,
                    // Rejected purchase: no fulfillment event, so Got is 0.
                    got_qty: 0,
                    stock_at_trigger: intent.stock_at_trigger,
                    unit_price: intent.unit_price,
                    target_price: intent.target_price,
                    at: intent.at,
                    timings: Some(timings.clone()),
                    raw_price_event: Some(intent.raw_price_event.clone()),
                    raw_result_event: None,
                    request_payload: outcome.request_payload.clone(),
                    optimistic: intent.optimistic,
                },
                &reason,
                outcome.order_id.clone(),
            ));
            events.push(EngineEvent::Broadcast {
                kind: "order",
                data: json!({
                    "phase": "failed",
                    "itemId": intent.item_id,
                    "label": intent.label,
                    "qty": intent.qty,
                    "reason": reason,
                    "timings": timings,
                    "at": intent.at,
                    "optimistic": intent.optimistic,
                }),
            });
        }

        events
    }

    // ── fills ───────────────────────────────────────────────────────────────

    fn handle_fill(
        &mut self,
        store: &mut Store,
        event: &UpstreamEvent,
        raw: &Value,
        received_at: i64,
    ) -> Vec<EngineEvent> {
        let Some(fill) = event.fill.clone() else {
            return Vec::new();
        };
        let pending = fill
            .order_id
            .as_ref()
            .and_then(|order_id| self.pending.get(order_id).cloned());

        let Some(pending_meta) = pending else {
            match &fill.order_id {
                Some(order_id) => self.buffer_unmatched(order_id, event, raw, received_at),
                None => self
                    .logger
                    .debug("fill", "ignoring result without an order id"),
            }
            return Vec::new();
        };

        let item_id = if pending_meta.item_id.is_empty() {
            event.item_id.clone().unwrap_or_default()
        } else {
            pending_meta.item_id.clone()
        };
        let config = store.find_config(&item_id).cloned();
        let now = self.now();
        let fill_at = received_at;
        let mut fill_timings = pending_meta.timings.clone();
        fill_timings.fill_at = Some(fill_at as f64);
        fill_timings.fill_handled_at = Some(now as f64);

        let label = if pending_meta.label.is_empty() {
            label_for(config.as_ref(), &item_id)
        } else {
            pending_meta.label.clone()
        };
        let target_price = Some(pending_meta.target_price)
            .filter(|value| value.is_finite() && *value > 0.0)
            .or_else(|| config.as_ref().map(|config| config.target_price));

        if let Some(order_id) = &fill.order_id {
            if store.has_order(order_id) {
                self.logger.debug(
                    "fill",
                    format!("ignoring duplicate confirmation {order_id}"),
                );
                return Vec::new();
            }
        }

        let mut events = Vec::new();
        if fill.status as i64 == ORDER_STATUS_FILLED && fill.count > 0 && fill.price_raw > 0.0 {
            let total_price = fill.price_raw / 1_000_000.0;
            let unit_price = total_price / fill.count as f64;
            let reference = target_price.unwrap_or(unit_price);
            let profit = round_price((reference - unit_price) * fill.count as f64);

            {
                let stats = &mut store.state.stats;
                stats.filled += 1;
                stats.bought += fill.count;
                stats.spent_rlt = round_price(stats.spent_rlt + total_price);
                stats.profit_rlt = round_price(stats.profit_rlt + profit);
            }

            let entry = OrderEntry {
                id: fill
                    .order_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                order_id: fill.order_id.clone(),
                item_id: item_id.clone(),
                label: label.clone(),
                qty: fill.count,
                requested_qty: Some(pending_meta.qty),
                stock_at_trigger: pending_meta.stock_at_trigger,
                unit_price: round_price(unit_price),
                total_price: round_price(total_price),
                target_price: reference,
                profit_rlt: profit,
                status: "filled".to_string(),
                at: now,
                latency_ms: Some(now - pending_meta.at),
                timings: Some(fill_timings),
                raw_events: crate::raw_events::sanitize(&json!({
                    "purchaseRequest": pending_meta.request_payload,
                    "priceEvent": pending_meta.raw_price_event,
                    "resultEvent": raw,
                })),
                note: pending_meta
                    .optimistic
                    .then(|| "optimistic follow-up".to_string()),
                message: Some("-".to_string()),
            };

            let entry_id = entry.id.clone();
            if let Err(error) = store.add_history(entry.clone()) {
                warn_throttled(
                    &self.logger,
                    &mut self.last_history_error_at,
                    now,
                    "store",
                    format!("could not record fill {entry_id}: {error}"),
                );
            }
            if let Some(order_id) = &fill.order_id {
                self.pending.remove(order_id);
            }

            // The fill is booked against the local ledger: the part of the
            // request upstream did not take comes back to the book, and what it
            // did take stays spent. The raw `last_stock` is deliberately left
            // alone — it must keep reporting what upstream said.
            let stock_after = {
                let runtime = self.runtime_for(&item_id, pending_meta.qty.max(1));
                runtime.fills += 1;
                runtime.last_result = Some(json!({
                    "status": "filled", "at": now, "qty": fill.count,
                    "price": unit_price, "profit": profit
                }));
                runtime
                    .settle_fill(
                        now,
                        pending_meta.qty,
                        fill.count,
                        pending_meta.total_count_at_trigger,
                        pending_meta.unit_price,
                    )
                    .or(runtime.last_stock)
                    .unwrap_or(0)
            };

            self.logger.info(
                "fill",
                format!(
                    "FILLED {label} ×{} @ {:.6} RLT | profit {}{:.6} RLT | {}ms",
                    fill.count,
                    unit_price,
                    if profit >= 0.0 { "+" } else { "" },
                    profit,
                    entry.latency_ms.unwrap_or(0)
                ),
            );

            let mut data = serde_json::to_value(&entry).unwrap_or(Value::Null);
            if let Some(object) = data.as_object_mut() {
                object.insert("phase".to_string(), Value::String("filled".into()));
                object.insert("stockAfter".to_string(), json!(stock_after));
            }
            events.push(EngineEvent::Broadcast {
                kind: "order",
                data,
            });
            events.push(EngineEvent::Broadcast {
                kind: "stats",
                data: serde_json::to_value(store.state.stats).unwrap_or(Value::Null),
            });
            events.push(EngineEvent::Broadcast {
                kind: "report",
                data: serde_json::to_value(store.report_view()).unwrap_or(Value::Null),
            });
            self.logger
                .debug("fill", format!("recorded {entry_id} for {item_id}"));
            events.extend(self.rearm_after_fill(
                store,
                config.as_ref(),
                &pending_meta,
                &label,
                fill.count,
                fill_at,
                now,
            ));
            return events;
        }

        if fill.status as i64 == ORDER_STATUS_FAILED && fill.count == 0 && fill.price_raw == 0.0 {
            if let Some(order_id) = &fill.order_id {
                self.pending.remove(order_id);
            }
            // Upstream just said there is nothing, so this order's reservation
            // goes back to the book. The ledger is kept rather than dropped: a
            // follow-up that is still in flight owns its own share of the same
            // floor, and the next frame closes the window by showing at most
            // what is actually left.
            self.runtime_for(&item_id, pending_meta.qty.max(1))
                .release_reservation(now, pending_meta.qty, pending_meta.unit_price);
            self.logger.warn(
                "fill",
                format!(
                    "FAILED {label} — no items were obtained (order {})",
                    fill.order_id.clone().unwrap_or_else(|| "?".into())
                ),
            );
            events.extend(self.record_failure(
                store,
                FailureMeta {
                    item_id: item_id.clone(),
                    label: label.clone(),
                    // Keep the attempted order in the loot row. The upstream
                    // result is empty (count/price 0), but the log must still
                    // show what we tried to buy and how long it took, exactly
                    // like `failureMeta = pendingMeta` in the Bun engine.
                    qty: pending_meta.qty,
                    // Got comes straight from the event: fill.count is 0 on
                    // this branch (checked above).
                    got_qty: fill.count,
                    stock_at_trigger: pending_meta.stock_at_trigger,
                    unit_price: pending_meta.unit_price,
                    target_price: target_price.unwrap_or(pending_meta.target_price),
                    at: pending_meta.at,
                    timings: Some(fill_timings),
                    raw_price_event: Some(pending_meta.raw_price_event.clone()),
                    raw_result_event: Some(raw.clone()),
                    request_payload: pending_meta.request_payload.clone(),
                    optimistic: pending_meta.optimistic,
                },
                "no items obtained",
                fill.order_id.clone(),
            ));
            events.push(EngineEvent::Broadcast {
                kind: "order",
                data: json!({
                    "phase": "failed",
                    "itemId": item_id,
                    "label": label,
                    "orderId": fill.order_id,
                    "reason": "no items obtained",
                    "at": now,
                }),
            });
            return events;
        }

        self.logger.debug(
            "fill",
            format!("ignored fill status {} for {label}", fill.status),
        );
        events
    }

    /// Fires the next slice of a level as soon as its fill lands, instead of
    /// waiting for the next market frame to re-advertise the leftover stock.
    ///
    /// The quantity no longer comes from the intent's own unfilled remainder.
    /// It comes from the same planner a frame would call, against the local
    /// ledger the fill just settled, so every quantity relationship is covered:
    ///
    /// * `got < sent` — the order was partially matched and the rest of the
    ///   request is still on the book;
    /// * `sent < ready` — the request never covered the best level, so the level
    ///   still holds `ready - sent` units that a frame would re-advertise one WS
    ///   hop later;
    /// * a full fill of the whole level — the settled book is empty and the
    ///   planner returns `no-stock`, so nothing is fired.
    ///
    /// The local floor is the bound, not `stockAtTrigger - got`. The floor
    /// already had every order the engine placed reserved out of it, so a stale
    /// frame or an earlier follow-up can never hand the same units to two
    /// orders — the wrong buy this used to trigger when the real result landed.
    ///
    /// The plan still goes through the normal price/target/minimum checks; a
    /// slice below `min_acceptable_qty` is left alone, exactly like a thin frame
    /// would be.
    #[allow(clippy::too_many_arguments)]
    fn rearm_after_fill(
        &mut self,
        store: &mut Store,
        config: Option<&Target>,
        pending: &PendingOrder,
        label: &str,
        got: i64,
        signal_at: i64,
        now: i64,
    ) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        let Some(config) = config else {
            return events;
        };
        if got <= 0 {
            return events;
        }
        if !store.state.settings.mode.plans() {
            return events;
        }
        let key = price_key(pending.unit_price);
        let available = {
            let runtime = self.runtime_for(&config.item_id, config.desired_qty);
            match runtime.reconcile {
                // Only a live ledger for the same level may be traded from:
                // once a frame (or the window) has taken authority back, the
                // fill's old trigger snapshot is stale and re-arming from it
                // would buy goods that are already committed or gone.
                Some(window) if window.live(now) && window.price_key == key => window.floor,
                _ => return events,
            }
        };
        if available < 1 {
            return events;
        }

        let plan = plan_order(
            crate::rules::PlanInput {
                active: config.active,
                target_price: config.target_price,
                // A fill is a fresh look at the same level, so the target's
                // normal per-signal quantity applies. It is deliberately not
                // the intent's unfilled remainder: when `sent < ready` the book
                // is deeper than the request and a frame would order the same
                // quantity again.
                desired_qty: config.desired_qty,
                // Never re-double a level just because high-stock mode would
                // have doubled it on a frame: a fill confirms what was taken,
                // it is not a new signal.
                high_stock_threshold: i64::MAX,
                min_acceptable_qty: config.min_acceptable_qty,
                allow_partial_order: config.allow_partial_order,
            },
            pending.unit_price,
            available as f64,
            false,
            Some(config.target_price),
        );
        if !plan.is_buy() {
            self.logger.info(
                "order",
                format!(
                    "fill for {label} leaves {available} on the book, but the plan says {} — no local rearm",
                    plan.reason.unwrap_or("skip")
                ),
            );
            return events;
        }
        let qty = plan.qty.unwrap_or(1);
        // `plan_order` only applies the minimum when the book is thinner than
        // the request. A follow-up asks for a slice of the level, so it would
        // otherwise slip a below-minimum order through; holding it to the same
        // minimum keeps a config like "never fill fewer than 2" honest.
        if qty < config.min_acceptable_qty {
            self.logger.info(
                "order",
                format!(
                    "fill for {label} leaves {qty}, below the {}-unit minimum — no local rearm",
                    config.min_acceptable_qty
                ),
            );
            return events;
        }
        self.logger.info(
            "order",
            format!(
                "LOCAL REARM {label} ×{qty} @ {:.6} RLT — {available} left on the level after the fill, not waiting for the next frame",
                pending.unit_price
            ),
        );
        events.extend(self.place_order(
            store,
            config,
            label,
            &pending.item_type,
            qty,
            false,
            (pending.unit_price * PRICE_SCALE).round(),
            pending.unit_price,
            Some(available),
            // The follow-up shares the window that is already open; forwarding
            // the parent's `totalCount` would be a stale proof.
            None,
            signal_at,
            now,
            "marketplace_orders_filled",
            pending.raw_price_event.clone(),
            true,
        ));
        events
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn compute_trend(
        config: &Target,
        runtime: &mut Runtime,
        now: i64,
        force: bool,
    ) -> Option<TrendSignal> {
        let target_price = config.target_price;
        if !force
            && runtime.trend.is_some()
            && runtime.trend_target == target_price
            && now - runtime.trend_at < TREND_REFRESH_MS
        {
            return runtime.trend.clone();
        }

        let previous_state = if runtime.trend_target == target_price {
            runtime.trend.as_ref().map(|trend| trend.state.to_string())
        } else {
            None
        };

        let fill_points = &runtime.fill_history;
        let fill_span = match (fill_points.first(), fill_points.last()) {
            (Some(first), Some(last)) => last.t - first.t,
            _ => 0,
        };
        let fill_fresh = fill_points
            .last()
            .map(|point| now - point.t <= 30 * 60_000)
            .unwrap_or(false);
        let use_fill = fill_points.len() >= 12 && fill_span >= 30 * 60_000 && fill_fresh;

        let book_qty = runtime.last_stock;
        let options = TrendOptions {
            now,
            target_price: Some(target_price),
            previous_state,
            book_qty,
            min_qty: Some(config.min_acceptable_qty),
            depth_source: use_fill,
            fill: runtime.last_fill.map(|fill| FillRef {
                price: fill.price,
                filled: fill.filled as f64,
                partial: fill.partial,
                approx: fill.approx,
            }),
        };
        let signal = {
            // Borrow the series instead of cloning up to 1440 points on every
            // rebuild: the model only ever reads it, so the copy was pure churn.
            let series: &[Point] = if use_fill {
                &runtime.fill_history
            } else {
                &runtime.history
            };
            build_trend(series, &options)
        };
        runtime.trend = Some(signal.clone());
        runtime.trend_json = serde_json::to_string(&signal)
            .ok()
            .and_then(|text| RawValue::from_string(text).ok())
            .map(Arc::from);
        runtime.trend_at = now;
        runtime.trend_target = target_price;
        Some(signal)
    }

    #[allow(clippy::too_many_arguments)]
    fn persist_market(
        &mut self,
        store: &Store,
        config: &Target,
        now: i64,
        price: f64,
        low: f64,
        high: f64,
        raw_low: f64,
        raw_high: f64,
        fill: Option<Fill>,
        bucket: i64,
        band_changed: bool,
        finalized: Option<(i64, f64, f64, f64, f64, f64)>,
    ) {
        let (persisted_at, point_write_at, last_bucket) = {
            let runtime = self.runtime_for(&config.item_id, config.desired_qty);
            (
                runtime.persisted_at,
                runtime.point_write_at,
                runtime.last_point_bucket,
            )
        };

        // A frame that carries the marketplace band forces a snapshot regardless
        // of the throttle; ordinary ticks are throttled to one write per window.
        if band_changed || now - persisted_at >= MARKET_SNAPSHOT_THROTTLE_MS {
            let snapshot = {
                let runtime = self.runtime_for(&config.item_id, config.desired_qty);
                MarketSnapshot {
                    item_id: config.item_id.clone(),
                    last_price: runtime.last_price,
                    last_stock: runtime.last_stock,
                    allowed_price_min: runtime.allowed_price_min,
                    allowed_price_max: runtime.allowed_price_max,
                    allowed_at: runtime.allowed_at,
                    item_type: runtime.item_type.clone(),
                    seen_at: runtime.seen_at.unwrap_or(now),
                }
            };
            if let Err(error) = store.save_market_snapshot(&snapshot) {
                let runtime = self.runtime_for(&config.item_id, config.desired_qty);
                if now - runtime.persist_error_at > 60_000 {
                    runtime.persist_error_at = now;
                    self.logger.warn(
                        "engine",
                        format!("could not persist market snapshot: {error}"),
                    );
                }
            } else {
                self.runtime_for(&config.item_id, config.desired_qty)
                    .persisted_at = now;
            }
        }

        // The minute that just ended is written with its exact median instead
        // of the running value its last frame happened to see. `record_price_samples`
        // only touches the close and band, so a fill recorded in that bucket
        // survives.
        if let Some((old_bucket, old_close, old_low, old_high, old_raw_low, old_raw_high)) =
            finalized
            && let Err(error) = store.record_price_samples(&[(
                &config.item_id,
                old_bucket,
                old_close,
                old_low,
                old_high,
                old_raw_low,
                old_raw_high,
            )])
        {
            let runtime = self.runtime_for(&config.item_id, config.desired_qty);
            if now - runtime.persist_error_at > 60_000 {
                runtime.persist_error_at = now;
                self.logger
                    .warn("engine", format!("could not finalise price point: {error}"));
            }
        }

        if last_bucket != Some(bucket) || now - point_write_at >= PRICE_POINT_WRITE_INTERVAL_MS {
            let fill_tuple = fill.map(|fill| (fill.price, fill.filled as f64, fill.approx));
            let band = PriceBand {
                low,
                high,
                raw_low,
                raw_high,
            };
            if let Err(error) =
                store.record_price_point(&config.item_id, bucket, price, band, fill_tuple)
            {
                let runtime = self.runtime_for(&config.item_id, config.desired_qty);
                if now - runtime.persist_error_at > 60_000 {
                    runtime.persist_error_at = now;
                    self.logger
                        .warn("engine", format!("could not record price point: {error}"));
                }
            } else {
                let runtime = self.runtime_for(&config.item_id, config.desired_qty);
                runtime.last_point_bucket = Some(bucket);
                runtime.point_write_at = now;
            }
        }
    }

    fn log_scan(&mut self, config: &Target, price: f64, stock: i64, plan: &Plan, now: i64) {
        let runtime = self.runtime_for(&config.item_id, config.desired_qty);
        if now - runtime.last_log_at < SCAN_LOG_INTERVAL_MS {
            return;
        }
        runtime.last_log_at = now;
        let label = label_for(Some(config), &config.item_id);
        let message = if plan.action == Action::Buy {
            format!(
                "{label} {price:.6} RLT ×{stock} — IN RANGE (target {:.6})",
                config.target_price
            )
        } else if price <= config.target_price {
            // `plan.reason` is the real reason: callers pass the observed plan
            // (evaluated even while off), never the gated one.
            format!(
                "{label} {price:.6} RLT ×{stock} — IN RANGE, waiting ({})",
                plan.reason.unwrap_or("unknown")
            )
        } else {
            format!(
                "{label} {price:.6} RLT ×{stock} — above target {:.6}",
                config.target_price
            )
        };
        self.logger.info("scan", message);
    }

    fn take_rate_slot(&mut self, now: i64, max_per_minute: i64) -> bool {
        let cutoff = now - 60_000;
        self.order_stamps.retain(|stamp| *stamp >= cutoff);
        if self.order_stamps.len() as i64 >= max_per_minute {
            return false;
        }
        self.order_stamps.push(now);
        true
    }

    /// How many rate-limit slots the current minute has already spent. Used by
    /// tests to prove that `off` costs nothing while `shadow` does.
    #[cfg(test)]
    fn order_stamps_len(&self) -> usize {
        self.order_stamps.len()
    }

    fn buffer_unmatched(
        &mut self,
        order_id: &str,
        event: &UpstreamEvent,
        raw: &Value,
        received_at: i64,
    ) {
        let now = self.now();
        self.prune_unmatched(now);
        self.unmatched.retain(|(id, _)| id != order_id);
        while self.unmatched.len() >= MAX_UNMATCHED_RESULTS {
            if let Some((oldest, _)) = self.unmatched.pop_front() {
                self.logger.debug(
                    "fill",
                    format!("discarding unmatched result {oldest} (buffer full)"),
                );
            }
        }
        self.unmatched.push_back((
            order_id.to_string(),
            BufferedResult {
                event: event.clone(),
                raw: raw.clone(),
                received_at,
                buffered_at: now,
            },
        ));
        self.logger.debug(
            "fill",
            format!("unmatched result {order_id}; waiting briefly for purchase response"),
        );
    }

    fn prune_unmatched(&mut self, now: i64) {
        let mut kept = VecDeque::new();
        while let Some((order_id, buffered)) = self.unmatched.pop_front() {
            if now - buffered.buffered_at > PENDING_TIMEOUT_MS {
                self.logger.debug(
                    "fill",
                    format!("discarding unmatched result {order_id} after 120s"),
                );
            } else {
                kept.push_back((order_id, buffered));
            }
        }
        self.unmatched = kept;
    }

    fn record_failure(
        &mut self,
        store: &mut Store,
        meta: FailureMeta,
        reason: &str,
        order_id: Option<String>,
    ) -> Vec<EngineEvent> {
        store.state.stats.failed += 1;
        {
            let runtime = self.runtime_for(&meta.item_id, meta.qty.max(1));
            runtime.failures += 1;
        }
        let now = self.now();
        let mut timings = meta.timings.clone().unwrap_or_default();
        if timings.failed_at.is_none() {
            timings.failed_at = Some(now as f64);
        }

        let entry = OrderEntry {
            id: order_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            order_id,
            item_id: meta.item_id.clone(),
            label: meta.label.clone(),
            // Failures: `qty` is the count from the fulfillment result event
            // (`fill.count`, carried in `meta.got_qty`), while `requested_qty`
            // keeps the attempted count for the loot log's Sent column.
            qty: meta.got_qty,
            requested_qty: Some(meta.qty),
            stock_at_trigger: meta.stock_at_trigger,
            unit_price: meta.unit_price,
            total_price: 0.0,
            target_price: meta.target_price,
            profit_rlt: 0.0,
            status: "failed".to_string(),
            at: now,
            latency_ms: Some(now - meta.at).filter(|value| *value >= 0),
            timings: Some(timings),
            raw_events: crate::raw_events::sanitize(&json!({
                "purchaseRequest": meta.request_payload,
                "priceEvent": meta.raw_price_event,
                "resultEvent": meta.raw_result_event,
            })),
            note: Some(if meta.optimistic {
                format!("optimistic follow-up — {reason}")
                    .chars()
                    .take(160)
                    .collect()
            } else {
                reason.chars().take(160).collect()
            }),
            message: Some(reason.chars().take(160).collect()),
        };

        if let Err(error) = store.add_history(entry.clone()) {
            warn_throttled(
                &self.logger,
                &mut self.last_history_error_at,
                now,
                "store",
                format!("could not record the failed order: {error}"),
            );
        }

        vec![
            EngineEvent::Broadcast {
                kind: "history",
                data: serde_json::to_value(&entry).unwrap_or(Value::Null),
            },
            EngineEvent::Broadcast {
                kind: "stats",
                data: serde_json::to_value(store.state.stats).unwrap_or(Value::Null),
            },
            EngineEvent::Broadcast {
                kind: "report",
                data: serde_json::to_value(store.report_view()).unwrap_or(Value::Null),
            },
        ]
    }

    // ── lifecycle ───────────────────────────────────────────────────────────

    /// Rebuilds the display cache from SQLite after a restart.
    pub fn hydrate_market(&mut self, store: &Store) -> usize {
        let snapshots = store.load_market_snapshots();
        let mut restored = 0;
        for snapshot in snapshots {
            if snapshot.seen_at <= 0 {
                continue;
            }
            let Some(config) = store.find_config(&snapshot.item_id).cloned() else {
                continue;
            };
            let now = self.now();
            let desired_qty = config.desired_qty;
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            if snapshot.last_price.is_some() {
                runtime.last_price = snapshot.last_price;
            }
            if snapshot.last_stock.is_some() {
                runtime.last_stock = snapshot.last_stock;
            }
            if snapshot.allowed_price_min.is_some() {
                runtime.allowed_price_min = snapshot.allowed_price_min;
            }
            if snapshot.allowed_price_max.is_some() {
                runtime.allowed_price_max = snapshot.allowed_price_max;
            }
            runtime.allowed_at = snapshot.allowed_at;
            runtime.item_type = snapshot
                .item_type
                .clone()
                .or_else(|| runtime.item_type.clone())
                .or_else(|| Some(config.item_type.clone()));
            runtime.seen_at = Some(snapshot.seen_at);
            runtime.hydrated_at = Some(now);
            runtime.stale = true;

            let stored = store.load_price_history(&config.item_id, PRICE_HISTORY_LIMIT, 0);
            if !stored.is_empty() {
                let mut merged: HashMap<i64, f64> = runtime
                    .history
                    .iter()
                    .map(|point| (point.t, point.p))
                    .collect();
                for point in &stored {
                    merged.insert(point.bucket_at, point.price);
                }
                let mut points: Vec<Point> =
                    merged.into_iter().map(|(t, p)| Point { t, p }).collect();
                points.sort_by_key(|point| point.t);
                if points.len() > PRICE_HISTORY_LIMIT {
                    points.drain(0..points.len() - PRICE_HISTORY_LIMIT);
                }
                runtime.history = points;

                let fill_points: Vec<Point> = stored
                    .iter()
                    .filter_map(|point| {
                        point.fill_price.map(|price| Point {
                            t: point.bucket_at,
                            p: price,
                        })
                    })
                    .collect();
                if !fill_points.is_empty() {
                    let mut merged_fill: HashMap<i64, f64> = runtime
                        .fill_history
                        .iter()
                        .map(|point| (point.t, point.p))
                        .collect();
                    for point in &fill_points {
                        merged_fill.insert(point.t, point.p);
                    }
                    let mut points: Vec<Point> = merged_fill
                        .into_iter()
                        .map(|(t, p)| Point { t, p })
                        .collect();
                    points.sort_by_key(|point| point.t);
                    if points.len() > PRICE_HISTORY_LIMIT {
                        points.drain(0..points.len() - PRICE_HISTORY_LIMIT);
                    }
                    runtime.fill_history = points;

                    if let Some(latest) = stored.last() {
                        if let Some(fill_price) = latest.fill_price {
                            runtime.last_fill = Some(Fill {
                                price: fill_price,
                                filled: latest.fill_qty.unwrap_or(1.0) as i64,
                                requested: desired_qty,
                                partial: false,
                                approx: latest.fill_approx,
                                levels: 0,
                            });
                        }
                    }
                    runtime.fill_qty_for = desired_qty;
                }
            }

            let runtime = self.runtime_for(&config.item_id, desired_qty);
            Self::compute_trend(&config, runtime, now, true);
            restored += 1;
        }

        if restored > 0 {
            self.logger.info(
                "engine",
                format!(
                    "restored {restored} cached market snapshots from sqlite — waiting for live frames"
                ),
            );
        }
        restored
    }

    /// Writes every known snapshot once, used on graceful shutdown.
    pub fn flush_market_cache(&mut self, store: &Store) {
        let rows: Vec<MarketSnapshot> = self
            .runtime
            .iter()
            .filter_map(|(item_id, runtime)| {
                let seen_at = runtime.seen_at.or(runtime.last_seen_at)?;
                store.find_config(item_id)?;
                Some(MarketSnapshot {
                    item_id: item_id.clone(),
                    last_price: runtime.last_price,
                    last_stock: runtime.last_stock,
                    allowed_price_min: runtime.allowed_price_min,
                    allowed_price_max: runtime.allowed_price_max,
                    allowed_at: runtime.allowed_at,
                    item_type: runtime.item_type.clone(),
                    seen_at,
                })
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        if let Err(error) = store.save_market_snapshots(&rows) {
            self.logger
                .warn("engine", format!("could not persist market cache: {error}"));
        }
    }

    pub fn reset_market_state(&mut self) {
        self.runtime.clear();
    }

    pub fn reset_counters(&mut self) {
        for runtime in self.runtime.values_mut() {
            runtime.orders = 0;
            runtime.fills = 0;
            runtime.failures = 0;
            runtime.last_result = None;
        }
    }

    /// Releases runtimes for targets that no longer exist and have not been
    /// seen for `max_idle_ms`. A target that is deleted and re-added within that
    /// window keeps its history, so the chart is unchanged.
    pub fn prune_orphan_runtimes(&mut self, store: &Store, now: i64, max_idle_ms: i64) -> usize {
        let before = self.runtime.len();
        self.runtime.retain(|item_id, runtime| {
            if store.find_config(item_id).is_some() {
                return true;
            }
            let last_seen = runtime.seen_at.or(runtime.hydrated_at).unwrap_or(0);
            let has_data = runtime.last_price.is_some()
                || !runtime.history.is_empty()
                || !runtime.fill_history.is_empty()
                || runtime.trend.is_some();
            has_data && now.saturating_sub(last_seen) <= max_idle_ms
        });
        before - self.runtime.len()
    }

    pub fn refresh_trend(&mut self, store: &Store, item_id: &str) -> Option<TrendSignal> {
        let config = store.find_config(item_id)?.clone();
        let now = self.now();
        let desired_qty = config.desired_qty;
        let runtime = self.runtime_for(&config.item_id, desired_qty);
        if runtime.fill_qty_for != desired_qty {
            runtime.fill_history.clear();
            runtime.last_fill = None;
            runtime.fill_qty_for = desired_qty;
        }
        Self::compute_trend(&config, runtime, now, true)
    }

    pub fn prune_pending(&mut self) {
        let now = self.now();
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, meta)| now - meta.at > PENDING_TIMEOUT_MS)
            .map(|(order_id, _)| order_id.clone())
            .collect();
        for order_id in expired {
            self.pending.remove(&order_id);
            self.logger.warn(
                "order",
                format!("no confirmation for {order_id} after 120s; releasing it"),
            );
        }
        self.prune_unmatched(now);
    }

    pub fn pending_snapshot(&self) -> Vec<Value> {
        self.pending
            .iter()
            .map(|(order_id, meta)| {
                json!({
                    "orderId": order_id,
                    "itemId": meta.item_id,
                    "itemType": meta.item_type,
                    "label": meta.label,
                    "qty": meta.qty,
                    "unitPrice": meta.unit_price,
                    "targetPrice": meta.target_price,
                    "at": meta.at,
                    "timings": meta.timings,
                })
            })
            .collect()
    }

    /// Writes one close per target per minute, even when that item's own frames
    /// are quiet.
    ///
    /// The feed is change-driven: an item whose book does not move sends no
    /// frame, which used to read as "no data" on the chart. Once a target has
    /// been seen live in this process, its last price is sampled every minute so
    /// the line stays flat instead of breaking.
    ///
    /// `feed_alive` is false when the upstream itself is down. Nothing is
    /// written then, because a real outage must stay a visible gap rather than
    /// a fabricated flat line.
    pub fn sample_prices(
        &mut self,
        store: &mut Store,
        at: i64,
        feed_alive: bool,
    ) -> Vec<PriceSample> {
        if !feed_alive {
            return Vec::new();
        }
        let bucket = at.div_euclid(PRICE_BUCKET_MS) * PRICE_BUCKET_MS;
        let item_ids: Vec<String> = store
            .state
            .configs
            .iter()
            .map(|config| config.item_id.clone())
            .collect();
        let mut samples = Vec::new();
        let mut finalized = Vec::new();
        for item_id in item_ids {
            let Some(runtime) = self.runtime.get_mut(&item_id.to_ascii_lowercase()) else {
                continue;
            };
            // Never seen by a live frame since boot: sampling would bridge the
            // downtime this process just came out of.
            if runtime.live_seen_at.is_none() {
                continue;
            }
            let Some(last_price) = runtime.last_price else {
                continue;
            };

            // The frame stream may have gone quiet before the minute rolled:
            // finish it here so its stored close is the exact median, then
            // carry the settled level into the new minute.
            if let Some(mut agg) = runtime.minute.take() {
                if agg.bucket < bucket {
                    let old_bucket = agg.bucket;
                    let old_close = agg.close();
                    finalized.push((
                        item_id.clone(),
                        old_bucket,
                        old_close,
                        agg.low,
                        agg.high,
                        agg.raw_low,
                        agg.raw_high,
                    ));
                    if let Some(point) = runtime.history.iter_mut().find(|p| p.t == old_bucket) {
                        point.p = old_close;
                    }
                    let mut next = MinuteAgg::start(bucket, agg.settled, bucket);
                    next.value(at);
                    runtime.minute = Some(next);
                } else if agg.bucket == bucket {
                    runtime.minute = Some(agg);
                } else {
                    let mut next = MinuteAgg::start(bucket, last_price, bucket);
                    next.value(at);
                    runtime.minute = Some(next);
                }
            } else {
                let mut next = MinuteAgg::start(bucket, last_price, bucket);
                next.value(at);
                runtime.minute = Some(next);
            }

            // A frame already owns this minute; the sampler must not overwrite
            // a fresher close with the previous one.
            if matches!(runtime.history.last(), Some(point) if point.t >= bucket) {
                continue;
            }
            let (price, low, high, raw_low, raw_high) = match runtime.minute.as_mut() {
                Some(agg) => {
                    let price = agg.value(at);
                    (price, agg.low, agg.high, agg.raw_low, agg.raw_high)
                }
                None => (last_price, last_price, last_price, last_price, last_price),
            };
            runtime.history.push(Point {
                t: bucket,
                p: price,
            });
            if runtime.history.len() > PRICE_HISTORY_LIMIT {
                let excess = runtime.history.len() - PRICE_HISTORY_LIMIT;
                runtime.history.drain(0..excess);
            }
            samples.push(PriceSample {
                item_id,
                bucket,
                price,
                low,
                high,
                raw_low,
                raw_high,
            });
        }
        let mut rows: Vec<(&str, i64, f64, f64, f64, f64, f64)> = finalized
            .iter()
            .map(
                |(item_id, bucket_at, price, low, high, raw_low, raw_high)| {
                    (
                        item_id.as_str(),
                        *bucket_at,
                        *price,
                        *low,
                        *high,
                        *raw_low,
                        *raw_high,
                    )
                },
            )
            .collect();
        rows.extend(samples.iter().map(|sample| {
            (
                sample.item_id.as_str(),
                sample.bucket,
                sample.price,
                sample.low,
                sample.high,
                sample.raw_low,
                sample.raw_high,
            )
        }));
        if let Err(error) = store.record_price_samples(&rows)
            && at - self.sample_error_at > 60_000
        {
            self.sample_error_at = at;
            self.logger
                .warn("engine", format!("could not record price samples: {error}"));
        }
        samples
    }

    pub fn runtime_snapshot(&self, store: &Store) -> Value {
        let mut out = serde_json::Map::new();
        // Only targets that still exist are exposed. Orphan runtimes are kept
        // for a grace period (see `prune_orphan_runtimes`) but never serialized,
        // so a long delete history no longer costs CPU on every snapshot.
        for config in &store.state.configs {
            let key = config.item_id.to_ascii_lowercase();
            let Some(runtime) = self.runtime.get(&key) else {
                continue;
            };
            let label = label_for(Some(config), &key);
            out.insert(key, runtime_json(runtime, label));
        }
        Value::Object(out)
    }
}

fn runtime_json(runtime: &Runtime, label: String) -> Value {
    let mut value = serde_json::Map::new();
    value.insert("label".into(), json!(label));
    value.insert("lastPrice".into(), json!(runtime.last_price));
    value.insert("lastStock".into(), json!(runtime.last_stock));
    value.insert("lastSeenAt".into(), json!(runtime.last_seen_at));
    value.insert("seenAt".into(), json!(runtime.seen_at));
    value.insert("stale".into(), json!(runtime.stale));
    value.insert("itemType".into(), json!(runtime.item_type));
    value.insert("allowedPriceMin".into(), json!(runtime.allowed_price_min));
    value.insert("allowedPriceMax".into(), json!(runtime.allowed_price_max));
    value.insert("allowedAt".into(), json!(runtime.allowed_at));
    value.insert(
        "history".into(),
        serde_json::to_value(&runtime.history).unwrap_or(Value::Null),
    );
    value.insert(
        "lastFill".into(),
        serde_json::to_value(runtime.last_fill).unwrap_or(Value::Null),
    );
    value.insert(
        "trend".into(),
        serde_json::to_value(&runtime.trend).unwrap_or(Value::Null),
    );
    value.insert("trendAt".into(), json!(runtime.trend_at));
    value.insert("hydratedAt".into(), json!(runtime.hydrated_at));
    value.insert("inflight".into(), json!(runtime.inflight));
    value.insert("orders".into(), json!(runtime.orders));
    value.insert("fills".into(), json!(runtime.fills));
    value.insert("failures".into(), json!(runtime.failures));
    value.insert("lastResult".into(), json!(runtime.last_result));
    value.insert("failureStreak".into(), json!(runtime.failure_streak));
    value.insert("blockedUntil".into(), json!(runtime.blocked_until));
    value.insert(
        "reconcile".into(),
        serde_json::to_value(runtime.reconcile).unwrap_or(Value::Null),
    );
    value.insert("lastLogAt".into(), json!(runtime.last_log_at));
    value.insert("lastOrderAt".into(), json!(runtime.last_order_at));
    value.insert("lastInRangeAt".into(), json!(runtime.last_in_range_at));
    Value::Object(value)
}

struct FailureMeta {
    item_id: String,
    label: String,
    qty: i64,
    /// Count from the fulfillment result event (`fill.count`). This is what's
    /// recorded as `OrderEntry::qty` (Got). For a rejected purchase there is
    /// no result event, so it stays 0.
    got_qty: i64,
    stock_at_trigger: Option<i64>,
    unit_price: f64,
    target_price: f64,
    at: i64,
    timings: Option<Timings>,
    raw_price_event: Option<Value>,
    raw_result_event: Option<Value>,
    /// Exact purchase request body, stored with the loot row so the detail
    /// modal can show what was sent even when no result frame arrived.
    request_payload: Option<Value>,
    /// True for a locally fired follow-up. Only changes how the row reads: the
    /// backoff decision is made by the caller.
    optimistic: bool,
}

/// Server-side helper: the current wall clock.
pub fn wall_clock() -> Clock {
    Arc::new(now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::LOG_BUFFER_SIZE;
    use crate::model::{Mode, Target};
    use crate::validation;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Harness {
        engine: Engine,
        store: Store,
        now: Arc<AtomicI64>,
        dir: std::path::PathBuf,
        logger: Arc<Logger>,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn harness() -> Harness {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rc-rust-engine-{nanos}"));
        // `info` so `log_scan` lines land in the ring buffer the test reads.
        let logger = Arc::new(Logger::new("info"));
        let mut store = Store::open(dir.clone(), logger.clone()).expect("store");
        let target = Target {
            item_id: "6319f7baa8ce530569ed16b9".into(),
            label: "Hashboard".into(),
            item_type: "miner".into(),
            rarity_color: None,
            target_price: 0.1,
            desired_qty: 10,
            high_stock_threshold: 20,
            min_acceptable_qty: 1,
            allow_partial_order: true,
            active: true,
            created_at: None,
            updated_at: None,
        };
        store.upsert_config(target, None);
        let patch = validation::SettingsPatch {
            delay_per_order: Some(0),
            max_orders_per_minute: Some(60),
            ..Default::default()
        };
        store.patch_settings(patch).unwrap();

        let now = Arc::new(AtomicI64::new(1_000_000));
        let clock_now = now.clone();
        let engine = Engine::new(
            Arc::new(move || clock_now.load(Ordering::SeqCst)),
            logger.clone(),
        );
        Harness {
            engine,
            store,
            now,
            dir,
            logger,
        }
    }

    /// The most recent scan lines, joined, so a test can assert on the reason.
    fn scan_summary(harness: &Harness) -> String {
        harness
            .logger
            .recent(LOG_BUFFER_SIZE)
            .iter()
            .filter(|entry| entry["scope"] == "scan")
            .filter_map(|entry| entry["message"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn market_frame(price_raw: i64, quantity: i64) -> Value {
        json!({
            "cmd": "marketplace_orders_update",
            "value": {
                "item_id": "6319F7BAA8CE530569ED16B9",
                "currency": "RLT",
                "item_type": "miner",
                "data": { "list": [{ "price": price_raw, "quantity": quantity }] }
            }
        })
    }

    /// The same frame with the whole-book `totalCount` upstream also sends.
    fn market_frame_with_total(price_raw: i64, quantity: i64, total: i64) -> Value {
        json!({
            "cmd": "marketplace_orders_update",
            "value": {
                "item_id": "6319F7BAA8CE530569ED16B9",
                "currency": "RLT",
                "item_type": "miner",
                "data": {
                    "list": [{ "price": price_raw, "quantity": quantity }],
                    "totalCount": total
                }
            }
        })
    }

    /// Accepts an intent the way the wiring does and returns the settlement.
    fn accept(h: &mut Harness, intent: &OrderIntent, order_id: &str, at: i64) -> Vec<EngineEvent> {
        h.now.store(at, Ordering::SeqCst);
        h.engine.on_purchase_result(
            &mut h.store,
            intent,
            PurchaseOutcome {
                ok: true,
                order_id: Some(order_id.to_string()),
                accepted_at: Some(at),
                ..Default::default()
            },
        )
    }

    fn fill_frame(order_id: &str, count: i64, price_raw: i64, status: i64) -> Value {
        json!({
            "cmd": "marketplace_orders_filled",
            "value": {
                "item_id": "6319f7baa8ce530569ed16b9",
                "order_id": order_id,
                "count": count,
                "price": price_raw,
                "status": status
            }
        })
    }

    fn submissions(events: &[EngineEvent]) -> Vec<&OrderIntent> {
        events
            .iter()
            .filter_map(|event| match event {
                EngineEvent::Submit(intent) => Some(intent.as_ref()),
                _ => None,
            })
            .collect()
    }

    /// Market ticks, serialised the way the control plane ships them.
    fn market_payloads(events: &[EngineEvent]) -> Vec<Value> {
        events
            .iter()
            .filter_map(|event| match event {
                EngineEvent::Market(tick) => serde_json::to_value(tick.as_ref()).ok(),
                _ => None,
            })
            .collect()
    }

    /// The item monitor must rank every id the RLT market shows, not only the
    /// ones on the target fleet: the whole point of the table is the discovery.
    #[test]
    fn counts_market_frames_for_ids_outside_the_target_fleet() {
        let mut h = harness();
        let stranger = json!({
            "cmd": "marketplace_orders_update",
            "value": {
                "item_id": "65aa0001d3e4f5061728394a",
                "currency": "RLT",
                "item_type": "rack",
                "data": { "list": [{ "price": 250_000, "quantity": 3 }] }
            }
        });
        // No target matches, so the engine returns early — the frame still has
        // to be counted.
        let events = h
            .engine
            .handle_frame(&mut h.store, &stranger, 1_000_100, true);
        assert!(events.is_empty(), "a stranger must not produce a tick");

        // A non-RLT marketplace is a different economy; counting it would rank
        // ids the RLT monitor can never serve.
        let other_currency = json!({
            "cmd": "marketplace_orders_update",
            "value": {
                "item_id": "65aa0002d3e4f5061728394b",
                "currency": "USDT",
                "data": { "list": [{ "price": 250_000, "quantity": 3 }] }
            }
        });
        h.engine
            .handle_frame(&mut h.store, &other_currency, 1_000_100, true);

        // The fleet target is counted too, with its type picked up from the
        // frame so the catalogue can be filtered by product category.
        h.engine
            .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_100, true);

        let report = h
            .store
            .item_activity(&crate::store::ItemActivityQuery::default(), 1_000_100);
        let ids: Vec<&str> = report.rows.iter().map(|row| row.item_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["6319f7baa8ce530569ed16b9", "65aa0001d3e4f5061728394a"]
        );
        assert_eq!(report.rows[0].item_type, "miner");
        assert_eq!(report.rows[1].item_type, "rack");
        // The catalogue now lists both, which is what lets the monitor name
        // them later without another frame.
        assert!(h.store.catalog_entry("65aa0001d3e4f5061728394a").is_some());
        assert!(h.store.find_config("65aa0001d3e4f5061728394a").is_none());
    }

    #[test]
    fn market_ticks_are_skipped_while_no_deck_is_connected() {
        let mut h = harness();
        h.engine.set_broadcast_enabled(false);
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        assert!(
            market_payloads(&events).is_empty(),
            "a headless worker must not build market payloads"
        );
        // The order decision itself must be untouched.
        assert_eq!(submissions(&events).len(), 1);

        h.engine.set_broadcast_enabled(true);
        let resumed =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_002, true);
        assert_eq!(market_payloads(&resumed).len(), 1);
    }

    #[test]
    fn orphan_runtimes_are_hidden_and_released_after_the_grace_period() {
        let mut h = harness();
        {
            let runtime = h.engine.runtime_for("deleted-item", 1);
            runtime.seen_at = Some(1_000_000);
            runtime.history.push(Point {
                t: 1_000_000,
                p: 0.5,
            });
        }
        assert!(
            h.engine
                .runtime_snapshot(&h.store)
                .get("deleted-item")
                .is_none(),
            "an orphan must never be serialized into a snapshot"
        );

        // Within the grace period the runtime is kept (delete + re-add shows
        // the same chart), after it the entry is released.
        assert_eq!(
            h.engine.prune_orphan_runtimes(&h.store, 1_000_000, 60_000),
            0
        );
        assert_eq!(
            h.engine.prune_orphan_runtimes(&h.store, 1_200_000, 60_000),
            1
        );
    }

    #[test]
    fn market_broadcast_keeps_its_wire_shape() {
        let mut h = harness();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let mut market = market_payloads(&events);
        let market = market.remove(0);

        assert_eq!(market["itemId"], "6319f7baa8ce530569ed16b9");
        assert_eq!(market["currency"], "RLT");
        assert_eq!(market["itemType"], "miner");
        assert_eq!(market["stock"], 10);
        assert!((market["close"].as_f64().unwrap() - 0.09).abs() < 1e-12);
        assert_eq!(market["hot"], true);
        assert_eq!(market["mode"], "live");
        assert_eq!(market["bucket"], 960_000);
        // The trend is embedded as JSON, not as a stringified payload. The
        // pre-serialised `RawValue` must not survive `to_value` as a string.
        assert!(
            market["trend"].is_object(),
            "trend must stay an object, got {:?}",
            market["trend"]
        );
        assert!(market["trend"]["status"].is_string());
        assert!(market["fill"].is_object(), "fill: {:?}", market["fill"]);
    }

    #[test]
    fn submits_an_order_when_the_price_is_below_target() {
        let mut h = harness();
        let frame = market_frame(90_000, 10);
        let events = h.engine.handle_frame(&mut h.store, &frame, 1_000_001, true);

        let intents = submissions(&events);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].qty, 10);
        assert!((intents[0].unit_price - 0.09).abs() < 1e-12);
        assert_eq!(h.store.state.stats.orders_sent, 1);
        assert!(
            h.engine
                .runtime_snapshot(&h.store)
                .get("6319f7baa8ce530569ed16b9")
                .is_some()
        );
        // A market tick must be emitted before the submit intent.
        assert_eq!(market_payloads(&events).len(), 1);
    }

    #[test]
    fn off_mode_blocks_submission_but_keeps_the_market_tick() {
        let mut h = harness();
        h.store
            .patch_settings(validation::SettingsPatch {
                mode: Some(Mode::Off),
                ..Default::default()
            })
            .unwrap();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        assert!(submissions(&events).is_empty());
        assert_eq!(market_payloads(&events).len(), 1);
        // No budget is spent while off: a later live order is not delayed.
        assert_eq!(h.engine.order_stamps_len(), 0);
    }

    /// While off the log and the `hot` flag must still describe the real plan,
    /// not a blanket "paused". A frame that is genuinely too thin has to say so.
    #[test]
    fn off_mode_still_reports_the_real_reason() {
        let mut h = harness();
        let mut target = h.store.state.configs[0].clone();
        target.min_acceptable_qty = 5;
        h.store.upsert_config(target, None);
        h.store
            .patch_settings(validation::SettingsPatch {
                mode: Some(Mode::Off),
                ..Default::default()
            })
            .unwrap();

        // Price is inside the ceiling, but 2 units is below the minimum of 5.
        // The honest reason is `stock-below-minimum`, never `paused`.
        let _ = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 2), 1_000_001, true);
        let summary = scan_summary(&h);
        assert!(
            summary.contains("stock-below-minimum"),
            "unexpected scan log: {summary:?}"
        );
        assert!(
            !summary.contains("paused"),
            "off must not fake the reason: {summary:?}"
        );

        // A hot frame still advertises hotness even though nothing is submitted.
        let mut target = h.store.state.configs[0].clone();
        target.min_acceptable_qty = 1;
        h.store.upsert_config(target, None);
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_002, true);
        assert!(submissions(&events).is_empty());
        assert!(
            market_payloads(&events)
                .iter()
                .any(|data| data["hot"] == true)
        );
        assert_eq!(h.engine.order_stamps_len(), 0, "off spends no budget");
    }

    #[test]
    fn shadow_mode_broadcasts_the_plan_without_submitting() {
        let mut h = harness();
        h.store
            .patch_settings(validation::SettingsPatch {
                mode: Some(Mode::Shadow),
                ..Default::default()
            })
            .unwrap();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        assert!(submissions(&events).is_empty());
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "dry"
        )));
        // Shadow spends the budget on purpose so it measures live throughput.
        assert_eq!(h.engine.order_stamps_len(), 1);
    }

    #[test]
    fn shadow_mode_needs_no_session_but_live_mode_does() {
        let mut h = harness();
        h.store
            .patch_settings(validation::SettingsPatch {
                mode: Some(Mode::Shadow),
                ..Default::default()
            })
            .unwrap();
        let shadow =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, false);
        assert!(shadow.iter().any(|event| matches!(
            event,
            EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "dry"
        )));

        h.store
            .patch_settings(validation::SettingsPatch {
                mode: Some(Mode::Live),
                ..Default::default()
            })
            .unwrap();
        h.now.store(1_000_500, Ordering::SeqCst);
        let live = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_500, false);
        assert!(
            submissions(&live).is_empty(),
            "live must not submit without a linked session"
        );
    }

    #[test]
    fn live_without_a_session_does_not_spend_the_rate_budget() {
        let mut h = harness();
        h.store
            .patch_settings(validation::SettingsPatch {
                mode: Some(Mode::Live),
                ..Default::default()
            })
            .unwrap();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, false);
        assert!(submissions(&events).is_empty());
        assert_eq!(
            h.engine.order_stamps_len(),
            0,
            "a missing session must not burn the per-minute budget"
        );
    }

    #[test]
    fn cooldown_blocks_a_second_order_inside_the_delay_window() {
        let mut h = harness();
        h.store
            .patch_settings(validation::SettingsPatch {
                delay_per_order: Some(5_000),
                ..Default::default()
            })
            .unwrap();
        let first = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        assert_eq!(submissions(&first).len(), 1);

        h.now.store(1_002_000, Ordering::SeqCst);
        let second =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_002_000, true);
        assert!(submissions(&second).is_empty());
    }

    #[test]
    fn accepted_purchase_then_fill_records_history_and_stats() {
        let mut h = harness();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();

        h.now.store(1_000_120, Ordering::SeqCst);
        let accepted = h.engine.on_purchase_result(
            &mut h.store,
            &intent,
            PurchaseOutcome {
                ok: true,
                order_id: Some("order-1".into()),
                accepted_at: Some(1_000_120),
                ..Default::default()
            },
        );
        assert!(accepted.iter().any(|event| matches!(
            event,
            EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "accepted"
        )));
        assert_eq!(h.engine.pending_snapshot().len(), 1);

        h.now.store(1_000_400, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 10, 900_000, ORDER_STATUS_FILLED),
            1_000_400,
            true,
        );
        assert!(fill.iter().any(|event| matches!(
            event,
            EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "filled"
        )));

        assert_eq!(h.store.state.stats.filled, 1);
        assert_eq!(h.store.state.stats.bought, 10);
        assert!((h.store.state.stats.spent_rlt - 0.9).abs() < 1e-9);
        assert!(h.store.has_order("order-1"));
        assert!(h.engine.pending_snapshot().is_empty());
    }

    #[test]
    fn purchase_request_payload_is_kept_on_the_filled_row() {
        let mut h = harness();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        let payload = json!({
            "challenge": "",
            "action": "marketplace",
            "itemId": "6319f7baa8ce530569ed16b9",
            "itemType": "mutation_component",
            "totalCount": 10,
            "currency": "RLT",
            "totalPrice": 900_000.0
        });

        h.now.store(1_000_120, Ordering::SeqCst);
        h.engine.on_purchase_result(
            &mut h.store,
            &intent,
            PurchaseOutcome {
                ok: true,
                order_id: Some("order-payload".into()),
                accepted_at: Some(1_000_120),
                request_payload: Some(payload.clone()),
                ..Default::default()
            },
        );

        h.now.store(1_000_400, Ordering::SeqCst);
        h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-payload", 10, 900_000, ORDER_STATUS_FILLED),
            1_000_400,
            true,
        );

        let entry = h.store.state.history.first().expect("fill row");
        let raw = entry.raw_events.as_ref().expect("raw events");
        assert_eq!(raw["purchaseRequest"], payload);
    }

    #[test]
    fn purchase_request_payload_is_kept_on_the_rejected_row() {
        let mut h = harness();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        let payload = json!({
            "action": "marketplace",
            "itemId": "6319f7baa8ce530569ed16b9",
            "totalCount": 10,
            "currency": "RLT",
            "totalPrice": 900_000.0
        });

        h.now.store(1_000_200, Ordering::SeqCst);
        h.engine.on_purchase_result(
            &mut h.store,
            &intent,
            PurchaseOutcome {
                ok: false,
                error: Some("marketplace rejected the order".into()),
                request_payload: Some(payload.clone()),
                ..Default::default()
            },
        );

        let entry = h.store.state.history.first().expect("failure row");
        let raw = entry.raw_events.as_ref().expect("raw events");
        assert_eq!(raw["purchaseRequest"], payload);
    }

    #[test]
    fn purchase_outcome_carries_the_http_marks_into_the_timeline() {
        let mut h = harness();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        // The engine wrote a provisional sendAt; the HTTP mark must win.
        assert!(intent.timings.send_at.is_some());

        h.now.store(1_000_120, Ordering::SeqCst);
        let accepted = h.engine.on_purchase_result(
            &mut h.store,
            &intent,
            PurchaseOutcome {
                ok: true,
                order_id: Some("order-ttfb".into()),
                send_at: Some(1_000_050),
                response_at: Some(1_000_110),
                complete_at: Some(1_000_111),
                ttfb_ms: Some(60.5),
                accepted_at: Some(1_000_120),
                ..Default::default()
            },
        );

        let timings = accepted
            .iter()
            .find_map(|event| match event {
                EngineEvent::Broadcast {
                    kind: "order",
                    data,
                } if data["phase"] == "accepted" => Some(data["timings"].clone()),
                _ => None,
            })
            .expect("accepted broadcast carries timings");
        assert_eq!(timings["sendAt"], 1_000_050.0);
        assert_eq!(timings["responseAt"], 1_000_110.0);
        assert_eq!(timings["ttfbMs"], 60.5);
    }

    #[test]
    fn rejected_purchase_records_a_failure_and_backs_off() {
        let mut h = harness();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();

        h.now.store(1_000_200, Ordering::SeqCst);
        let rejected = h.engine.on_purchase_result(
            &mut h.store,
            &intent,
            PurchaseOutcome {
                ok: false,
                error: Some("marketplace rejected the order".into()),
                ..Default::default()
            },
        );
        assert!(rejected.iter().any(|event| matches!(
            event,
            EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "failed"
        )));
        assert_eq!(h.store.state.stats.failed, 1);

        // Backoff blocks the next frame inside the 5 s window.
        h.now.store(1_002_000, Ordering::SeqCst);
        let blocked =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_002_000, true);
        assert!(submissions(&blocked).is_empty());

        // After the window it may try again.
        h.now.store(1_006_500, Ordering::SeqCst);
        let retry = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_006_500, true);
        assert_eq!(submissions(&retry).len(), 1);
    }

    #[test]
    fn no_items_failure_keeps_the_attempted_order_details() {
        let mut h = harness();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();

        h.now.store(1_000_120, Ordering::SeqCst);
        h.engine.on_purchase_result(
            &mut h.store,
            &intent,
            PurchaseOutcome {
                ok: true,
                order_id: Some("order-empty".into()),
                accepted_at: Some(1_000_120),
                ..Default::default()
            },
        );

        h.now.store(1_000_400, Ordering::SeqCst);
        let failed = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-empty", 0, 0, ORDER_STATUS_FAILED),
            1_000_400,
            true,
        );
        assert!(failed.iter().any(|event| matches!(
            event,
            EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "failed"
        )));

        let entry = h.store.state.history.first().expect("failure row");
        assert_eq!(entry.status, "failed");
        // Got = fulfillment count (0 for failed); Sent keeps the attempted qty.
        assert_eq!(entry.qty, 0);
        assert_eq!(entry.requested_qty, Some(intent.qty));
        assert!((entry.unit_price - intent.unit_price).abs() < 1e-12);
        assert!((entry.target_price - intent.target_price).abs() < 1e-12);
        // `latencyMs` spans dispatch → result, not the instant the failure row
        // was written (the Bun engine used the pending order's `at`).
        assert_eq!(entry.latency_ms, Some(1_000_400 - intent.at));
    }

    #[test]
    fn duplicate_fill_confirmation_is_ignored() {
        let mut h = harness();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        h.engine.on_purchase_result(
            &mut h.store,
            &intent,
            PurchaseOutcome {
                ok: true,
                order_id: Some("order-9".into()),
                ..Default::default()
            },
        );
        h.now.store(1_000_500, Ordering::SeqCst);
        h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-9", 5, 450_000, ORDER_STATUS_FILLED),
            1_000_500,
            true,
        );
        let before = h.store.state.stats.filled;

        h.now.store(1_000_600, Ordering::SeqCst);
        let duplicate = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-9", 5, 450_000, ORDER_STATUS_FILLED),
            1_000_600,
            true,
        );
        assert!(duplicate.is_empty());
        assert_eq!(h.store.state.stats.filled, before);
    }

    #[test]
    fn an_early_result_is_replayed_once_the_purchase_is_accepted() {
        let mut h = harness();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();

        // The result frame arrives before the HTTP response.
        h.now.store(1_000_050, Ordering::SeqCst);
        let early = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-early", 10, 900_000, ORDER_STATUS_FILLED),
            1_000_050,
            true,
        );
        assert!(
            early.is_empty(),
            "no pending order yet, so it must be buffered"
        );

        h.now.store(1_000_120, Ordering::SeqCst);
        let accepted = h.engine.on_purchase_result(
            &mut h.store,
            &intent,
            PurchaseOutcome {
                ok: true,
                order_id: Some("order-early".into()),
                ..Default::default()
            },
        );
        assert!(accepted.iter().any(|event| matches!(
            event,
            EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "filled"
        )));
        assert!(h.store.has_order("order-early"));
    }

    #[test]
    fn hydrate_restores_the_snapshot_and_runs_the_model() {
        let mut h = harness();
        let frame = market_frame(90_000, 10);
        h.engine.handle_frame(&mut h.store, &frame, 1_000_001, true);
        h.engine.flush_market_cache(&h.store);

        let mut fresh = Engine::new(Arc::new(|| 1_000_010), Arc::new(Logger::new("error")));
        let restored = fresh.hydrate_market(&h.store);
        assert_eq!(restored, 1);
        let snapshot = fresh.runtime_snapshot(&h.store);
        let runtime = &snapshot["6319f7baa8ce530569ed16b9"];
        assert!((runtime["lastPrice"].as_f64().unwrap() - 0.09).abs() < 1e-12);
        assert_eq!(runtime["stale"], true);
    }

    #[test]
    fn samples_quiet_minutes_from_the_last_live_close() {
        let mut h = harness();
        let bucket = 1_000_000_i64.div_euclid(60_000) * 60_000;

        // A target the feed has never seen live must not be sampled: that would
        // bridge the downtime this process just came out of.
        assert!(
            h.engine
                .sample_prices(&mut h.store, bucket + 60_000, true)
                .is_empty()
        );

        h.engine
            .handle_frame(&mut h.store, &market_frame(90_000, 4), 1_000_001, true);

        // The frame already owns its own minute, so there is nothing to add.
        assert!(
            h.engine
                .sample_prices(&mut h.store, bucket + 15_000, true)
                .is_empty()
        );

        // The next minute is quiet: the last live close is carried forward.
        let samples = h.engine.sample_prices(&mut h.store, bucket + 60_000, true);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].bucket, bucket + 60_000);
        assert!((samples[0].price - 0.09).abs() < 1e-12);
        let stored = h.store.price_series("6319f7baa8ce530569ed16b9", 0);
        assert_eq!(stored.len(), 2, "frame close + sampled close");

        // A feed that is not alive writes nothing at all.
        assert!(
            h.engine
                .sample_prices(&mut h.store, bucket + 120_000, false)
                .is_empty()
        );
        assert_eq!(h.store.price_series("6319f7baa8ce530569ed16b9", 0).len(), 2);
    }

    fn stored_point(h: &Harness, item_id: &str, bucket: i64) -> crate::model::PriceSeriesPoint {
        h.store
            .price_series(item_id, 0)
            .into_iter()
            .find(|point| point.bucket_at == bucket)
            .expect("stored point for the bucket")
    }

    fn stored_close(h: &Harness, item_id: &str, bucket: i64) -> f64 {
        stored_point(h, item_id, bucket).price
    }

    #[test]
    fn a_transient_bait_price_never_reaches_the_chart() {
        let mut h = harness();
        // 1_000_000 ms falls inside this minute.
        let bucket = 960_000_i64;
        let item = "6319f7baa8ce530569ed16b9";

        // The real level holds, then a one-unit bait ask flashes for 2 s.
        h.now.store(bucket + 1_000, Ordering::SeqCst);
        h.engine.handle_frame(
            &mut h.store,
            &market_frame(100_000, 4),
            bucket + 1_000,
            true,
        );
        h.now.store(bucket + 2_000, Ordering::SeqCst);
        h.engine
            .handle_frame(&mut h.store, &market_frame(10_000, 1), bucket + 2_000, true);
        h.now.store(bucket + 4_000, Ordering::SeqCst);
        h.engine.handle_frame(
            &mut h.store,
            &market_frame(100_000, 4),
            bucket + 4_000,
            true,
        );

        // The live line never took the bait either.
        let snapshot = h.engine.runtime_snapshot(&h.store);
        let history = snapshot[item]["history"].as_array().unwrap();
        let last = history.last().unwrap()["p"].as_f64().unwrap();
        assert!((last - 0.10).abs() < 1e-12, "live line showed {last}");

        // Rolling the minute stores the sustained level as the close.
        h.now.store(bucket + 60_001, Ordering::SeqCst);
        h.engine.handle_frame(
            &mut h.store,
            &market_frame(100_000, 4),
            bucket + 60_001,
            true,
        );
        assert!((stored_close(&h, item, bucket) - 0.10).abs() < 1e-12);
        let point = stored_point(&h, item, bucket);
        // The held band ignores the 2 s bait; the raw band keeps it.
        assert_eq!((point.low, point.high), (Some(0.10), Some(0.10)));
        assert_eq!(
            (point.raw_low, point.raw_high),
            (Some(0.01), Some(0.10)),
            "the raw band must expose the print the dwell guard rejected"
        );
    }

    #[test]
    fn a_spike_that_outlives_the_dwell_but_not_half_the_minute_loses() {
        let mut h = harness();
        let bucket = 960_000_i64;
        let item = "6319f7baa8ce530569ed16b9";
        let frame = |h: &mut Harness, at: i64, price: i64| {
            h.now.store(at, Ordering::SeqCst);
            h.engine
                .handle_frame(&mut h.store, &market_frame(price, 4), at, true);
        };

        frame(&mut h, bucket + 1_000, 100_000);
        frame(&mut h, bucket + 20_000, 100_000);
        frame(&mut h, bucket + 20_001, 10_000);
        // Passes the 3 s dwell, but only rules for 5 s of a 60 s minute.
        frame(&mut h, bucket + 25_001, 10_000);
        frame(&mut h, bucket + 30_001, 100_000);
        frame(&mut h, bucket + 35_001, 100_000);
        frame(&mut h, bucket + 60_001, 100_000);

        assert!(
            (stored_close(&h, item, bucket) - 0.10).abs() < 1e-12,
            "a 10 s wick must not own a 60 s close"
        );
        // The admitted wick still belongs in the band, which is what the
        // expanded chart draws as its low/high column.
        let point = stored_point(&h, item, bucket);
        assert_eq!((point.low, point.high), (Some(0.01), Some(0.10)));
    }

    #[test]
    fn a_print_that_lasts_past_the_dwell_enters_the_band() {
        let mut h = harness();
        let bucket = 960_000_i64;
        let item = "6319f7baa8ce530569ed16b9";
        let frame = |h: &mut Harness, at: i64, price: i64| {
            h.now.store(at, Ordering::SeqCst);
            h.engine
                .handle_frame(&mut h.store, &market_frame(price, 4), at, true);
        };

        frame(&mut h, bucket + 1_000, 100_000);
        frame(&mut h, bucket + 10_000, 100_000);
        // The cheap print appears, then is still there 3.5 s later: it is real
        // enough to join the band even though it is too short to own the close.
        frame(&mut h, bucket + 20_000, 60_000);
        frame(&mut h, bucket + 23_500, 60_000);
        frame(&mut h, bucket + 24_000, 100_000);
        frame(&mut h, bucket + 29_000, 100_000);
        frame(&mut h, bucket + 60_001, 100_000);

        assert!((stored_close(&h, item, bucket) - 0.10).abs() < 1e-12);
        let point = stored_point(&h, item, bucket);
        assert_eq!((point.low, point.high), (Some(0.06), Some(0.10)));
    }

    #[test]
    fn a_sustained_move_wins_the_minute_close() {
        let mut h = harness();
        let bucket = 960_000_i64;
        let item = "6319f7baa8ce530569ed16b9";
        let frame = |h: &mut Harness, at: i64, price: i64| {
            h.now.store(at, Ordering::SeqCst);
            h.engine
                .handle_frame(&mut h.store, &market_frame(price, 4), at, true);
        };

        frame(&mut h, bucket + 1_000, 100_000);
        frame(&mut h, bucket + 10_000, 80_000);
        frame(&mut h, bucket + 15_000, 80_000);
        // The new level holds for the remaining ~45 s.
        frame(&mut h, bucket + 59_000, 80_000);
        frame(&mut h, bucket + 60_001, 80_000);

        assert!(
            (stored_close(&h, item, bucket) - 0.08).abs() < 1e-12,
            "a sustained move must still read as a move"
        );
        let point = stored_point(&h, item, bucket);
        assert_eq!((point.low, point.high), (Some(0.08), Some(0.10)));
    }

    // ── partial fills and local reconciliation ──────────────────────────────

    /// Drives one order of `qty` through acceptance and a partial fill of
    /// `got`, returning the follow-up intents the engine fired on its own.
    fn partial_fill(
        h: &mut Harness,
        qty: i64,
        got: i64,
        order_id: &str,
        base: i64,
    ) -> Vec<OrderIntent> {
        h.now.store(base, Ordering::SeqCst);
        let events = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, qty), base, true);
        let intent = submissions(&events)[0].clone();
        assert_eq!(intent.qty, qty);

        accept(h, &intent, order_id, base + 120);
        h.now.store(base + 400, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame(order_id, got, 90_000 * got, ORDER_STATUS_FILLED),
            base + 400,
            true,
        );
        submissions(&fill).into_iter().cloned().collect()
    }

    #[test]
    fn a_partial_fill_rearms_the_remainder_without_waiting_for_a_frame() {
        let mut h = harness();
        let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001);

        assert_eq!(
            follow.len(),
            1,
            "the remainder must be fired from the fill, not the next frame"
        );
        assert_eq!(follow[0].qty, 1);
        assert!(follow[0].optimistic);
        assert_eq!(follow[0].stock_at_trigger, Some(1));
        assert_eq!(
            follow[0].timings.signal_cmd.as_deref(),
            Some("marketplace_orders_filled")
        );
        assert_eq!(
            follow[0].timings.signal_at,
            Some(1_000_401.0),
            "the fill frame is the signal, so the timeline must start there"
        );
        assert_eq!(follow[0].item_id, "6319f7baa8ce530569ed16b9");
        assert_eq!(h.store.state.stats.orders_sent, 2);

        // The follow-up itself fills: the chain stops and the loot row says the
        // order was a local guess rather than a frame-driven one.
        accept(&mut h, &follow[0], "order-2", 1_000_450);
        h.now.store(1_000_700, Ordering::SeqCst);
        let done = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-2", 1, 90_000, ORDER_STATUS_FILLED),
            1_000_700,
            true,
        );
        assert!(submissions(&done).is_empty(), "nothing is left to chase");
        let entry = h.store.state.history.first().expect("filled row");
        assert_eq!(entry.status, "filled");
        assert_eq!(entry.note.as_deref(), Some("optimistic follow-up"));
    }

    #[test]
    fn a_full_fill_does_not_rearm() {
        let mut h = harness();
        let follow = partial_fill(&mut h, 3, 3, "order-1", 1_000_001);
        assert!(follow.is_empty(), "nothing is left to chase");
    }

    /// `sent < ready`: the request never covered the best level, so the units a
    /// frame would re-advertise one WS hop later are bought from the result
    /// itself. The old rule only fired on `got < sent`, so this level was
    /// abandoned until the next frame.
    #[test]
    fn a_full_fill_with_stock_left_rearms_early() {
        let mut h = harness();
        {
            let mut target = h.store.state.configs[0].clone();
            target.desired_qty = 3;
            h.store.upsert_config(target, None);
        }
        h.now.store(1_000_001, Ordering::SeqCst);
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        assert_eq!(
            intent.qty, 3,
            "the request intentionally stops below the book"
        );

        accept(&mut h, &intent, "order-1", 1_000_120);
        h.now.store(1_000_400, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 3, 270_000, ORDER_STATUS_FILLED),
            1_000_400,
            true,
        );

        // `got == sent`, so there is no shortfall at all: the trigger is the
        // stock the level still shows, exactly like a fresh frame would see it.
        let follow = submissions(&fill);
        assert_eq!(follow.len(), 1, "a deep level is not abandoned for a frame");
        assert_eq!(follow[0].qty, 3);
        assert!(follow[0].optimistic);
        assert_eq!(follow[0].stock_at_trigger, Some(7));

        // The deck learns the settled book from the same event, not later.
        let broadcast = fill
            .iter()
            .find_map(|event| match event {
                EngineEvent::Broadcast {
                    kind: "order",
                    data,
                } if data["phase"] == "filled" => Some(data.clone()),
                _ => None,
            })
            .expect("filled broadcast");
        assert_eq!(broadcast["stockAfter"], 7);
    }

    #[test]
    fn a_submitted_order_protects_its_units_before_the_fill_lands() {
        let mut h = harness();
        h.now.store(1_000_001, Ordering::SeqCst);
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        accept(&mut h, &intent, "order-1", 1_000_120);

        // A frame generated before upstream applied the request still shows the
        // pre-purchase book. Ordering from it would commit ten units that the
        // request already took.
        h.now.store(1_000_200, Ordering::SeqCst);
        let stale = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_200, true);
        assert!(submissions(&stale).is_empty());
        let tick = market_payloads(&stale).remove(0);
        assert_eq!(tick["stock"], 10, "the deck keeps the raw upstream depth");
        assert_eq!(tick["effectiveStock"], 0);
        assert_eq!(tick["reconciling"], true);
    }

    /// The bug this guards: the early update used to rebuild its remaining
    /// quantity from the trigger frame, so a frame or a follow-up that already
    /// spent the leftover was handed it a second time when the result landed.
    #[test]
    fn the_early_update_never_spends_the_leftover_twice() {
        let mut h = harness();
        {
            let mut target = h.store.state.configs[0].clone();
            target.desired_qty = 3;
            h.store.upsert_config(target, None);
        }
        h.now.store(1_000_001, Ordering::SeqCst);
        let events = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        accept(&mut h, &intent, "order-1", 1_000_120);

        // A frame lands before the result and already shows the level down to
        // the one unit the fill will leave. The request has those units
        // committed, so it must not buy them.
        h.now.store(1_000_200, Ordering::SeqCst);
        let caught_up =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 1), 1_000_200, true);
        assert!(submissions(&caught_up).is_empty());

        // The real event lands: two of three filled, one unit is known left.
        h.now.store(1_000_400, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
            1_000_400,
            true,
        );
        let follow = submissions(&fill);
        assert_eq!(follow.len(), 1);
        assert_eq!(follow[0].qty, 1, "only the unit the fill left behind");
        let follow = follow[0].clone();
        accept(&mut h, &follow, "order-2", 1_000_500);

        // A stale frame repeats the same depth. The follow-up owns that unit,
        // so no second order may be fired for it.
        h.now.store(1_000_600, Ordering::SeqCst);
        let stale = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 1), 1_000_600, true);
        assert!(submissions(&stale).is_empty());
        assert_eq!(h.store.state.stats.orders_sent, 2);
    }

    #[test]
    fn a_late_fill_does_not_reopen_the_window() {
        let mut h = harness();
        h.now.store(1_000_001, Ordering::SeqCst);
        let events = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        accept(&mut h, &intent, "order-1", 1_000_120);

        // The result arrives after the local window expired: the feed has been
        // authoritative for a while, so the old trigger must not be revived.
        let late = 1_000_120 + RECONCILE_WINDOW_MS + 1;
        h.now.store(late, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
            late,
            true,
        );
        assert!(submissions(&fill).is_empty(), "a late fill cannot rearm");
        let snapshot = h.engine.runtime_snapshot(&h.store);
        assert!(snapshot["6319f7baa8ce530569ed16b9"]["reconcile"].is_null());

        // A fresh frame is trusted and can trade the unit that is left.
        h.now.store(late + 100, Ordering::SeqCst);
        let fresh = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 1), late + 100, true);
        let tick = market_payloads(&fresh).remove(0);
        assert_eq!(tick["effectiveStock"], 1);
        assert_eq!(tick["reconciling"], false);
        assert_eq!(submissions(&fresh).len(), 1);
    }

    #[test]
    fn a_failed_fill_gives_its_reservation_back() {
        let mut h = harness();
        h.now.store(1_000_001, Ordering::SeqCst);
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        accept(&mut h, &intent, "order-1", 1_000_120);

        h.now.store(1_000_400, Ordering::SeqCst);
        let failed = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 0, 0, ORDER_STATUS_FAILED),
            1_000_400,
            true,
        );
        assert!(submissions(&failed).is_empty());
        let snapshot = h.engine.runtime_snapshot(&h.store);
        assert_eq!(
            snapshot["6319f7baa8ce530569ed16b9"]["reconcile"]["floor"], 10,
            "nothing was obtained, so the whole request returns to the book"
        );

        // The next frame shows the level intact; because the floor is back at
        // the pre-order depth the frame wins and the missed level is retryable.
        h.now.store(1_000_500, Ordering::SeqCst);
        let resumed =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_500, true);
        assert_eq!(market_payloads(&resumed)[0]["reconciling"], false);
        assert_eq!(submissions(&resumed).len(), 1);
    }

    #[test]
    fn an_over_fill_cannot_inflate_the_book() {
        let mut h = harness();
        h.now.store(1_000_001, Ordering::SeqCst);
        let events = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        accept(&mut h, &intent, "order-1", 1_000_120);

        // Upstream reports more units than were requested. The ledger must clamp
        // instead of going negative or offering a phantom remainder.
        h.now.store(1_000_400, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 5, 450_000, ORDER_STATUS_FILLED),
            1_000_400,
            true,
        );
        assert!(submissions(&fill).is_empty());
        let snapshot = h.engine.runtime_snapshot(&h.store);
        assert_eq!(
            snapshot["6319f7baa8ce530569ed16b9"]["reconcile"]["floor"],
            0
        );
    }

    #[test]
    fn a_stale_frame_cannot_resurrect_stock_a_fill_already_consumed() {
        let mut h = harness();
        let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001)[0].clone();
        assert_eq!(follow.qty, 1);
        // Accept the follow-up so `inflight` is no longer what blocks the frame.
        accept(&mut h, &follow, "order-2", 1_000_550);

        // A frame generated before our purchase was applied still shows the
        // original three units. Trusting it would buy goods that are gone.
        h.now.store(1_000_600, Ordering::SeqCst);
        let stale = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_600, true);
        assert!(
            submissions(&stale).is_empty(),
            "the stale frame must not re-order stock the fill already consumed"
        );

        let tick = market_payloads(&stale).remove(0);
        assert_eq!(tick["stock"], 3, "the deck keeps the raw upstream depth");
        assert_eq!(
            tick["effectiveStock"], 0,
            "the decision stock already spent both units it saw"
        );
        assert_eq!(tick["reconciling"], true);

        // The raw runtime view is untouched too: it is upstream's number.
        let snapshot = h.engine.runtime_snapshot(&h.store);
        assert_eq!(
            snapshot["6319f7baa8ce530569ed16b9"]["lastStock"], 3,
            "persisted/displayed stock must stay honest"
        );
    }

    #[test]
    fn a_frame_that_shows_at_most_the_floor_closes_the_window_early() {
        let mut h = harness();
        {
            let mut target = h.store.state.configs[0].clone();
            target.min_acceptable_qty = 2;
            h.store.upsert_config(target, None);
        }

        // 3 on the book, 2 bought: the 1-unit remainder is below the minimum,
        // so the follow-up is (correctly) not fired and the floor stays open.
        let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001);
        assert!(follow.is_empty());

        h.now.store(1_000_500, Ordering::SeqCst);
        let caught_up =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 1), 1_000_500, true);
        let snapshot = h.engine.runtime_snapshot(&h.store);
        assert!(
            snapshot["6319f7baa8ce530569ed16b9"]["reconcile"].is_null(),
            "a frame at or below the floor cannot be a pre-fill snapshot"
        );
        let tick = market_payloads(&caught_up).remove(0);
        assert_eq!(tick["stock"], 1);
        assert_eq!(tick["effectiveStock"], 1);
        assert_eq!(tick["reconciling"], false);
    }

    #[test]
    fn an_expired_window_lets_the_frame_win_again() {
        let mut h = harness();
        {
            let mut target = h.store.state.configs[0].clone();
            target.min_acceptable_qty = 2;
            h.store.upsert_config(target, None);
        }
        let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001);
        assert!(follow.is_empty());

        // Beyond the window the next frame is assumed fresh: a genuine restock
        // must not stay invisible forever.
        let at = 1_000_400 + RECONCILE_WINDOW_MS + 1;
        h.now.store(at, Ordering::SeqCst);
        let fresh = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), at, true);
        let tick = market_payloads(&fresh).remove(0);
        assert_eq!(tick["effectiveStock"], 3);
        assert_eq!(tick["reconciling"], false);
        assert_eq!(
            submissions(&fresh).len(),
            1,
            "a fresh frame with enough stock may trade again"
        );
    }

    #[test]
    fn an_optimistic_follow_up_rejection_does_not_back_off_the_target() {
        let mut h = harness();
        let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001)[0].clone();

        h.now.store(1_000_600, Ordering::SeqCst);
        let rejected = h.engine.on_purchase_result(
            &mut h.store,
            &follow,
            PurchaseOutcome {
                ok: false,
                error: Some("no items".into()),
                ..Default::default()
            },
        );
        assert!(rejected.iter().any(|event| matches!(
            event,
            EngineEvent::Broadcast { kind: "order", data } if data["phase"] == "failed"
        )));

        let snapshot = h.engine.runtime_snapshot(&h.store);
        let runtime = &snapshot["6319f7baa8ce530569ed16b9"];
        assert_eq!(
            runtime["blockedUntil"], 0,
            "a wrong guess about the remainder must not freeze the target"
        );
        assert_eq!(runtime["failureStreak"], 0);
        assert_eq!(
            h.store.state.stats.failed, 1,
            "the failed attempt is still recorded"
        );

        // The failure row says why it will not back off.
        let entry = h.store.state.history.first().expect("failure row");
        assert!(
            entry.note.as_deref().unwrap_or("").contains("optimistic"),
            "note: {:?}",
            entry.note
        );

        // Even so, the window is not abandoned for a guess that just failed:
        // until it expires the floor still protects the book.
        h.now.store(1_000_700, Ordering::SeqCst);
        let guarded =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_700, true);
        assert!(submissions(&guarded).is_empty());
    }

    #[test]
    fn a_follow_up_respects_the_current_target_price() {
        let mut h = harness();
        h.now.store(1_000_001, Ordering::SeqCst);
        let events = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        accept(&mut h, &intent, "order-1", 1_000_120);

        // The target price is lowered while the order is in flight: the
        // remainder is no longer worth chasing at the old price.
        let mut target = h.store.state.configs[0].clone();
        target.target_price = 0.05;
        h.store.upsert_config(target, None);

        h.now.store(1_000_400, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
            1_000_400,
            true,
        );
        assert!(
            submissions(&fill).is_empty(),
            "a follow-up buys at the price that triggered it, so the new ceiling must stop it"
        );
    }

    #[test]
    fn a_follow_up_never_doubles_a_deep_book() {
        let mut h = harness();
        {
            let mut target = h.store.state.configs[0].clone();
            target.desired_qty = 3;
            h.store.upsert_config(target, None);
        }

        // 10 on the book, 3 bought, 2 filled: the level still holds 8, so the
        // early follow-up repeats the target's per-signal quantity (3) and is
        // never doubled just because the book looks deep.
        h.now.store(1_000_001, Ordering::SeqCst);
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        assert_eq!(intent.qty, 3);
        assert!(!intent.doubled);

        accept(&mut h, &intent, "order-1", 1_000_120);
        h.now.store(1_000_400, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
            1_000_400,
            true,
        );
        let follow = submissions(&fill);
        assert_eq!(follow.len(), 1);
        assert_eq!(
            follow[0].qty, 3,
            "a deep level gets the normal per-signal quantity, not the intent remainder"
        );
        assert!(!follow[0].doubled);
        assert_eq!(follow[0].stock_at_trigger, Some(8));
    }

    #[test]
    fn a_remainder_below_the_minimum_is_not_chased() {
        let mut h = harness();
        {
            let mut target = h.store.state.configs[0].clone();
            target.min_acceptable_qty = 2;
            h.store.upsert_config(target, None);
        }
        let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001);
        assert!(
            follow.is_empty(),
            "the configured minimum still applies to a follow-up"
        );
    }

    #[test]
    fn off_mode_stops_the_local_rearm() {
        let mut h = harness();
        h.now.store(1_000_001, Ordering::SeqCst);
        let events = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        accept(&mut h, &intent, "order-1", 1_000_120);
        h.store
            .patch_settings(validation::SettingsPatch {
                mode: Some(Mode::Off),
                ..Default::default()
            })
            .unwrap();

        h.now.store(1_000_400, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
            1_000_400,
            true,
        );
        assert!(
            submissions(&fill).is_empty(),
            "off must not fire anything, locally or otherwise"
        );
    }

    #[test]
    fn a_follow_up_needs_a_linked_session() {
        let mut h = harness();
        h.now.store(1_000_001, Ordering::SeqCst);
        let events = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        accept(&mut h, &intent, "order-1", 1_000_120);

        // The session disappears between the order and its fill.
        h.now.store(1_000_400, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
            1_000_400,
            false,
        );
        assert!(
            submissions(&fill).is_empty(),
            "without a session the follow-up would only burn a rate slot"
        );
    }

    #[test]
    fn a_duplicate_fill_cannot_rearm_twice() {
        let mut h = harness();
        let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001);
        assert_eq!(follow.len(), 1);

        h.now.store(1_000_500, Ordering::SeqCst);
        let duplicate = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
            1_000_500,
            true,
        );
        assert!(duplicate.is_empty());
        assert_eq!(
            h.store.state.stats.orders_sent, 2,
            "one fill can only ever fire one follow-up"
        );
    }

    #[test]
    fn a_fill_that_beats_the_http_response_still_rearms() {
        let mut h = harness();
        h.now.store(1_000_001, Ordering::SeqCst);
        let events = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
        let intent = submissions(&events)[0].clone();

        // The fill frame lands before the purchase response is processed, so it
        // is buffered and replayed inside `on_purchase_result`.
        h.now.store(1_000_050, Ordering::SeqCst);
        let early = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-early", 2, 180_000, ORDER_STATUS_FILLED),
            1_000_050,
            true,
        );
        assert!(
            early.is_empty(),
            "no pending order yet, so it must be buffered"
        );

        h.now.store(1_000_120, Ordering::SeqCst);
        let accepted = h.engine.on_purchase_result(
            &mut h.store,
            &intent,
            PurchaseOutcome {
                ok: true,
                order_id: Some("order-early".into()),
                ..Default::default()
            },
        );
        let follow = submissions(&accepted);
        assert_eq!(
            follow.len(),
            1,
            "the replayed fill must still fire the remainder"
        );
        assert_eq!(follow[0].qty, 1);
        assert!(follow[0].optimistic);

        // The follow-up owns the in-flight slot; clearing it after the replay
        // would have let a third order through.
        let snapshot = h.engine.runtime_snapshot(&h.store);
        assert_eq!(snapshot["6319f7baa8ce530569ed16b9"]["inflight"], true);
    }

    #[test]
    fn a_frame_fuller_than_the_book_we_bought_from_is_trusted() {
        let mut h = harness();
        let follow = partial_fill(&mut h, 3, 2, "order-1", 1_000_001)[0].clone();
        accept(&mut h, &follow, "order-2", 1_000_550);

        // The seller listed more than the pre-purchase book ever held. A stale
        // snapshot cannot show that, so the frame is new information.
        h.now.store(1_000_600, Ordering::SeqCst);
        let restock =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 6), 1_000_600, true);
        let tick = market_payloads(&restock).remove(0);
        assert_eq!(tick["reconciling"], false);
        assert_eq!(tick["effectiveStock"], 6);
    }

    #[test]
    fn a_moved_best_price_closes_the_window() {
        let mut h = harness();
        // Buy the whole 3-unit level; the floor is therefore 0.
        h.now.store(1_000_001, Ordering::SeqCst);
        let events = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        accept(&mut h, &intent, "order-1", 1_000_120);
        h.now.store(1_000_400, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 3, 270_000, ORDER_STATUS_FILLED),
            1_000_400,
            true,
        );
        assert!(submissions(&fill).is_empty(), "the whole level was taken");

        // A cheaper listing appears on another level. The clamp is about the
        // level we cleared, so this must be tradable immediately.
        h.now.store(1_000_500, Ordering::SeqCst);
        let next = h
            .engine
            .handle_frame(&mut h.store, &market_frame(80_000, 2), 1_000_500, true);
        let tick = market_payloads(&next).remove(0);
        assert_eq!(tick["reconciling"], false);
        assert_eq!(tick["effectiveStock"], 2);
        let follow = submissions(&next);
        assert_eq!(follow.len(), 1);
        assert_eq!(follow[0].qty, 2);
        assert!(!follow[0].optimistic);
    }

    #[test]
    fn a_stale_frame_at_the_cleared_level_stays_clamped_after_a_full_fill() {
        let mut h = harness();
        h.now.store(1_000_001, Ordering::SeqCst);
        let events = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_001, true);
        let intent = submissions(&events)[0].clone();
        accept(&mut h, &intent, "order-1", 1_000_120);
        h.now.store(1_000_400, Ordering::SeqCst);
        h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 3, 270_000, ORDER_STATUS_FILLED),
            1_000_400,
            true,
        );

        // Same level, same depth as before the purchase: indistinguishable from
        // a pre-fill snapshot, so it waits out the window.
        h.now.store(1_000_500, Ordering::SeqCst);
        let stale = h
            .engine
            .handle_frame(&mut h.store, &market_frame(90_000, 3), 1_000_500, true);
        assert!(submissions(&stale).is_empty());
        let tick = market_payloads(&stale).remove(0);
        assert_eq!(tick["stock"], 3);
        assert_eq!(tick["effectiveStock"], 0);
        assert_eq!(tick["reconciling"], true);
    }

    #[test]
    fn a_frame_whose_total_count_dropped_closes_the_window() {
        let mut h = harness();
        h.now.store(1_000_001, Ordering::SeqCst);
        let events = h.engine.handle_frame(
            &mut h.store,
            &market_frame_with_total(90_000, 5, 20),
            1_000_001,
            true,
        );
        let intent = submissions(&events)[0].clone();
        assert_eq!(intent.total_count_at_trigger, Some(20));
        accept(&mut h, &intent, "order-1", 1_000_120);

        h.now.store(1_000_400, Ordering::SeqCst);
        let fill = h.engine.handle_frame(
            &mut h.store,
            &fill_frame("order-1", 2, 180_000, ORDER_STATUS_FILLED),
            1_000_400,
            true,
        );
        let follow = submissions(&fill)[0].clone();
        assert_eq!(follow.qty, 3);
        accept(&mut h, &follow, "order-2", 1_000_500);

        // The best level looks deeper than our floor, but the whole book lost
        // exactly the two units we bought: this frame has applied the fill.
        h.now.store(1_000_600, Ordering::SeqCst);
        let caught_up = h.engine.handle_frame(
            &mut h.store,
            &market_frame_with_total(90_000, 4, 18),
            1_000_600,
            true,
        );
        let tick = market_payloads(&caught_up).remove(0);
        assert_eq!(tick["reconciling"], false);
        assert_eq!(tick["effectiveStock"], 4);

        // The old window is gone. The frame bought 4 units of its own, so a new
        // ledger legitimately opens for that order — with `bought` back at zero
        // and the frame's depth as its trigger.
        let follow_now = submissions(&caught_up);
        assert_eq!(follow_now.len(), 1);
        assert_eq!(follow_now[0].qty, 4);
        let snapshot = h.engine.runtime_snapshot(&h.store);
        let reconcile = &snapshot["6319f7baa8ce530569ed16b9"]["reconcile"];
        assert_eq!(reconcile["triggerStock"], 4);
        assert_eq!(reconcile["bought"], 0);
        assert_eq!(reconcile["floor"], 0, "the new order committed all four");
    }
}
