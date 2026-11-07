//! Wiring: feed → engine → broadcasts → purchases → session refresh.
//!
//! This is the Rust equivalent of `src/worker/index.js`. It owns the long-lived
//! tasks and is the only place that knows about all the pieces at once.
//!
//! Lock discipline: handlers and these tasks always take `store` → `engine`,
//! never the other way around, and never hold a `std::sync::Mutex` guard across
//! an `.await`. The engine's `handle_frame` runs synchronously inside the guard
//! and returns the events to send afterwards.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::api::{PurchaseRequest, PurchaseResult};
use crate::constants::{
    EVENT_FLUSH_INTERVAL_MS, PRICE_SAMPLE_FEED_IDLE_MS, PRICE_SAMPLE_INTERVAL_MS,
};
use crate::dns_probe::{self, SocketMode};
use crate::engine::{EngineEvent, OrderIntent, PurchaseOutcome};
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
    Ok(())
}

pub struct Worker {
    pub feed: FeedHandle,
    pub keepalive: Arc<KeepaliveService>,
    feed_task: JoinHandle<()>,
    ticker: JoinHandle<()>,
    sampler: JoinHandle<()>,
    stats_task: JoinHandle<()>,
    event_task: JoinHandle<()>,
    keepalive_task: Option<JoinHandle<()>>,
    system_tasks: Vec<JoinHandle<()>>,
}

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
            stats_task,
            event_task,
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
        self.stats_task.abort();
        self.event_task.abort();
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
            EngineEvent::Submit(intent) => {
                let state = state.clone();
                tokio::spawn(async move {
                    run_purchase(state, *intent).await;
                });
            }
        }
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
    let mut response_at = now_ms();

    // 401 → refresh → retry once, like the JS `#request` path.
    if result.unauthorized {
        state.logger.warn(
            "order",
            "session rejected the purchase (401); refreshing and retrying",
        );
        if refresh_session(&state).await {
            result = state.api.purchase(&credentials(&state), &request).await;
            response_at = now_ms();
        }
    }

    if real {
        let host = dns_probe::host_of(state.api.base_url());
        apply_dns_telemetry(&mut intent.timings, host.as_deref(), &result);
    }

    let outcome = PurchaseOutcome {
        ok: result.ok,
        order_id: result.order_id,
        error: result.error,
        response_at: Some(response_at),
        complete_at: Some(now_ms()),
        accepted_at: None,
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
