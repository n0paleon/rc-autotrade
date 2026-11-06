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

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::constants::{
    MARKET_SNAPSHOT_THROTTLE_MS, ORDER_STATUS_FAILED, ORDER_STATUS_FILLED, PRICE_HISTORY_LIMIT,
    PRICE_POINT_WRITE_INTERVAL_MS,
};
use crate::depth::{Fill, estimate_fill_price};
use crate::events::{Kind, UpstreamEvent, normalize_upstream_event};
use crate::logger::Logger;
use crate::model::{MarketSnapshot, OrderEntry, Target};
use crate::rules::{Action, Plan, plan_order, round_price};
use crate::store::{Store, now_ms};
use crate::timings::Timings;
use crate::trend::{FillRef, Point, TrendOptions, TrendSignal, build_trend};

const PENDING_TIMEOUT_MS: i64 = 120_000;
const MAX_UNMATCHED_RESULTS: usize = 128;
const SCAN_LOG_INTERVAL_MS: i64 = 12_000;
const TREND_REFRESH_MS: i64 = 10_000;

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Everything the engine wants the outside world to do, in order.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    Broadcast { kind: &'static str, data: Value },
    Submit(Box<OrderIntent>),
}

#[derive(Debug, Clone)]
pub struct OrderIntent {
    pub item_id: String,
    pub item_type: String,
    pub label: String,
    pub qty: i64,
    pub unit_price_raw: f64,
    pub unit_price: f64,
    pub target_price: f64,
    pub notional: f64,
    pub doubled: bool,
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
    pub response_at: Option<i64>,
    pub complete_at: Option<i64>,
    pub accepted_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct PendingOrder {
    item_id: String,
    item_type: String,
    label: String,
    qty: i64,
    unit_price: f64,
    target_price: f64,
    at: i64,
    timings: Timings,
    raw_price_event: Value,
}

#[derive(Debug, Clone)]
struct BufferedResult {
    event: UpstreamEvent,
    raw: Value,
    received_at: i64,
    buffered_at: i64,
}

#[derive(Debug, Clone)]
struct Runtime {
    last_price: Option<f64>,
    last_stock: Option<i64>,
    last_seen_at: Option<i64>,
    seen_at: Option<i64>,
    stale: bool,
    item_type: Option<String>,
    allowed_price_min: Option<f64>,
    allowed_price_max: Option<f64>,
    allowed_at: Option<i64>,
    history: Vec<Point>,
    fill_history: Vec<Point>,
    fill_qty_for: i64,
    last_fill: Option<Fill>,
    trend: Option<TrendSignal>,
    trend_at: i64,
    trend_target: f64,
    persisted_at: i64,
    point_write_at: i64,
    last_point_bucket: Option<i64>,
    persist_error_at: i64,
    hydrated_at: Option<i64>,
    inflight: bool,
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
            seen_at: None,
            stale: false,
            item_type: None,
            allowed_price_min: None,
            allowed_price_max: None,
            allowed_at: None,
            history: Vec::new(),
            fill_history: Vec::new(),
            fill_qty_for: desired_qty,
            last_fill: None,
            trend: None,
            trend_at: 0,
            trend_target: 0.0,
            persisted_at: 0,
            point_write_at: 0,
            last_point_bucket: None,
            persist_error_at: 0,
            hydrated_at: None,
            inflight: false,
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
        }
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
        let event = normalize_upstream_event(raw);
        match event.kind {
            Kind::Market => {
                let Some(item_id) = event.item_id.clone() else {
                    return Vec::new();
                };
                self.handle_market(store, &event, &item_id, raw, received_at, has_token)
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
        has_token: bool,
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
        let item_type = event
            .item_type
            .clone()
            .or_else(|| {
                store
                    .find_config(item_id)
                    .map(|config| config.item_type.clone())
            })
            .unwrap_or_else(|| "mutation_component".to_string());

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
        let fill = estimate_fill_price(&listings, rest, desired_qty);

        let mut events = Vec::new();
        let (trend, allowed_min, allowed_max, allowed_at) = {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            runtime.last_price = Some(best.unit_price);
            runtime.last_stock = Some(best.quantity);
            runtime.last_seen_at = Some(now);
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

            let last = runtime.history.last().copied();
            match last {
                Some(point) if point.t >= bucket => {
                    if let Some(point) = runtime.history.last_mut() {
                        point.p = best.unit_price;
                    }
                }
                _ => {
                    runtime.history.push(Point {
                        t: bucket,
                        p: best.unit_price,
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

            let trend = Self::compute_trend(&config, runtime, now, false);
            (
                trend,
                runtime.allowed_price_min,
                runtime.allowed_price_max,
                runtime.allowed_at,
            )
        };

        self.persist_market(
            store,
            &config,
            now,
            best.unit_price,
            fill,
            bucket,
            event.allowed_price_min.is_some() || event.allowed_price_max.is_some(),
        );

        let settings = store.state.settings;
        let plan = plan_order(
            config.plan_input(),
            best.unit_price,
            best.quantity as f64,
            settings.paused,
            Some(config.target_price),
        );
        let hot = plan.action == Action::Buy;

        events.push(EngineEvent::Broadcast {
            kind: "market",
            data: json!({
                "itemId": config.item_id,
                "label": label_for(Some(&config), &config.item_id),
                "price": best.unit_price,
                "stock": best.quantity,
                "targetPrice": config.target_price,
                "desiredQty": desired_qty,
                "allowedPriceMin": allowed_min,
                "allowedPriceMax": allowed_max,
                "allowedAt": allowed_at,
                "trend": serde_json::to_value(&trend).unwrap_or(Value::Null),
                "fill": fill.map(|fill| serde_json::to_value(fill).unwrap_or(Value::Null)),
                "currency": event.currency.clone().unwrap_or_else(|| "RLT".into()),
                "itemType": item_type,
                "bucket": bucket,
                "hot": hot,
                "at": now,
            }),
        });

        if hot {
            self.runtime_for(&config.item_id, desired_qty)
                .last_in_range_at = now;
        }

        self.log_scan(&config, best.unit_price, best.quantity, &plan, now);

        if !hot {
            return events;
        }
        if settings.paused || self.runtime_for(&config.item_id, desired_qty).inflight {
            return events;
        }
        if self.runtime_for(&config.item_id, desired_qty).blocked_until > now {
            return events;
        }
        if now - self.last_order_at < settings.delay_per_order {
            self.logger.debug(
                "engine",
                format!(
                    "{} in range, waiting for cooldown",
                    label_for(Some(&config), &config.item_id)
                ),
            );
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
                        settings.max_orders_per_minute,
                        label_for(Some(&config), &config.item_id)
                    ),
                );
            }
            return events;
        }
        if !settings.dry_run && !has_token {
            if now - self.last_no_token_warn_at > 30_000 {
                self.logger.warn(
                    "engine",
                    "signal found but no Rollercoin session is linked; open the Link tab",
                );
                self.last_no_token_warn_at = now;
            }
            return events;
        }

        let qty = plan.qty.unwrap_or(1);
        let notional = plan
            .notional
            .unwrap_or_else(|| round_price(best.unit_price * qty as f64));
        let mut timings = Timings::default();
        timings.v = 1;
        timings.signal_at = Some(signal_at as f64);
        timings.handle_at = Some(handle_at as f64);
        timings.order_at = Some(now as f64);
        timings.signal_cmd = Some(event.cmd.clone());

        let label = label_for(Some(&config), &config.item_id);
        self.last_order_at = now;
        {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            runtime.last_order_at = now;
            runtime.orders += 1;
        }

        let intent = OrderIntent {
            item_id: config.item_id.clone(),
            item_type,
            label: label.clone(),
            qty,
            unit_price_raw: best.price_raw,
            unit_price: best.unit_price,
            target_price: config.target_price,
            notional,
            doubled: plan.doubled.unwrap_or(false),
            at: now,
            timings,
            raw_price_event: raw.clone(),
        };

        if settings.dry_run {
            self.runtime_for(&config.item_id, desired_qty).last_result = Some(json!({
                "status": "dry", "at": now, "qty": qty, "price": best.unit_price
            }));
            self.logger.info(
                "order",
                format!(
                    "DRY RUN — would buy {} ×{} @ {:.6} RLT",
                    label, qty, best.unit_price
                ),
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
                    "doubled": intent.doubled,
                }),
            });
            return events;
        }

        store.state.stats.orders_sent += 1;
        let _ = store.flush();
        {
            let runtime = self.runtime_for(&config.item_id, desired_qty);
            runtime.inflight = true;
            runtime.last_result = Some(json!({
                "status": "submitted", "at": now, "qty": qty, "price": best.unit_price
            }));
        }

        let mut submitted = intent;
        submitted.timings.dispatch_at = Some(self.now() as f64);
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
            }),
        });
        self.logger.info(
            "order",
            format!(
                "SEND {} ×{} @ {:.6} RLT ({:.6} total){}",
                label,
                qty,
                submitted.unit_price,
                submitted.notional,
                if submitted.doubled {
                    " [HIGH STOCK ×2]"
                } else {
                    ""
                }
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
        timings.response_at = outcome.response_at.map(|value| value as f64);
        timings.complete_at = outcome.complete_at.map(|value| value as f64);

        let accepted = outcome.ok && outcome.order_id.is_some();
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
                    unit_price: intent.unit_price,
                    target_price: intent.target_price,
                    at: intent.at,
                    timings: timings.clone(),
                    raw_price_event: intent.raw_price_event.clone(),
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
                runtime.failure_streak += 1;
                let backoff =
                    (5_000_i64 * 2_i64.pow((runtime.failure_streak - 1).min(3) as u32)).min(60_000);
                runtime.blocked_until = now + backoff;
                runtime.last_result = Some(json!({
                    "status": "rejected", "at": now, "reason": reason
                }));
                (runtime.failure_streak, backoff)
            };
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
            events.extend(self.record_failure(
                store,
                FailureMeta {
                    item_id: intent.item_id.clone(),
                    label: intent.label.clone(),
                    qty: intent.qty,
                    unit_price: intent.unit_price,
                    target_price: intent.target_price,
                    at: intent.at,
                    timings: Some(timings.clone()),
                    raw_price_event: Some(intent.raw_price_event.clone()),
                    raw_result_event: None,
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
                }),
            });
        }

        self.runtime_for(&intent.item_id, intent.qty.max(1))
            .inflight = false;
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
                unit_price: round_price(unit_price),
                total_price: round_price(total_price),
                target_price: reference,
                profit_rlt: profit,
                status: "filled".to_string(),
                at: now,
                latency_ms: Some(now - pending_meta.at),
                timings: Some(fill_timings),
                raw_events: crate::raw_events::sanitize(&json!({
                    "priceEvent": pending_meta.raw_price_event,
                    "resultEvent": raw,
                })),
                note: None,
                message: Some("-".to_string()),
            };

            let entry_id = entry.id.clone();
            let _ = store.add_history(entry.clone());
            if let Some(order_id) = &fill.order_id {
                self.pending.remove(order_id);
            }

            let stock_after = {
                let runtime = self.runtime_for(&item_id, pending_meta.qty.max(1));
                runtime.fills += 1;
                runtime.last_stock = Some((runtime.last_stock.unwrap_or(0) - fill.count).max(0));
                runtime.last_result = Some(json!({
                    "status": "filled", "at": now, "qty": fill.count,
                    "price": unit_price, "profit": profit
                }));
                runtime.last_stock
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
            return events;
        }

        if fill.status as i64 == ORDER_STATUS_FAILED && fill.count == 0 && fill.price_raw == 0.0 {
            if let Some(order_id) = &fill.order_id {
                self.pending.remove(order_id);
            }
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
                    unit_price: pending_meta.unit_price,
                    target_price: target_price.unwrap_or(pending_meta.target_price),
                    at: pending_meta.at,
                    timings: Some(fill_timings),
                    raw_price_event: Some(pending_meta.raw_price_event.clone()),
                    raw_result_event: Some(raw.clone()),
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
        let series = if use_fill {
            fill_points.clone()
        } else {
            runtime.history.clone()
        };

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

        let signal = build_trend(&series, &options);
        runtime.trend = Some(signal.clone());
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
        fill: Option<Fill>,
        bucket: i64,
        band_changed: bool,
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

        if last_bucket != Some(bucket) || now - point_write_at >= PRICE_POINT_WRITE_INTERVAL_MS {
            let fill_tuple = fill.map(|fill| (fill.price, fill.filled as f64, fill.approx));
            if let Err(error) = store.record_price_point(&config.item_id, bucket, price, fill_tuple)
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
            qty: meta.qty,
            unit_price: meta.unit_price,
            total_price: 0.0,
            target_price: meta.target_price,
            profit_rlt: 0.0,
            status: "failed".to_string(),
            at: now,
            latency_ms: Some(now - meta.at).filter(|value| *value >= 0),
            timings: Some(timings),
            raw_events: crate::raw_events::sanitize(&json!({
                "priceEvent": meta.raw_price_event,
                "resultEvent": meta.raw_result_event,
            })),
            note: Some(reason.chars().take(160).collect()),
            message: Some(reason.chars().take(160).collect()),
        };

        let _ = store.add_history(entry.clone());

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

    pub fn runtime_snapshot(&self, store: &Store) -> Value {
        let mut out = serde_json::Map::new();
        for (item_id, runtime) in &self.runtime {
            let config = store.find_config(item_id);
            let mut value = serde_json::Map::new();
            value.insert("label".into(), json!(label_for(config, item_id)));
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
            value.insert("lastLogAt".into(), json!(runtime.last_log_at));
            value.insert("lastOrderAt".into(), json!(runtime.last_order_at));
            value.insert("lastInRangeAt".into(), json!(runtime.last_in_range_at));
            out.insert(item_id.clone(), Value::Object(value));
        }
        Value::Object(out)
    }
}

struct FailureMeta {
    item_id: String,
    label: String,
    qty: i64,
    unit_price: f64,
    target_price: f64,
    at: i64,
    timings: Option<Timings>,
    raw_price_event: Option<Value>,
    raw_result_event: Option<Value>,
}

/// Server-side helper: the current wall clock.
pub fn wall_clock() -> Clock {
    Arc::new(now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Target;
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
        let logger = Arc::new(Logger::new("error"));
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
        let engine = Engine::new(Arc::new(move || clock_now.load(Ordering::SeqCst)), logger);
        Harness {
            engine,
            store,
            now,
            dir,
        }
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
        // A market broadcast must be emitted before the submit intent.
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::Broadcast { kind: "market", .. }))
        );
    }

    #[test]
    fn paused_settings_block_submission_but_keep_the_market_tick() {
        let mut h = harness();
        h.store
            .patch_settings(validation::SettingsPatch {
                paused: Some(true),
                ..Default::default()
            })
            .unwrap();
        let events =
            h.engine
                .handle_frame(&mut h.store, &market_frame(90_000, 10), 1_000_001, true);
        assert!(submissions(&events).is_empty());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::Broadcast { kind: "market", .. }))
        );
    }

    #[test]
    fn dry_run_broadcasts_the_plan_without_submitting() {
        let mut h = harness();
        h.store
            .patch_settings(validation::SettingsPatch {
                dry_run: Some(true),
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
        assert_eq!(entry.qty, intent.qty);
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
}
