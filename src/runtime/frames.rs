//! Frame path: engine run, event dispatch, price samples and market coalescing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::clock::now_ms;
use crate::constants::{
    MARKET_BROADCAST_INTERVAL_MS, MARKET_COALESCE_MAX, PRICE_SAMPLE_FEED_IDLE_MS,
};
use crate::engine::{EngineEvent, MarketTick};
use crate::events::is_marketplace_frame;
use crate::lock::lock;
use crate::server::AppState;

use super::purchase::run_purchase;

/// Runs the engine for one frame and dispatches whatever it asked for.
pub(super) fn process_frame(state: &Arc<AppState>, raw: Value, at: i64) {
    let events = {
        let mut store = lock(&state.store);
        // The activity chart is marketplace-only: listing/price updates and
        // order fulfillment results. Prediction-market, reward-statistic and
        // exchange-rate frames share the socket and must not inflate it.
        if is_marketplace_frame(&raw) {
            store.note_market_event(at);
        }
        let has_token = !store.state.session.access_token.is_empty();
        let mut engine = lock(&state.engine);
        // A headless worker (no deck connected) does not build market payloads.
        engine.set_broadcast_enabled(state.has_control_clients());
        engine.handle_frame(&mut store, &raw, at, has_token)
    };
    state.last_frame_at.store(at, Ordering::Relaxed);
    dispatch(state, events);
}

/// One sampler tick: write a close for every target the feed has gone quiet on.
///
/// The feed counts as alive only when the socket is online *and* some frame
/// arrived recently. A silent socket is disconnected traffic, not a flat
/// market, and must keep showing as a gap in the price chart.
pub(super) fn sample_prices(state: &Arc<AppState>) {
    let now = now_ms();
    let feed_online = lock(&state.gateway)
        .as_ref()
        .map(|feed| feed.status() == "online")
        .unwrap_or(false);
    let last_frame = state.last_frame_at.load(Ordering::Relaxed);
    let feed_alive = feed_online && last_frame > 0 && now - last_frame <= PRICE_SAMPLE_FEED_IDLE_MS;

    let samples = {
        let mut store = lock(&state.store);
        let mut engine = lock(&state.engine);
        engine.sample_prices(&mut store, now, feed_alive)
    };

    for sample in samples {
        state.broadcast(
            "price",
            json!({
                "itemId": sample.item_id,
                "price": sample.price,
                "low": sample.low,
                "high": sample.high,
                "rawLow": sample.raw_low,
                "rawHigh": sample.raw_high,
                "bucket": sample.bucket,
                "at": now,
                "sampled": true,
            }),
        );
    }
}

pub(super) fn dispatch(state: &Arc<AppState>, events: Vec<EngineEvent>) {
    for event in events {
        match event {
            EngineEvent::Broadcast { kind, data } => state.broadcast(kind, data),
            EngineEvent::Market(tick) => queue_market_tick(state, tick),
            EngineEvent::Submit(intent) => {
                let state = state.clone();
                tokio::spawn(async move {
                    run_purchase(state, *intent).await;
                });
            }
        }
    }
}

/// Hands a market tick to the coalescer, or sends it straight through when the
/// queue is gone or full so a tick is never dropped on the floor.
fn queue_market_tick(state: &Arc<AppState>, tick: Box<MarketTick>) {
    let slot = lock(&state.market);
    let Some(sender) = slot.as_ref() else {
        drop(slot);
        state.broadcast_serialized("market", tick.as_ref());
        return;
    };
    match sender.try_send(tick) {
        Ok(()) => {}
        Err(error) => state.broadcast_serialized("market", error.into_inner().as_ref()),
    }
}

/// Keeps only the newest tick per item and flushes them at
/// [`MARKET_BROADCAST_INTERVAL_MS`]. A busy market frame for one target can
/// arrive tens of times per second; the deck needs a handful.
pub(super) fn spawn_market_coalescer(
    state: Arc<AppState>,
    mut receiver: tokio::sync::mpsc::Receiver<Box<MarketTick>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut pending: HashMap<String, Box<MarketTick>> = HashMap::new();
        let mut interval =
            tokio::time::interval(Duration::from_millis(MARKET_BROADCAST_INTERVAL_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await; // the first tick is immediate
        loop {
            tokio::select! {
                incoming = receiver.recv() => {
                    let Some(tick) = incoming else {
                        break;
                    };
                    if pending.len() >= MARKET_COALESCE_MAX {
                        flush_market_ticks(&state, &mut pending);
                    }
                    pending.insert(tick.item_id.clone(), tick);
                }
                _ = interval.tick() => flush_market_ticks(&state, &mut pending),
            }
        }
        flush_market_ticks(&state, &mut pending);
    })
}

fn flush_market_ticks(state: &Arc<AppState>, pending: &mut HashMap<String, Box<MarketTick>>) {
    if pending.is_empty() {
        return;
    }
    for (_, tick) in pending.drain() {
        state.broadcast_serialized("market", tick.as_ref());
    }
}
