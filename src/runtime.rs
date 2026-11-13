//! Wiring: feed → engine → broadcasts → purchases → session refresh.
//!
//! This is the Rust equivalent of `src/worker/index.js`. It owns the long-lived
//! tasks and is the only place that knows about all the pieces at once.
//!
//! Lock discipline: handlers and these tasks always take `store` → `engine`,
//! never the other way around, and never hold a `std::sync::Mutex` guard across
//! an `.await`. The engine's `handle_frame` runs synchronously inside the guard
//! and returns the events to send afterwards.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::api::{PurchaseRequest, PurchaseResult};
use crate::constants::{
    EVENT_FLUSH_INTERVAL_MS, ITEM_EVENT_FLUSH_INTERVAL_MS, MARKET_BROADCAST_INTERVAL_MS,
    MARKET_COALESCE_MAX, PRICE_SAMPLE_FEED_IDLE_MS, PRICE_SAMPLE_INTERVAL_MS,
};
use crate::dns_probe::{self, SocketMode};
use crate::engine::{EngineEvent, MarketTick, OrderIntent, PurchaseOutcome};
use crate::feed::FeedHandle;
use crate::gateway::{self, GatewayConfig, GatewayEvent};
use crate::keepalive::{KeepaliveConfig, KeepaliveService};
use crate::logger::Logger;
use crate::mock::{self, MockConfig, MockHandle, MockItem};
use crate::model::Target;
use crate::server::AppState;
use crate::session;
use crate::store::Store;
use crate::timings::Timings;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Seeds the demo targets and a synthetic session when `MOCK=1` and the store
/// is empty, matching `index.js` (the simulator must be usable with zero setup).
pub fn seed_mock_state(store: &mut Store, logger: &Arc<Logger>) -> Result<(), String> {
    if store.state.configs.is_empty() {
        let now = crate::store::now_ms();
        let targets = vec![
            Target {
                item_id: "6571aa39ca8bfef34adcd9e9".into(),
                label: "NEON SHARD".into(),
                item_type: "mutation_component".into(),
                rarity_color: None,
                target_price: 0.1,
                desired_qty: 10,
                high_stock_threshold: 20,
                min_acceptable_qty: 1,
                allow_partial_order: true,
                active: true,
                created_at: Some(now),
                updated_at: Some(now),
            },
            Target {
                item_id: "66b0a1c2d3e4f5061728394a".into(),
                label: "PIXEL CORE".into(),
                item_type: "mutation_component".into(),
                rarity_color: None,
                target_price: 0.05,
                desired_qty: 25,
                high_stock_threshold: 60,
                min_acceptable_qty: 5,
                allow_partial_order: true,
                active: true,
                created_at: Some(now),
                updated_at: Some(now),
            },
        ];
        store.set_configs(targets)?;
        logger.info("mock", "seeded demo targets");
    }
    if store.state.session.access_token.is_empty() {
        let patch = crate::validation::SessionPatch {
            access_token: Some(format!(
                "mock.{}.{}",
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4()
            )),
            refresh_token: Some("mock-refresh".into()),
            source: Some("mock".into()),
            ..Default::default()
        };
        store.apply_session_patch(patch)?;
        logger.info("mock", "seeded mock session");
    }
    seed_mock_catalog(store, logger);
    Ok(())
}

/// Names the simulator's ambient items up front.
///
/// The real worker resolves names from `item-info`; `MOCK=1` has no session to
/// call it with, so without this the item monitor would show six 24-character
/// ids and no product names. Rarity is derived from the level the same way the
/// miner resolver does it.
fn seed_mock_catalog(store: &mut Store, logger: &Arc<Logger>) {
    let now = crate::store::now_ms();
    let mut seeded = 0usize;
    for (item_id, name, item_type, level) in crate::mock::AMBIENT_ITEMS {
        if store.catalog_entry(item_id).is_some() {
            continue;
        }
        let rarity = match level {
            1 => Some(("Uncommon", "2bd600")),
            2 => Some(("Rare", "2196f3")),
            3 => Some(("Epic", "a52bff")),
            4 => Some(("Legendary", "ffb800")),
            5 => Some(("Unreal", "ff3b6b")),
            _ => None,
        };
        let label = match rarity {
            Some((rarity, _)) => format!("{rarity} {name}"),
            None => name.to_string(),
        };
        store.apply_catalog_item(
            item_id,
            &label,
            item_type,
            rarity.map(|(rarity, _)| rarity),
            rarity.map(|(_, color)| color),
            now,
        );
        seeded += 1;
    }
    for target in store.state.configs.clone() {
        if store.catalog_entry(&target.item_id).is_some() {
            continue;
        }
        store.apply_catalog_item(
            &target.item_id,
            &target.label,
            &target.item_type,
            None,
            target.rarity_color.as_deref(),
            now,
        );
        seeded += 1;
    }
    if seeded > 0 {
        logger.info("mock", format!("seeded {seeded} catalogue item(s)"));
    }
}

pub struct Worker {
    pub feed: FeedHandle,
    pub keepalive: Arc<KeepaliveService>,
    feed_task: JoinHandle<()>,
    ticker: JoinHandle<()>,
    sampler: JoinHandle<()>,
    maintenance: JoinHandle<()>,
    stats_task: JoinHandle<()>,
    event_task: JoinHandle<()>,
    item_event_task: JoinHandle<()>,
    market_task: JoinHandle<()>,
    keepalive_task: Option<JoinHandle<()>>,
    system_tasks: Vec<JoinHandle<()>>,
}

/// How often housekeeping runs (WAL checkpoint + orphan sweep). Price points
/// are only pruned once an hour, but the sweep itself is cheap.
const MAINTENANCE_INTERVAL_MS: u64 = 5 * 60_000;
/// Same retention the boot-time prune uses; the API only serves 7 days.
const PRICE_PRUNE_INTERVAL_MS: i64 = 3_600_000;
/// A deleted target keeps its in-memory history for a week so deleting and
/// re-adding it still shows the same chart. After that the runtime is released.
const ORPHAN_RUNTIME_IDLE_MS: i64 = 7 * 86_400_000;

/// Cadence of the background health sampler. Broadcasts live in the control
/// socket now; this task only samples and logs health transitions so a
/// struggling worker still leaves a trace with no deck connected.
pub const HEALTH_TICK_MS: u64 = 5_000;

/// Logs health transitions, matching `ControlServer.#trackSystemHealth` from
/// the Bun worker.
fn track_health(state: &Arc<AppState>, health_level: &mut String) {
    let system = state
        .system
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .snapshot();
    let level = system
        .pointer("/health/level")
        .and_then(Value::as_str)
        .unwrap_or("ok")
        .to_string();
    if level == *health_level {
        return;
    }
    let reasons = system
        .pointer("/health/reasons")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    match level.as_str() {
        "critical" => state
            .logger
            .warn("system", format!("worker health critical — {reasons}")),
        "warn" => state
            .logger
            .warn("system", format!("worker health degraded — {reasons}")),
        _ if *health_level == "warn" || *health_level == "critical" => {
            state.logger.info("system", "worker health recovered")
        }
        _ => {}
    }
    *health_level = level;
}

fn mock_items(state: &Arc<AppState>) -> Vec<MockItem> {
    let store = state
        .store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    store
        .state
        .configs
        .iter()
        .map(|target| MockItem {
            item_id: target.item_id.clone(),
            label: target.label.clone(),
            item_type: target.item_type.clone(),
            target_price: target.target_price,
            desired_qty: target.desired_qty,
            high_stock_threshold: target.high_stock_threshold,
            min_acceptable_qty: target.min_acceptable_qty,
            active: target.active,
        })
        .collect()
}

impl Worker {
    /// Starts the feed, the engine loop and the 30 s session tick.
    pub fn start(state: Arc<AppState>) -> Self {
        // Market ticks are coalesced per item before they reach the sockets.
        // Created before the feed task so the very first frame has somewhere to
        // go. The channel is bounded; a full queue falls back to a direct send.
        let (market_tx, market_rx) = tokio::sync::mpsc::channel::<Box<MarketTick>>(1_024);
        {
            let mut slot = state
                .market
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(market_tx);
        }
        let market_task = spawn_market_coalescer(state.clone(), market_rx);

        let credentials = {
            let state = state.clone();
            Arc::new(move || {
                let store = state
                    .store
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                (
                    store.state.session.access_token.clone(),
                    store.state.session.cookie.clone(),
                )
            })
        };

        let (feed, mut events) = if state.config.mock {
            let provider_state = state.clone();
            let items: mock::ItemProvider = Arc::new(move || mock_items(&provider_state));
            let (handle, receiver) = mock::spawn(MockConfig {
                items,
                interval_ms: state.config.mock_interval_ms as u64,
                logger: state.logger.clone(),
                latency: state.latency.clone(),
            });
            let feed = FeedHandle::Mock(handle.clone());
            {
                let mut slot = state
                    .gateway
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *slot = Some(feed.clone());
            }
            {
                let mut slot = state
                    .mock
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *slot = Some(handle);
            }
            (feed, receiver)
        } else {
            let (gateway, receiver) = gateway::spawn(GatewayConfig {
                url: state.config.ws_url.clone(),
                credentials: credentials.clone(),
                logger: state.logger.clone(),
                latency: state.latency.clone(),
            });
            let feed = FeedHandle::Gateway(gateway);
            {
                let mut slot = state
                    .gateway
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *slot = Some(feed.clone());
            }
            (feed, receiver)
        };

        let probe_state = state.clone();
        let should_probe: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
            let store = probe_state
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            !store.state.session.access_token.is_empty()
                && store.state.settings.mode.probes()
                && store.state.configs.iter().any(|target| target.active)
        });
        let keepalive = KeepaliveService::new(KeepaliveConfig {
            url: state.config.keep_alive_url.clone(),
            // The simulator has no HTTP edge to keep warm.
            interval_ms: if state.config.mock {
                0
            } else {
                state.config.keep_alive_ms.max(0) as u64
            },
            timeout_ms: 10_000,
            suspend_ms: 5 * 60_000,
            jitter_ratio: 0.1,
            credentials,
            should_probe,
            logger: state.logger.clone(),
            latency: state.latency.clone(),
            last_request_at: state.api.last_request_at_handle(),
            // Same `reqwest::Client` (and therefore the same pool) as purchases.
            http: state.api.http_client(),
        });
        {
            let mut slot = state
                .keepalive
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(keepalive.clone());
        }
        let keepalive_task = keepalive.start();

        let system_tasks = crate::system::spawn(
            state.system.clone(),
            state.config.system_sample_ms.max(500) as u64,
            state.logger.clone(),
        );

        let feed_state = state.clone();
        let feed_task = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    GatewayEvent::Status(data) => feed_state.broadcast("gateway", data),
                    GatewayEvent::Frame { raw, at } => process_frame(&feed_state, raw, at),
                }
            }
        });

        let ticker_state = state.clone();
        let mock_mode = state.config.mock;
        let ticker = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.tick().await; // the first tick is immediate
            loop {
                interval.tick().await;
                // A mock session cannot be refreshed against the real upstream.
                if !mock_mode {
                    tick_session(&ticker_state).await;
                }
            }
        });

        // The market feed is change-driven, so an item that does not move sends
        // nothing. A per-minute pass writes the last live close anyway; it goes
        // quiet whenever the feed itself stops, keeping real outages as gaps.
        let sample_state = state.clone();
        let sampler = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_millis(PRICE_SAMPLE_INTERVAL_MS));
            interval.tick().await; // the first tick is immediate
            loop {
                interval.tick().await;
                sample_prices(&sample_state);
            }
        });

        // Housekeeping: keep the price table inside its documented retention,
        // checkpoint the WAL so it cannot grow without bound, and release the
        // runtime of targets that have been deleted for a while. All three are
        // invisible to the deck: the served windows and payloads do not change.
        let maintenance_state = state.clone();
        let maintenance = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_millis(MAINTENANCE_INTERVAL_MS));
            interval.tick().await; // the first tick is immediate
            let mut pruned_at = now_ms();
            loop {
                interval.tick().await;
                let now = now_ms();

                let checkpoint = crate::blocking::run(|| {
                    let store = maintenance_state
                        .store
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    store.checkpoint_wal()
                });
                if let Err(error) = checkpoint {
                    maintenance_state
                        .logger
                        .warn("store", format!("WAL checkpoint failed: {error}"));
                }

                if now - pruned_at >= PRICE_PRUNE_INTERVAL_MS {
                    pruned_at = now;
                    let pruned = crate::blocking::run(|| {
                        let store = maintenance_state
                            .store
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        store.prune_price_history()
                    });
                    match pruned {
                        Ok(removed) if removed > 0 => maintenance_state
                            .logger
                            .info("store", format!("pruned {removed} expired price point(s)")),
                        Ok(_) => {}
                        Err(error) => maintenance_state.logger.warn("store", error),
                    }
                }

                let released = crate::blocking::run(|| {
                    let store = maintenance_state
                        .store
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let mut engine = maintenance_state
                        .engine
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    engine.prune_orphan_runtimes(&store, now, ORPHAN_RUNTIME_IDLE_MS)
                });
                if released > 0 {
                    maintenance_state.logger.info(
                        "engine",
                        format!("released {released} stale target runtime(s)"),
                    );
                }
            }
        });

        // Frame counts stay in memory; this task turns them into one SQLite
        // transaction every few seconds instead of a write per frame.
        let event_state = state.clone();
        let event_task = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_millis(EVENT_FLUSH_INTERVAL_MS));
            interval.tick().await; // the first tick is immediate
            loop {
                interval.tick().await;
                let flushed = {
                    let mut store = event_state
                        .store
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    store.flush_event_points()
                };
                if let Err(error) = flushed {
                    event_state.logger.warn(
                        "events",
                        format!("could not persist market activity: {error}"),
                    );
                }
            }
        });

        // Per-item frame counts also stay in memory; this task writes them in
        // one transaction on a slower cadence than the global activity counter.
        let item_event_state = state.clone();
        let item_event_task = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_millis(ITEM_EVENT_FLUSH_INTERVAL_MS));
            interval.tick().await; // the first tick is immediate
            loop {
                interval.tick().await;
                let flushed = {
                    let mut store = item_event_state
                        .store
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    store.flush_item_events()
                };
                if let Err(error) = flushed {
                    item_event_state.logger.warn(
                        "events",
                        format!("could not persist item activity: {error}"),
                    );
                }
            }
        });

        let stats_state = state.clone();
        let stats_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(HEALTH_TICK_MS));
            interval.tick().await; // the first tick is immediate
            let mut health_level = "ok".to_string();
            loop {
                interval.tick().await;
                track_health(&stats_state, &mut health_level);
            }
        });

        Self {
            feed,
            keepalive,
            feed_task,
            ticker,
            sampler,
            maintenance,
            stats_task,
            event_task,
            item_event_task,
            market_task,
            keepalive_task,
            system_tasks,
        }
    }

    pub async fn stop(self) {
        self.keepalive.stop();
        if let Some(task) = self.keepalive_task {
            task.abort();
        }
        for task in self.system_tasks {
            task.abort();
        }
        self.feed.stop().await;
        // Give the feed a moment to close cleanly.
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.feed_task.abort();
        self.ticker.abort();
        self.sampler.abort();
        self.maintenance.abort();
        self.stats_task.abort();
        self.event_task.abort();
        self.item_event_task.abort();
        self.market_task.abort();
    }
}

/// Runs the engine for one frame and dispatches whatever it asked for.
fn process_frame(state: &Arc<AppState>, raw: Value, at: i64) {
    let events = {
        let mut store = state
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Every frame the deck counts on the "Market events" tile is also what
        // the activity chart draws, so both numbers come from the same source.
        store.note_market_event(at);
        let has_token = !store.state.session.access_token.is_empty();
        let mut engine = state
            .engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
fn sample_prices(state: &Arc<AppState>) {
    let now = now_ms();
    let feed_online = state
        .gateway
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|feed| feed.status() == "online")
        .unwrap_or(false);
    let last_frame = state.last_frame_at.load(Ordering::Relaxed);
    let feed_alive = feed_online && last_frame > 0 && now - last_frame <= PRICE_SAMPLE_FEED_IDLE_MS;

    let samples = {
        let mut store = state
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut engine = state
            .engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

fn dispatch(state: &Arc<AppState>, events: Vec<EngineEvent>) {
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
    let slot = state
        .market
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
fn spawn_market_coalescer(
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

fn credentials(state: &Arc<AppState>) -> crate::api::Credentials {
    let store = state
        .store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    session::credentials(&store.state.session)
}

async fn run_purchase(state: Arc<AppState>, mut intent: OrderIntent) {
    let request = PurchaseRequest {
        item_id: intent.item_id.clone(),
        item_type: intent.item_type.clone(),
        qty: intent.qty,
        unit_price_raw: intent.unit_price_raw,
    };
    let request_payload = request.payload();

    // MOCK=1 routes purchases to the simulator; the HTTP client is untouched.
    let mock: Option<MockHandle> = state
        .mock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let real = mock.is_none();

    let mut result = match mock {
        Some(mock) => mock.purchase(request.clone()).await,
        None => state.api.purchase(&credentials(&state), &request).await,
    };
    // The first dispatch stays on the timeline even across a 401 retry, so the
    // stage still shows the refresh round-trip; the retry contributes the final
    // headers and TTFB.
    let send_at = result.send_at;
    let mut response_at = result.response_at;
    let mut ttfb_ms = result.ttfb_ms;
    let mut complete_at = now_ms();
    let mut attempts: u8 = 1;
    let mut refreshed = false;

    // 401 → refresh → retry once, like the JS `#request` path.
    if result.unauthorized {
        state.logger.warn(
            "order",
            "session rejected the purchase (401); refreshing and retrying",
        );
        if refresh_session(&state).await {
            result = state.api.purchase(&credentials(&state), &request).await;
            response_at = result.response_at;
            ttfb_ms = result.ttfb_ms;
            complete_at = now_ms();
            attempts = 2;
            refreshed = true;
        }
    }

    intent.timings.attempts = Some(attempts);
    if refreshed {
        intent.timings.unauthorized = Some(true);
    }

    if real {
        let host = dns_probe::host_of(state.api.base_url());
        apply_dns_telemetry(&mut intent.timings, host.as_deref(), &result);
    } else {
        // The simulator performs no HTTP exchange, so there is no header mark.
        // Keep the timeline readable by falling back to the completion time.
        response_at = response_at.or(Some(complete_at));
    }

    let outcome = PurchaseOutcome {
        ok: result.ok,
        order_id: result.order_id,
        error: result.error,
        send_at,
        response_at,
        complete_at: Some(complete_at),
        ttfb_ms,
        accepted_at: None,
        request_payload: Some(request_payload),
    };

    let events = {
        let mut store = state
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut engine = state
            .engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        engine.on_purchase_result(&mut store, &intent, outcome)
    };
    dispatch(&state, events);
}

/// Writes the connection and resolver facts onto the order timings. A fresh
/// connection keeps the real lookup; a pooled keep-alive socket reports
/// `reused` with no DNS cost; IP and local upstreams are `skipped`.
fn apply_dns_telemetry(timings: &mut Timings, host: Option<&str>, result: &PurchaseResult) {
    timings.socket_mode = Some(result.socket.as_str().to_string());

    let Some(host) = host else {
        return;
    };
    timings.dns_host = Some(host.chars().take(128).collect());
    if dns_probe::is_local(host) {
        timings.dns_mode = Some("skipped".to_string());
        return;
    }

    match &result.dns {
        // hyper only resolves when it opens a connection, so this is the lookup
        // this purchase actually paid for.
        Some(lookup) => {
            timings.dns_mode = Some("lookup".to_string());
            timings.dns_ms = Some(lookup.ms);
            timings.dns_address = lookup.address.clone();
            timings.dns_family = lookup.family;
            timings.dns_error = lookup.error.clone();
        }
        None if result.socket == SocketMode::Reused => {
            timings.dns_mode = Some("reused".to_string());
        }
        // A connection was opened but the lookup did not complete (cancelled
        // by a timeout, for example), or the upstream is not observable.
        None => {
            timings.dns_mode = Some("unknown".to_string());
        }
    }
}

/// One refresh attempt. Returns whether the access token changed.
pub async fn refresh_session(state: &Arc<AppState>) -> bool {
    let Ok(_guard) = state.refresh_lock.try_lock() else {
        // Another refresh is already in flight.
        return false;
    };

    let session_snapshot = {
        let store = state
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        store.state.session.clone()
    };
    let result = session::refresh_once(&state.api, &session_snapshot).await;

    if !result.ok {
        let message = result
            .error
            .clone()
            .unwrap_or_else(|| "refresh failed".into());
        let status = {
            let mut store = state
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            store.mark_session_error(&message);
            store.session_status()
        };
        state
            .logger
            .warn("session", format!("refresh failed: {message}"));
        // The deck used to learn about a failed refresh from the session
        // manager's `updated` event; without this push the Link/HUD panels
        // kept showing the last successful status until a manual refresh.
        state.broadcast("session", status);
        state.wake_keepalive("session refresh failed");
        return false;
    }

    let (changed, status) = {
        let mut store = state
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = session::apply_refresh(&mut store, &result);
        (changed, store.session_status())
    };
    state.logger.info("session", "access token renewed");
    state.broadcast("session", status);
    state.wake_keepalive("session refreshed");
    if changed {
        // A refresh that happens while the feed is down must reopen it with the
        // new token; a live feed keeps its socket (same rule as the Bun worker).
        crate::server::reconnect_gateway(state, "session refreshed", false).await;
    }
    changed
}

async fn tick_session(state: &Arc<AppState>) {
    let due = {
        let store = state
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        session::needs_refresh(&store.state.session, &store.state.settings, now_ms())
    };
    if due {
        refresh_session(state).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns_probe::DnsLookup;

    fn purchase(socket: SocketMode, dns: Option<DnsLookup>) -> PurchaseResult {
        PurchaseResult {
            socket,
            dns,
            ..Default::default()
        }
    }

    #[test]
    fn a_new_connection_keeps_the_real_lookup() {
        let mut timings = Timings::default();
        let result = purchase(
            SocketMode::New,
            Some(DnsLookup {
                ms: 18.25,
                address: Some("104.18.0.1".into()),
                family: Some(4),
                error: None,
            }),
        );
        apply_dns_telemetry(&mut timings, Some("rollercoin.com"), &result);
        assert_eq!(timings.socket_mode.as_deref(), Some("new"));
        assert_eq!(timings.dns_host.as_deref(), Some("rollercoin.com"));
        assert_eq!(timings.dns_mode.as_deref(), Some("lookup"));
        assert_eq!(timings.dns_ms, Some(18.25));
        assert_eq!(timings.dns_address.as_deref(), Some("104.18.0.1"));
        assert_eq!(timings.dns_family, Some(4));
        assert_eq!(timings.dns_error, None);
    }

    #[test]
    fn a_reused_socket_reports_no_dns_cost() {
        let mut timings = Timings::default();
        apply_dns_telemetry(
            &mut timings,
            Some("rollercoin.com"),
            &purchase(SocketMode::Reused, None),
        );
        assert_eq!(timings.socket_mode.as_deref(), Some("reused"));
        assert_eq!(timings.dns_mode.as_deref(), Some("reused"));
        assert_eq!(timings.dns_ms, None);
        assert_eq!(timings.dns_address, None);
    }

    #[test]
    fn local_and_ip_upstreams_are_skipped() {
        let mut timings = Timings::default();
        apply_dns_telemetry(
            &mut timings,
            Some("127.0.0.1"),
            &purchase(SocketMode::Unknown, None),
        );
        assert_eq!(timings.socket_mode.as_deref(), Some("unknown"));
        assert_eq!(timings.dns_mode.as_deref(), Some("skipped"));
        assert_eq!(timings.dns_host.as_deref(), Some("127.0.0.1"));
        assert_eq!(timings.dns_ms, None);
    }

    #[test]
    fn a_failed_lookup_keeps_its_duration_and_error() {
        let mut timings = Timings::default();
        let result = purchase(
            SocketMode::New,
            Some(DnsLookup {
                ms: 3.5,
                address: None,
                family: None,
                error: Some("lookup timed out".into()),
            }),
        );
        apply_dns_telemetry(&mut timings, Some("rollercoin.com"), &result);
        assert_eq!(timings.dns_mode.as_deref(), Some("lookup"));
        assert_eq!(timings.dns_ms, Some(3.5));
        assert_eq!(timings.dns_error.as_deref(), Some("lookup timed out"));
    }
}
