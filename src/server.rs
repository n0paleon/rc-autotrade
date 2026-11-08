//! HTTP control plane + control WebSocket, ported from `src/worker/server.js`.
//!
//! Scope of this first slice: the routes the React deck needs to read and
//! mutate state, the `rcw.v1` control socket, and static serving of `dist/`.
//! Engine-backed endpoints (`/api/control` reconnect, `/api/latency`,
//! `/api/system`, `/api/item-info`, backup, session refresh) answer 503 with an
//! explicit "not ported yet" message until their modules land.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use crate::api::ApiClient;
use crate::config::Config;
use crate::constants::{
    APP_NAME, APP_VERSION, EVENT_BUCKET_MS, EVENT_HISTORY_DAYS, EVENT_RETENTION_DAYS,
    LOOT_LOG_MAX_PAGE_SIZE, LOOT_LOG_PAGE_SIZE, Limits, PRICE_HISTORY_DAYS, PRICE_RETENTION_DAYS,
    TELEMETRY_INTERVAL_SECS,
};
use crate::engine::{Engine, MarketTick};
use crate::feed::FeedHandle;
use crate::item_info::ItemInfoService;
use crate::keepalive::KeepaliveService;
use crate::latency::LatencyTracker;
use crate::logger::Logger;
use crate::mock::MockHandle;
use crate::model::{Mode, Settings};
use crate::store::{HistoryQuery, Store, now_ms};
use crate::system::SystemMonitor;
use crate::validation::{self, FieldErrors};

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
        self.keepalive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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

    pub fn configs_view(&self, store: &Store, engine: &Engine) -> Vec<Value> {
        let mut runtime = engine.runtime_snapshot(store);
        let mut runtime_entries = runtime.as_object_mut();
        store
            .state
            .configs
            .iter()
            .map(|config| {
                let mut value = serde_json::to_value(config).unwrap_or(Value::Null);
                if let Some(object) = value.as_object_mut() {
                    let key = config.item_id.to_ascii_lowercase();
                    // Move the entry out of the temporary map instead of
                    // cloning the whole history for every target on each view.
                    let entry = runtime_entries
                        .as_mut()
                        .and_then(|entries| entries.remove(&key))
                        .unwrap_or(Value::Null);
                    object.insert("runtime".to_string(), entry);
                }
                value
            })
            .collect()
    }

    pub fn snapshot(&self, store: &Store, engine: &Engine) -> Value {
        let now = now_ms();
        let gateway = match self
            .gateway
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            Some(handle) => handle.stats_json(),
            None => json!({
                "status": "idle",
                "connectedAt": null,
                "lastMessageAt": null,
                "lastError": null,
                "attempts": 0,
                "reconnects": 0,
                "eventCount": 0,
                "parseErrors": 0,
                "commands": [],
                "upstream": upstream_host(&self.config.ws_url),
                "uptimeMs": 0,
                "lastPingMs": null,
            }),
        };
        let latency = self
            .latency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot();
        let pending = engine.pending_snapshot();

        json!({
            "now": now,
            "name": APP_NAME,
            "version": APP_VERSION,
            "startedAt": self.started_at,
            "uptimeMs": now - self.started_at,
            "worker": {
                "host": self.config.host,
                "port": self.actual_port(),
                "mock": self.config.mock,
                "logLevel": self.config.log_level,
            },
            "gateway": gateway,
            "session": store.session_status(),
            "latency": latency,
            "keepalive": self
                .keepalive_service()
                .map(|service| service.stats_json())
                .unwrap_or(Value::Null),
            "system": self
                .system
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .snapshot(),
            "settings": store.state.settings,
            "configs": self.configs_view(store, engine),
            "stats": store.state.stats,
            "report": store.report_view(),
            "history": store.state.history.iter().take(LOOT_LOG_PAGE_SIZE).cloned().collect::<Vec<_>>(),
            "historyTotal": store.history_count(),
            "pending": pending,
            "workerTokenPreview": store.state.worker_token_preview,
            "accessKeyPinned": !self.config.worker_token.is_empty(),
            "limits": {
                "delayMin": Limits::DELAY_MIN,
                "delayMax": Limits::DELAY_MAX,
                "qtyMax": Limits::QTY_MAX,
                "configsMax": Limits::CONFIGS_MAX,
                "ordersPerMinuteMax": Limits::ORDERS_PER_MINUTE_MAX,
            },
        })
    }
}

fn upstream_host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}

/// Reopens the market feed after a credential change.
///
/// `force` mirrors the Bun worker's `mustReplace` rule: manual/bridge session
/// changes always reconnect (the open socket still uses the old token), while a
/// scheduled refresh only reconnects a feed that is already down.
pub async fn reconnect_gateway(state: &Arc<AppState>, reason: &str, force: bool) {
    let gateway = state
        .gateway
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(gateway) = gateway {
        if force || gateway.status() != "online" {
            gateway.reconnect(reason).await;
        }
    }
}

fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn error_body(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "ok": false, "error": message }))).into_response()
}

fn fields_error(status: StatusCode, message: &str, fields: validation::FieldErrors) -> Response {
    (
        status,
        Json(json!({ "ok": false, "error": message, "fields": fields })),
    )
        .into_response()
}

/// Bearer-token guard for every `/api/*` route except `/api/health`.
pub struct Auth;

impl FromRequestParts<Arc<AppState>> for Auth {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .unwrap_or_default();

        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        if store.verify_worker_token(token) {
            return Ok(Auth);
        }
        Err(error_body(
            StatusCode::UNAUTHORIZED,
            "Missing or invalid worker access key.",
        ))
    }
}

// ── handlers ────────────────────────────────────────────────────────────────

async fn health(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({
        "ok": true,
        "name": APP_NAME,
        "version": APP_VERSION,
        "mock": state.config.mock,
        "uptimeMs": now_ms() - state.started_at,
    }))
    .into_response()
}

async fn state_view(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    let mut body = state.snapshot(&store, &engine);
    body["ok"] = Value::Bool(true);
    Json(body).into_response()
}

async fn get_configs(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({ "ok": true, "configs": state.configs_view(&store, &engine) })).into_response()
}

async fn put_configs(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let list = if body.is_array() {
        body.clone()
    } else {
        body.get("configs").cloned().unwrap_or(Value::Null)
    };

    let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let existing = store.config_ids();
    let values = match validation::validate_targets(&list, &existing) {
        Ok(values) => values,
        Err(fields) => {
            return fields_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Some targets are invalid.",
                fields,
            );
        }
    };

    if let Err(message) = store.set_configs(values.clone()) {
        state.logger.error("config", message);
        return error_body(StatusCode::INTERNAL_SERVER_ERROR, "Could not save targets.");
    }
    let mut engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    for target in &values {
        engine.refresh_trend(&store, &target.item_id);
    }
    let view = state.configs_view(&store, &engine);
    drop(engine);
    drop(store);

    state.logger.info(
        "config",
        format!(
            "target list replaced ({} item{})",
            values.len(),
            if values.len() == 1 { "" } else { "s" }
        ),
    );
    // Bulk enrichment runs after the response, like `enrichMany(...).catch`.
    spawn_enrichment(
        state.clone(),
        values
            .iter()
            .map(|target| (target.item_id.clone(), target.item_type.clone()))
            .collect(),
    );
    state.broadcast("configs", Value::Array(view.clone()));
    Json(json!({ "ok": true, "configs": view })).into_response()
}

async fn post_config(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let saved = {
        let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        let existing = store.config_ids();
        let target = match validation::validate_target(&body, &existing, None) {
            Ok(target) => target,
            Err(fields) => {
                return fields_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Target is invalid.",
                    fields,
                );
            }
        };
        let saved = store.upsert_config(target, None);
        let mut engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        engine.refresh_trend(&store, &saved.item_id);
        saved
    };
    state
        .logger
        .info("config", format!("target added: {}", saved.item_id));

    // Resolve the auto label before replying when possible, with the same 1.5 s
    // budget as the JS worker, so the deck receives a named target.
    if let Some(item) = enrich_item(&state, &saved.item_id, &saved.item_type).await {
        let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.apply_item_info(
            &saved.item_id,
            &item.label,
            &item.item_type,
            item.rarity_color.as_deref(),
        );
    }

    let view = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        let engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        state.configs_view(&store, &engine)
    };
    state.broadcast("configs", Value::Array(view.clone()));
    (
        StatusCode::CREATED,
        Json(json!({ "ok": true, "configs": view })),
    )
        .into_response()
}

/// Resolves an item label with the JS worker's 1.5 s budget. Never fails the
/// caller: an unresolved label just stays as the short item id.
async fn enrich_item(
    state: &Arc<AppState>,
    item_id: &str,
    item_type: &str,
) -> Option<crate::item_info::ItemInfo> {
    let credentials = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        crate::session::credentials(&store.state.session)
    };
    let future = state.item_info.preview(&credentials, item_id, item_type);
    match tokio::time::timeout(Duration::from_millis(1_500), future).await {
        Ok(Ok(item)) => Some(item),
        Ok(Err(message)) => {
            state
                .logger
                .warn("item-info", format!("enrichment failed: {message}"));
            None
        }
        Err(_) => {
            state
                .logger
                .warn("item-info", "enrichment timed out after 1500ms");
            None
        }
    }
}

/// Fire-and-forget enrichment used after edits and bulk replaces.
fn spawn_enrichment(state: Arc<AppState>, targets: Vec<(String, String)>) {
    tokio::spawn(async move {
        for (item_id, item_type) in targets {
            let Some(item) = enrich_item(&state, &item_id, &item_type).await else {
                continue;
            };
            let changed = {
                let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
                store.apply_item_info(
                    &item_id,
                    &item.label,
                    &item.item_type,
                    item.rarity_color.as_deref(),
                )
            };
            if changed {
                let view = {
                    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
                    let engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
                    state.configs_view(&store, &engine)
                };
                state.broadcast("configs", Value::Array(view));
            }
        }
    });
}

async fn patch_config(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Path(item_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let Some(existing) = store.find_config(&item_id).cloned() else {
        return error_body(StatusCode::NOT_FOUND, "Target not found.");
    };

    // Merge: the stored row wins for identity, the body wins for everything it
    // actually contains, exactly like `{...existing, ...body, itemId}`.
    let mut merged = serde_json::to_value(&existing).unwrap_or(Value::Null);
    if let (Some(base), Some(patch)) = (merged.as_object_mut(), body.as_object()) {
        for (key, value) in patch {
            base.insert(key.clone(), value.clone());
        }
        base.insert(
            "itemId".to_string(),
            Value::String(existing.item_id.clone()),
        );
    }

    let target = match validation::validate_target(&merged, &[], Some(&existing.item_id)) {
        Ok(target) => target,
        Err(fields) => {
            return fields_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Target is invalid.",
                fields,
            );
        }
    };

    let saved = store.upsert_config(target, Some(&existing.item_id));
    state
        .logger
        .info("config", format!("target updated: {}", saved.item_id));
    let mut engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    engine.refresh_trend(&store, &saved.item_id);
    let view = state.configs_view(&store, &engine);
    drop(engine);
    drop(store);

    // The JS worker enriches edits without blocking the response.
    spawn_enrichment(
        state.clone(),
        vec![(saved.item_id.clone(), saved.item_type.clone())],
    );
    state.broadcast("configs", Value::Array(view.clone()));
    Json(json!({ "ok": true, "configs": view })).into_response()
}

async fn delete_config(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Path(item_id): Path<String>,
) -> Response {
    let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let Some(existing) = store.find_config(&item_id).cloned() else {
        return error_body(StatusCode::NOT_FOUND, "Target not found.");
    };
    store.remove_config(&existing.item_id);
    state.logger.info(
        "config",
        format!(
            "target removed: {}",
            if existing.label.is_empty() {
                existing.item_id.clone()
            } else {
                existing.label.clone()
            }
        ),
    );
    let view = {
        let engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        state.configs_view(&store, &engine)
    };
    drop(store);

    state.broadcast("configs", Value::Array(view.clone()));
    Json(json!({ "ok": true, "configs": view })).into_response()
}

async fn patch_settings(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let patch = match validation::validate_settings(&body) {
        Ok(patch) => patch,
        Err(fields) => {
            return fields_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Some settings are invalid.",
                fields,
            );
        }
    };

    let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let resumed = patch.resumes();
    if let Err(fields) = check_arm_guard(&store.state.settings, &patch) {
        return fields_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Cannot arm live yet.",
            fields,
        );
    }
    if let Err(message) = store.patch_settings(patch) {
        state.logger.error("store", message);
        return error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save settings.",
        );
    }
    let settings = store.state.settings;
    drop(store);

    state.logger.info("config", "runtime settings updated");
    if resumed {
        state.wake_keepalive("trading resumed");
    }
    state.broadcast(
        "settings",
        serde_json::to_value(settings).unwrap_or(Value::Null),
    );
    Json(json!({ "ok": true, "settings": settings })).into_response()
}

/// Arms the worker out of `off` without ever arming live in one hop.
///
/// Going straight from off to live would put real money one click away from a
/// stopped worker, so that transition is refused: the caller has to pass
/// through shadow and confirm. Turning live *off* is always allowed.
fn check_arm_guard(
    current: &Settings,
    patch: &validation::SettingsPatch,
) -> Result<(), FieldErrors> {
    if patch.mode == Some(Mode::Live) && current.mode == Mode::Off {
        let mut fields = FieldErrors::new();
        fields.insert(
            "mode".into(),
            "Switch to shadow first and confirm the dry-run signals, then arm live.".into(),
        );
        return Err(fields);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ControlBody {
    action: String,
}

async fn control(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<ControlBody>,
) -> Response {
    match body.action.as_str() {
        "off" | "shadow" | "live" | "toggle" | "pause" | "resume" => {
            let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
            let current = store.state.settings.mode;
            let next = match body.action.as_str() {
                "off" | "pause" => Mode::Off,
                "shadow" => Mode::Shadow,
                "live" | "resume" => Mode::Live,
                // The header button is a panic switch in both directions: it
                // stops live trading, and it resumes into shadow — never
                // straight into live. Arming live is a deliberate act on the
                // runtime panel.
                _ if current == Mode::Live => Mode::Off,
                _ => Mode::Shadow,
            };
            let patch = validation::SettingsPatch {
                mode: Some(next),
                ..Default::default()
            };
            if let Err(fields) = check_arm_guard(&store.state.settings, &patch) {
                return fields_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Cannot arm live yet.",
                    fields,
                );
            }
            if let Err(message) = store.patch_settings(patch) {
                return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
            }
            let settings = store.state.settings;
            drop(store);
            state.logger.info(
                "control",
                match next {
                    Mode::Off => "trading off",
                    Mode::Shadow => "trading in shadow mode (no orders will be sent)",
                    Mode::Live => "trading live",
                },
            );
            state.broadcast(
                "settings",
                serde_json::to_value(settings).unwrap_or(Value::Null),
            );
            if next.probes() {
                state.wake_keepalive("trading resumed");
            }
            Json(json!({ "ok": true, "settings": settings })).into_response()
        }
        "reconnect" => {
            let gateway = state
                .gateway
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            match gateway {
                Some(gateway) => {
                    gateway.reconnect("manual").await;
                    Json(json!({ "ok": true })).into_response()
                }
                None => error_body(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Market feed is not running.",
                ),
            }
        }
        _ => error_body(
            StatusCode::BAD_REQUEST,
            "action must be off, shadow, live, toggle or reconnect.",
        ),
    }
}

async fn history(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(LOOT_LOG_PAGE_SIZE)
        .min(LOOT_LOG_MAX_PAGE_SIZE);
    let offset = params
        .get("offset")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);

    let query = HistoryQuery {
        q: params.get("q").cloned().unwrap_or_default(),
        status: params.get("status").cloned().unwrap_or_default(),
        limit: Some(limit),
        offset,
        before_at: params
            .get("beforeAt")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0),
        before_id: params.get("beforeId").cloned().unwrap_or_default(),
    };

    // Even an unfiltered page runs a COUNT(*) over the whole log.
    let page = crate::blocking::run(|| {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.history_page(query)
    });
    match page {
        Ok(page) => Json(json!({
            "ok": true,
            "entries": page.entries,
            "total": page.total,
            "limit": page.limit,
            "offset": page.offset,
            "hasMore": page.has_more,
        }))
        .into_response(),
        Err(message) => error_body(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

async fn report(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({ "ok": true, "report": store.report_view() })).into_response()
}

/// Bucketed market-frame counts for the activity chart. Buckets stay at the
/// stored 5-minute resolution; the deck sums them into 15m/30m/1h/2h windows
/// so that boundaries line up with the viewer's local clock, not the server's.
async fn events_view(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let days = params
        .get("days")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, EVENT_HISTORY_DAYS);
    let now = now_ms();
    let since = now - days * 86_400_000;
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let points: Vec<Value> = store
        .event_series(since)
        .into_iter()
        .map(|(bucket_at, events)| json!({ "t": bucket_at, "n": events }))
        .collect();
    Json(json!({
        "ok": true,
        "days": days,
        "bucketMs": EVENT_BUCKET_MS,
        "since": since,
        "until": now,
        "retentionDays": EVENT_RETENTION_DAYS,
        "points": points,
    }))
    .into_response()
}

/// Raw 1-minute closes for one target, used by the price chart's 3d/7d
/// periods. The deck buckets them client-side so every boundary follows the
/// viewer's local clock, and merges the live 24h window on top.
async fn price_history_view(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let item_id = params
        .get("itemId")
        .or_else(|| params.get("item_id"))
        .map(|value| value.trim())
        .filter(|value| !value.is_empty());
    let Some(item_id) = item_id else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "itemId is required." })),
        )
            .into_response();
    };
    let days = params
        .get("days")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, PRICE_HISTORY_DAYS);
    let now = now_ms();
    let since = now - days * 86_400_000;
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let points: Vec<Value> = store
        .price_series(item_id, since)
        .into_iter()
        .map(|(bucket_at, price)| json!({ "t": bucket_at, "p": price }))
        .collect();
    Json(json!({
        "ok": true,
        "itemId": item_id,
        "days": days,
        "bucketMs": 60_000,
        "since": since,
        "until": now,
        "retentionDays": PRICE_RETENTION_DAYS,
        "points": points,
    }))
    .into_response()
}

async fn get_session(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({ "ok": true, "session": store.session_status() })).into_response()
}

#[axum::debug_handler]
async fn post_session(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let patch = match validation::validate_session_input(&body) {
        Ok(patch) => patch,
        Err(fields) => {
            return fields_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid session data.",
                fields,
            );
        }
    };

    let (changed, status) = {
        let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        let before = store.state.session.access_token.clone();
        if let Err(message) = store.apply_session_patch(patch) {
            return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
        }
        let changed = before != store.state.session.access_token;
        let status = store.session_status();
        (changed, status)
    };
    state.broadcast("session", status.clone());
    state.wake_keepalive("session updated");

    if changed {
        // Manual credential replacement must re-handshake even when the feed
        // still reports "online": that socket is authenticated with the old
        // token. The Bun worker did the same for `reason === "manual"`.
        reconnect_gateway(&state, "session updated", true).await;
    }

    Json(json!({ "ok": true, "changed": changed, "session": status })).into_response()
}

async fn delete_session(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let (changed, status) = {
        let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        let before = store.state.session.access_token.clone();
        let patch = validation::SessionPatch {
            clear_access_token: true,
            clear_refresh_token: true,
            clear_cookie: true,
            ..Default::default()
        };
        if let Err(message) = store.apply_session_patch(patch) {
            return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
        }
        (
            before != store.state.session.access_token,
            store.session_status(),
        )
    };
    state.logger.info(
        "session",
        "Rollercoin session cleared from the control deck",
    );
    state.broadcast("session", status.clone());
    if changed {
        reconnect_gateway(&state, "session cleared", true).await;
    }
    Json(json!({ "ok": true, "session": status })).into_response()
}

async fn rotate_auth(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    if !state.config.worker_token.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": "Access key is pinned by WORKER_TOKEN. Change the environment variable to rotate it."
            })),
        )
            .into_response();
    }

    let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let token = match store.rotate_worker_token() {
        Ok(token) => token,
        Err(message) => return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message),
    };
    drop(store);
    state.logger.warn(
        "auth",
        "worker access key rotated; revoking control sockets",
    );
    // Close every open control socket with 4003; the deck treats that code as
    // "reconnect with the new key" (4001 stays reserved for a bad key).
    let next = *state.auth_epoch.borrow() + 1;
    state.auth_epoch.send_replace(next);
    Json(json!({
        "ok": true,
        "token": token,
        "preview": crate::store::preview_token(&token),
    }))
    .into_response()
}

async fn latency_view(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let latency = state
        .latency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .snapshot();
    Json(json!({ "ok": true, "latency": latency })).into_response()
}

async fn latency_probe(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let probe = match state.keepalive_service() {
        Some(service) if service.enabled() => Some(service.probe_once().await),
        _ => None,
    };
    let latency = state
        .latency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .snapshot();
    let probe_json = match probe {
        Some(result) => result.to_json(),
        None => json!({ "ok": false, "skipped": true, "reason": "keepalive disabled" }),
    };
    state.broadcast("latency", latency.clone());
    Json(json!({ "ok": true, "latency": latency, "probe": probe_json })).into_response()
}

async fn system_view(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let system = state
        .system
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .snapshot();
    Json(json!({ "ok": true, "system": system })).into_response()
}

async fn delete_history(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let (stats, report, configs, latency) = {
        let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.reset_stats();
        let stats = store.state.stats;
        let report = store.report_view();
        let mut engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        engine.reset_counters();
        let configs = state.configs_view(&store, &engine);
        drop(engine);
        let mut tracker = state
            .latency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tracker.reset();
        let latency = tracker.snapshot();
        (stats, report, configs, latency)
    };

    state.logger.info(
        "stats",
        "lifetime stats, per-target fill rates and loot log reset",
    );
    state.broadcast("stats", serde_json::to_value(stats).unwrap_or(Value::Null));
    state.broadcast(
        "report",
        serde_json::to_value(report).unwrap_or(Value::Null),
    );
    state.broadcast("latency", latency);
    state.broadcast("configs", Value::Array(configs));
    state.broadcast("history-cleared", json!({}));
    Json(json!({ "ok": true, "stats": stats })).into_response()
}

async fn get_backup(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let format = params
        .get("format")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "json".to_string());
    let now = crate::store::now_ms();

    match format.as_str() {
        "json" => {
            let include_credentials = params
                .get("credentials")
                .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false);
            // Building the export walks every row and encodes a large JSON
            // document; keep it off the reactor thread.
            let built = crate::blocking::run(|| {
                let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
                crate::backup::build_backup(&store, include_credentials, now)
                    .map(|payload| serde_json::to_vec(&payload).unwrap_or_default())
            });
            let body = match built {
                Ok(body) => body,
                Err(message) => {
                    return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
                }
            };
            attachment(
                axum::body::Body::from(body),
                "application/json; charset=utf-8",
                &crate::backup::backup_filename(now),
            )
        }
        "sqlite" | "db" => {
            // `VACUUM INTO` copies the whole database and can take a while.
            let snapshot = crate::blocking::run(|| {
                let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
                crate::backup::create_database_snapshot(&store, "export", now)
            });
            let path = match snapshot {
                Ok(path) => path,
                Err(message) => {
                    return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
                }
            };
            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    let _ = tokio::fs::remove_file(&path).await;
                    return error_body(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("could not read the snapshot: {error}"),
                    );
                }
            };
            let _ = tokio::fs::remove_file(&path).await;
            attachment(
                axum::body::Body::from(bytes),
                "application/vnd.sqlite3",
                &crate::backup::database_filename(now),
            )
        }
        _ => error_body(
            StatusCode::BAD_REQUEST,
            "format must be json, sqlite or db.",
        ),
    }
}

fn attachment(body: axum::body::Body, content_type: &str, filename: &str) -> Response {
    use axum::http::HeaderValue;

    let mut response = Response::new(body);
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn post_backup(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > crate::backup::BACKUP_MAX_BYTES {
        return error_body(
            StatusCode::PAYLOAD_TOO_LARGE,
            "The backup file is too large.",
        );
    }
    let raw: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return error_body(StatusCode::BAD_REQUEST, "The body must be valid JSON."),
    };

    let now = crate::store::now_ms();
    let parsed = crate::backup::parse_backup(&raw, now);
    if !parsed.ok {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "ok": false,
                "error": "This file is not a valid backup for this worker.",
                "fields": parsed.errors,
            })),
        )
            .into_response();
    }
    let Some(value) = parsed.value else {
        return error_body(StatusCode::BAD_REQUEST, "The backup has no data.");
    };

    let restore_credentials = params
        .get("credentials")
        .map(|flag| !matches!(flag.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .unwrap_or(true);

    // A restore rewrites every table (and writes a safety snapshot first), so
    // it runs on a blocking-friendly path instead of parking a tokio worker.
    let restored = crate::blocking::run(|| {
        let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        let previous_token = store.state.session.access_token.clone();
        crate::backup::restore_backup(&mut store, &value, restore_credentials, true, now)
            .map(|result| (previous_token, result))
    });
    let (previous_token, restored) = match restored {
        Ok(restored) => restored,
        Err(message) => {
            state
                .logger
                .error("backup", format!("restore failed: {message}"));
            return error_body(StatusCode::INTERNAL_SERVER_ERROR, &message);
        }
    };

    // The restore replaced every target and the market cache, so the engine's
    // live copy of that world is no longer valid.
    crate::blocking::run(|| {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        let mut engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        engine.reset_market_state();
        engine.hydrate_market(&store);
        for target in store.state.configs.clone() {
            engine.refresh_trend(&store, &target.item_id);
        }
    });
    state.wake_keepalive("backup restored");
    state.logger.info(
        "backup",
        format!(
            "restored {} targets and {} loot entries",
            restored.targets, restored.orders
        ),
    );

    let token_changed = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        previous_token != store.state.session.access_token
    };
    if restore_credentials && restored.credentials_restored && token_changed {
        let gateway = state
            .gateway
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(gateway) = gateway {
            gateway.reconnect("backup restored").await;
        }
    }

    state.broadcast("history-cleared", json!({}));
    let snapshot = crate::blocking::run(|| {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        let engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        state.snapshot(&store, &engine)
    });
    state.broadcast("snapshot", snapshot);

    Json(json!({
        "ok": true,
        "restored": {
            "counts": {
                "targets": restored.targets,
                "orders": restored.orders,
                "reportProducts": restored.report_products,
            },
            "credentialsRestored": restored.credentials_restored,
            "safetyBackup": restored.safety_backup,
        },
        "warnings": parsed.warnings,
    }))
    .into_response()
}

async fn item_info(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(item_id) = validation::normalize_item_id(&Value::String(
        params.get("itemId").cloned().unwrap_or_default(),
    )) else {
        return error_body(
            StatusCode::UNPROCESSABLE_ENTITY,
            "A valid itemId is required.",
        );
    };
    let item_type = params
        .get("itemType")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "mutation_component".to_string());
    let valid_type = item_type.len() >= 2
        && item_type.len() <= 40
        && item_type
            .chars()
            .next()
            .map(|ch| ch.is_ascii_alphabetic())
            .unwrap_or(false)
        && item_type
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if !valid_type {
        return error_body(StatusCode::UNPROCESSABLE_ENTITY, "Invalid itemType.");
    }

    let (credentials, configured) = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        let configured = store.find_config(&item_id).is_some();
        (
            crate::session::credentials(&store.state.session),
            configured,
        )
    };

    let result = state
        .item_info
        .preview(&credentials, &item_id, &item_type)
        .await;
    match result {
        Ok(value) => {
            if configured {
                let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
                store.apply_item_info(
                    &item_id,
                    &value.label,
                    &value.item_type,
                    value.rarity_color.as_deref(),
                );
                let view = state.configs_view(
                    &store,
                    &state.engine.lock().unwrap_or_else(|e| e.into_inner()),
                );
                drop(store);
                state.broadcast("configs", Value::Array(view));
            }
            Json(json!({
                "ok": true,
                "item": {
                    "itemId": value.item_id,
                    "itemName": value.item_name,
                    "rarity": value.rarity,
                    "rarityColor": value.rarity_color,
                    "itemType": value.item_type,
                    "label": value.label,
                }
            }))
            .into_response()
        }
        Err(message) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "ok": false, "error": message })),
        )
            .into_response(),
    }
}

async fn push_session(
    _auth: Auth,
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let patch = match validation::validate_session_input(&body) {
        Ok(patch) => patch,
        Err(fields) => {
            return fields_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid session data.",
                fields,
            );
        }
    };
    let Some(access_token) = patch.access_token.clone() else {
        return error_body(StatusCode::BAD_REQUEST, "accessToken is required.");
    };
    let refresh_token = patch.refresh_token.clone().unwrap_or_default();

    let (accepted, session) = {
        let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        let accepted = crate::session::merge_pushed(
            &mut store,
            &access_token,
            &refresh_token,
            crate::store::now_ms(),
        );
        let session = json!({
            "accessToken": store.state.session.access_token,
            "refreshToken": store.state.session.refresh_token,
        });
        (accepted, session)
    };
    if accepted {
        state.wake_keepalive("bridge session");
        let status = {
            let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
            store.session_status()
        };
        state.broadcast("session", status);
        // A bridge push replaces the live token, so the feed must re-handshake
        // or it keeps running (and eventually failing) on the previous one.
        reconnect_gateway(&state, "bridge session", true).await;
    }

    Json(json!({ "ok": true, "accepted": accepted, "session": session })).into_response()
}

async fn refresh_session_endpoint(_auth: Auth, State(state): State<Arc<AppState>>) -> Response {
    let refreshed = crate::runtime::refresh_session(&state).await;
    let status = {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.session_status()
    };
    Json(json!({ "ok": true, "refreshed": refreshed, "session": status })).into_response()
}

async fn not_ported() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "ok": false,
            "error": "This endpoint is not ported to the Rust worker yet. See docs/rust-parity.md."
        })),
    )
        .into_response()
}

// ── control socket ──────────────────────────────────────────────────────────

fn protocol_auth(headers: &HeaderMap, params: &HashMap<String, String>) -> (bool, String) {
    let protocols = headers
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();

    let offered = protocols.iter().any(|value| value == "rcw.v1");
    let token = protocols
        .iter()
        .find(|value| value.as_str() != "rcw.v1")
        .cloned()
        .or_else(|| params.get("token").cloned())
        .unwrap_or_default();
    (offered, token)
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (offered, token) = protocol_auth(&headers, &params);
    let authorized = offered && {
        let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.verify_worker_token(&token)
    };
    if !offered {
        return error_body(StatusCode::BAD_REQUEST, "rcw.v1 subprotocol is required.");
    }

    ws.protocols(["rcw.v1"])
        .on_upgrade(move |socket| handle_socket(socket, state, authorized))
}

fn current_snapshot(state: &AppState) -> Value {
    let store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    let engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
    state.snapshot(&store, &engine)
}

fn gateway_snapshot(state: &AppState) -> Option<Value> {
    state
        .gateway
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|gateway| gateway.stats_json())
}

fn system_snapshot(state: &AppState) -> Value {
    state
        .system
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .snapshot()
}

fn latency_snapshot(state: &AppState) -> Value {
    state
        .latency
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .snapshot()
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, authorized: bool) {
    if !authorized {
        let mut socket = socket;
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: 4001,
                reason: "invalid access key".into(),
            })))
            .await;
        state.logger.warn(
            "control",
            "control socket rejected (missing or invalid access key)",
        );
        return;
    }

    let (mut sender, mut receiver) = socket.split();
    let mut updates = state.hub.subscribe();
    let mut auth_rx = state.auth_epoch.subscribe();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.tick().await; // the first tick is immediate
    // Live counters belong to the socket, not to a global clock. All three
    // panels are pushed together on `TELEMETRY_INTERVAL_SECS`: pushing
    // `gateway` on every frame re-rendered the HUD continuously for no benefit.
    // At 3 s the `eventCount` readout moves faster than the upstream frame
    // cadence while the heavy `system` payload still goes out at a sane rate.
    let mut telemetry_tick = tokio::time::interval(Duration::from_secs(TELEMETRY_INTERVAL_SECS));
    // `Burst` (the default) would fire every skipped tick back to back after a
    // pause, flooding the socket with three frames in a row. `Delay` keeps the
    // spacing intact and simply drops the missed slots.
    telemetry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    telemetry_tick.tick().await; // the first tick is immediate
    let mut alive = true;

    let hello = {
        let mut snapshot = current_snapshot(&state);
        snapshot["ok"] = Value::Bool(false);
        let hello = json!({
            "type": "hello",
            "data": {
                "name": APP_NAME,
                "version": APP_VERSION,
                "serverTime": epoch_ms(),
                "recentLogs": state.logger.recent(80),
            },
            "at": epoch_ms(),
        });
        (hello, snapshot)
    };

    let (hello, snapshot) = hello;
    if sender
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    let snapshot_frame = json!({ "type": "snapshot", "data": snapshot, "at": epoch_ms() });
    if sender
        .send(Message::Text(snapshot_frame.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                if let Some(Ok(_)) = &incoming {
                    alive = true;
                }
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if text.len() > 4096 {
                            continue;
                        }
                        let Ok(message) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        match message.get("type").and_then(Value::as_str) {
                            Some("ping") => {
                                let pong = json!({
                                    "type": "pong",
                                    "data": { "t": message.get("t").cloned().unwrap_or(Value::Null), "now": epoch_ms() },
                                    "at": epoch_ms(),
                                });
                                if sender.send(Message::Text(pong.to_string().into())).await.is_err() {
                                    break;
                                }
                            }
                            Some("refresh") => {
                                let snapshot = current_snapshot(&state);
                                let frame = json!({ "type": "snapshot", "data": snapshot, "at": epoch_ms() });
                                if sender.send(Message::Text(frame.to_string().into())).await.is_err() {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            update = updates.recv() => {
                match update {
                    Ok(payload) => {
                        // The envelope was serialised once by `broadcast`; this
                        // is a handle clone, not a re-encoding of the payload.
                        let Ok(text) = Utf8Bytes::try_from(payload) else {
                            continue;
                        };
                        if sender.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // Rebuilding a full snapshot for one slow socket walked
                        // every target's 24 h history. Ask that deck to refresh
                        // over HTTP instead; its state is event-driven anyway.
                        state.logger.debug(
                            "socket",
                            format!("control socket lagged by {skipped} frame(s); requesting a resync"),
                        );
                        let frame = format!(
                            r#"{{"type":"resync","data":{{"reason":"lag","skipped":{skipped}}},"at":{}}}"#,
                            epoch_ms()
                        );
                        if sender.send(Message::Text(frame.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = telemetry_tick.tick() => {
                if let Some(gateway) = gateway_snapshot(&state) {
                    let frame = json!({ "type": "gateway", "data": gateway, "at": epoch_ms() });
                    if sender.send(Message::Text(frame.to_string().into())).await.is_err() {
                        break;
                    }
                }
                let system = json!({ "type": "system", "data": system_snapshot(&state), "at": epoch_ms() });
                if sender.send(Message::Text(system.to_string().into())).await.is_err() {
                    break;
                }
                let latency = json!({ "type": "latency", "data": latency_snapshot(&state), "at": epoch_ms() });
                if sender.send(Message::Text(latency.to_string().into())).await.is_err() {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                // Same 30 s liveness probe the Bun worker ran over every
                // control socket; it also keeps idle connections warm.
                if !alive {
                    break;
                }
                alive = false;
                if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            _ = auth_rx.changed() => {
                // 4003 tells the deck to reconnect immediately (a key rotation
                // invalidates the current socket); 4001 stays for bad keys.
                let _ = sender
                    .send(Message::Close(Some(CloseFrame {
                        code: 4003,
                        reason: "access key rotated".into(),
                    })))
                    .await;
                break;
            }
        }
    }
}

// ── router ──────────────────────────────────────────────────────────────────

pub fn router(state: Arc<AppState>, dist_dir: std::path::PathBuf) -> Router {
    let index = dist_dir.join("index.html");
    let static_files = ServeDir::new(&dist_dir).not_found_service(ServeFile::new(index));

    Router::new()
        .route("/api/health", get(health))
        .route("/api/state", get(state_view))
        .route(
            "/api/configs",
            get(get_configs).put(put_configs).post(post_config),
        )
        .route(
            "/api/configs/{item_id}",
            patch(patch_config).delete(delete_config),
        )
        .route("/api/settings", patch(patch_settings))
        .route("/api/control", post(control))
        .route("/api/history", get(history).delete(delete_history))
        .route("/api/report", get(report))
        .route("/api/events", get(events_view))
        .route("/api/price-history", get(price_history_view))
        .route(
            "/api/session",
            get(get_session).post(post_session).delete(delete_session),
        )
        .route("/api/auth/rotate", post(rotate_auth))
        .route(
            "/api/backup",
            get(get_backup)
                .post(post_backup)
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::backup::BACKUP_MAX_BYTES,
                )),
        )
        .route("/api/latency", get(latency_view))
        .route("/api/latency/probe", post(latency_probe))
        .route("/api/system", get(system_view))
        .route("/api/item-info", get(item_info))
        .route("/api/session/push", post(push_session))
        .route("/api/session/refresh", post(refresh_session_endpoint))
        .route("/ws", get(ws_handler))
        .fallback_service(static_files)
        .layer(axum::middleware::from_fn(security_headers))
        .layer(CorsLayer::permissive())
        .layer(axum::extract::DefaultBodyLimit::max(Limits::BODY_BYTES))
        .with_state(state)
}

/// Security headers the Bun worker sends on every response, plus the CSP it
/// adds to HTML documents. The deck's CSP allows Google Fonts and the control
/// WebSocket, nothing else.
const HTML_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' data: https://fonts.gstatic.com; img-src 'self' data:; connect-src 'self' ws: wss:; media-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use axum::http::HeaderValue;

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );

    let is_html = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("text/html"))
        .unwrap_or(false);
    if is_html {
        headers.insert(
            "content-security-policy",
            HeaderValue::from_static(HTML_CSP),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_envelope_keeps_its_wire_shape() {
        let frame = serde_json::to_string(&Envelope {
            kind: "market",
            data: &json!({ "itemId": "abc", "hot": true }),
            at: 42,
        })
        .unwrap();
        assert_eq!(
            frame,
            r#"{"type":"market","data":{"hot":true,"itemId":"abc"},"at":42}"#
        );
    }

    #[test]
    fn extracts_the_host_from_the_upstream_url() {
        assert_eq!(
            upstream_host("wss://nws.rollercoin.com/"),
            "nws.rollercoin.com"
        );
        assert_eq!(upstream_host("wss://example.com"), "example.com");
    }

    #[test]
    fn protocol_auth_accepts_the_token_as_second_protocol() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            "rcw.v1, super-secret-key".parse().unwrap(),
        );
        let (offered, token) = protocol_auth(&headers, &HashMap::new());
        assert!(offered);
        assert_eq!(token, "super-secret-key");
    }

    #[test]
    fn protocol_auth_falls_back_to_the_query_parameter() {
        let headers = HeaderMap::new();
        let mut params = HashMap::new();
        params.insert("token".to_string(), "from-query".to_string());
        let (offered, token) = protocol_auth(&headers, &params);
        assert!(!offered);
        assert_eq!(token, "from-query");
    }
}
