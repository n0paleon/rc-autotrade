//! Wiring: feed → engine → broadcasts → purchases → session refresh.
//!
//! Owns the long-lived tasks and is the only place that knows about all the
//! pieces at once.
//!
//! Lock discipline: handlers and these tasks always take `store` → `engine`,
//! never the other way around, and never hold a `std::sync::Mutex` guard across
//! an `.await`. The engine's `handle_frame` runs synchronously inside the guard
//! and returns the events to send afterwards.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::clock::now_ms;
use crate::constants::{
    EVENT_FLUSH_INTERVAL_MS, ITEM_EVENT_FLUSH_INTERVAL_MS, PRICE_SAMPLE_INTERVAL_MS,
};
use crate::engine::MarketTick;
use crate::feed::FeedHandle;
use crate::gateway::{self, GatewayConfig, GatewayEvent};
use crate::keepalive::{KeepaliveConfig, KeepaliveService};
use crate::lock::lock;
use crate::mock::{self, MockConfig};
use crate::server::AppState;

mod frames;
mod health;
mod mock_seed;
mod purchase;
mod session;

#[cfg(test)]
mod tests;

pub use health::HEALTH_TICK_MS;
pub use mock_seed::seed_mock_state;
pub use session::refresh_session;

use frames::{process_frame, sample_prices, spawn_market_coalescer};
use health::track_health;
use mock_seed::mock_items;
use session::tick_session;

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

impl Worker {
    /// Starts the feed, the engine loop and the 30 s session tick.
    pub fn start(state: Arc<AppState>) -> Self {
        // Market ticks are coalesced per item before they reach the sockets.
        // Created before the feed task so the very first frame has somewhere to
        // go. The channel is bounded; a full queue falls back to a direct send.
        let (market_tx, market_rx) = tokio::sync::mpsc::channel::<Box<MarketTick>>(1_024);
        {
            let mut slot = lock(&state.market);
            *slot = Some(market_tx);
        }
        let market_task = spawn_market_coalescer(state.clone(), market_rx);

        let credentials = {
            let state = state.clone();
            Arc::new(move || {
                let store = lock(&state.store);
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
                let mut slot = lock(&state.gateway);
                *slot = Some(feed.clone());
            }
            {
                let mut slot = lock(&state.mock);
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
                let mut slot = lock(&state.gateway);
                *slot = Some(feed.clone());
            }
            (feed, receiver)
        };

        let probe_state = state.clone();
        let should_probe: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
            let store = lock(&probe_state.store);
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
            let mut slot = lock(&state.keepalive);
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
                    let store = lock(&maintenance_state.store);
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
                        let store = lock(&maintenance_state.store);
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
                    let store = lock(&maintenance_state.store);
                    let mut engine = lock(&maintenance_state.engine);
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
                    let mut store = lock(&event_state.store);
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
                    let mut store = lock(&item_event_state.store);
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
