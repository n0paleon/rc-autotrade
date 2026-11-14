//! HTTP control plane and control WebSocket for the deck.
//!
//! Routes read and mutate the in-memory store and engine, serve the static
//! deck from `dist/`, and push state to every open `rcw.v1` control socket.

use std::sync::atomic::{AtomicI64, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::api::ApiClient;
use crate::clock::now_ms;
use crate::config::Config;
use crate::engine::{Engine, MarketTick};
use crate::feed::FeedHandle;
use crate::item_info::ItemInfoService;
use crate::keepalive::KeepaliveService;
use crate::latency::LatencyTracker;
use crate::lock::lock;
use crate::logger::Logger;
use crate::mock::MockHandle;
use crate::store::Store;
use crate::system::SystemMonitor;

mod analytics;
mod auth;
mod backup;
mod configs;
mod error;
mod history;
mod item_info;
mod router;
mod sessions;
mod settings;
mod system;
mod views;
mod ws;

#[cfg(test)]
mod tests;

pub use auth::Auth;
pub use router::router;

pub struct AppState {
    pub store: Mutex<Store>,
    pub engine: Mutex<Engine>,
    pub api: Arc<ApiClient>,
    pub item_info: Arc<ItemInfoService>,
    pub latency: Arc<Mutex<LatencyTracker>>,
    pub gateway: Mutex<Option<FeedHandle>>,
    pub mock: Mutex<Option<MockHandle>>,
    pub keepalive: Mutex<Option<Arc<KeepaliveService>>>,
    pub system: Arc<Mutex<SystemMonitor>>,
    pub logger: Arc<Logger>,
    pub config: Config,
    pub started_at: i64,
    pub port: AtomicU16,
    /// Receive time of the newest upstream frame, in ms. The price sampler uses
    /// it to tell a quiet item (frames still arriving for others) apart from a
    /// feed that is actually down.
    pub last_frame_at: AtomicI64,
    /// Every control-socket frame, already serialised into its envelope. The
    /// buffer is shared (`Bytes`) so each socket clones a handle instead of
    /// re-encoding the payload.
    pub hub: broadcast::Sender<Bytes>,
    /// Queue into the market coalescer. `None` before the worker starts.
    pub market: Mutex<Option<tokio::sync::mpsc::Sender<Box<MarketTick>>>>,
    /// Bumped when the worker access key is rotated so open control sockets
    /// can be closed with code 4003 (the deck reconnects with the new key).
    pub auth_epoch: tokio::sync::watch::Sender<u64>,
    /// Coalesces concurrent session refreshes (feed 401 + scheduled tick).
    pub refresh_lock: tokio::sync::Mutex<()>,
}

/// One control-socket frame: `{"type": ..., "data": ..., "at": ...}`.
///
/// Serialising the envelope directly avoids building the payload string first
/// and copying it into a second buffer on every broadcast.
#[derive(Serialize)]
struct Envelope<'a, T: ?Sized> {
    #[serde(rename = "type")]
    kind: &'a str,
    data: &'a T,
    at: i64,
}

impl AppState {
    /// Lock order is always `store` → `engine` (both are std mutexes, so this
    /// ordering is what keeps the two guards from deadlocking).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        store: Store,
        engine: Engine,
        api: Arc<ApiClient>,
        item_info: Arc<ItemInfoService>,
        latency: Arc<Mutex<LatencyTracker>>,
        system: Arc<Mutex<SystemMonitor>>,
        logger: Arc<Logger>,
    ) -> Self {
        let (hub, _) = broadcast::channel(1_024);
        let (auth_epoch, _) = tokio::sync::watch::channel(0u64);
        let state = Self {
            store: Mutex::new(store),
            engine: Mutex::new(engine),
            api,
            item_info,
            latency,
            gateway: Mutex::new(None),
            mock: Mutex::new(None),
            keepalive: Mutex::new(None),
            system,
            logger: logger.clone(),
            started_at: now_ms(),
            port: AtomicU16::new(config.port),
            last_frame_at: AtomicI64::new(0),
            config,
            hub,
            market: Mutex::new(None),
            auth_epoch,
            refresh_lock: tokio::sync::Mutex::new(()),
        };
        // Every log line also reaches connected decks, like `logger.setSink`.
        let sink_hub = state.hub.clone();
        logger.set_sink(Arc::new(move |entry| {
            if sink_hub.receiver_count() == 0 {
                return;
            }
            let Ok(frame) = serde_json::to_string(&Envelope {
                kind: "log",
                data: &entry,
                at: now_ms(),
            }) else {
                return;
            };
            let _ = sink_hub.send(Bytes::from(frame));
        }));
        state
    }

    /// True while at least one control socket is connected.
    pub fn has_control_clients(&self) -> bool {
        self.hub.receiver_count() > 0
    }

    pub fn set_port(&self, port: u16) {
        self.port.store(port, Ordering::Relaxed);
    }

    pub fn actual_port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    pub fn keepalive_service(&self) -> Option<Arc<KeepaliveService>> {
        lock(&self.keepalive).clone()
    }

    pub fn wake_keepalive(&self, reason: &str) {
        if let Some(service) = self.keepalive_service() {
            service.wake(reason);
        }
    }

    /// Serialises one envelope and hands the same buffer to every socket.
    ///
    /// Serialising per receiver meant a deep `Value` clone plus a `to_string`
    /// per client per frame. With no socket connected nothing is built at all.
    pub fn broadcast_serialized<T: Serialize + ?Sized>(&self, kind: &str, data: &T) {
        if self.hub.receiver_count() == 0 {
            return;
        }
        let Ok(frame) = serde_json::to_string(&Envelope {
            kind,
            data,
            at: now_ms(),
        }) else {
            return;
        };
        let _ = self.hub.send(Bytes::from(frame));
    }

    /// Convenience wrapper for callers that already hold a `Value`.
    pub fn broadcast(&self, kind: &str, data: Value) {
        self.broadcast_serialized(kind, &data);
    }
}

/// Reopens the market feed after a credential change.
///
/// `force` mirrors the `mustReplace` rule: manual/bridge session changes
/// always reconnect (the open socket still uses the old token), while a
/// scheduled refresh only reconnects a feed that is already down.
pub async fn reconnect_gateway(state: &Arc<AppState>, reason: &str, force: bool) {
    let gateway = lock(&state.gateway).clone();
    if let Some(gateway) = gateway
        && (force || gateway.status() != "online")
    {
        gateway.reconnect(reason).await;
    }
}
