use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use serde_json::value::RawValue;

use crate::depth::Fill;
use crate::model::Mode;
use crate::timings::Timings;

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
    /// Headroom percentage copied from the runtime settings when the order was
    /// planned. The purchase request scales its `totalPrice` by this so the
    /// marketplace can complete the order instead of filling it partially.
    pub total_price_markup_pct: f64,
    /// `totalPrice` sent in the purchase request, in raw micro-RLT. Equal to
    /// `unit_price_raw * qty` when the markup is 0.
    pub total_price_raw: f64,
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
