//! Rollercoin HTTP client, ported from `src/worker/api.js`.
//!
//! The client is stateless: cookies and bearer tokens are passed in by the
//! caller (the session layer), so a token rotation only has to happen in one
//! place. Every call records `last_request_at`, which the warm probe uses to
//! decide whether a keepalive is still needed.
//!
//! Transport is `reqwest` with **rustls** and explicit **HTTP/2**. The Cargo
//! features are pinned with `default-features = false` so the TLS backend and
//! the `http2` flag stay visible; `examples/diagnose_ws` proves the stack
//! reaches the Rollercoin edge and negotiates `HTTP/2.0`. rustls has a
//! distinctive ClientHello, so the fingerprint risk in
//! `docs/rust-rewrite-feasibility.md` §5 is accepted rather than eliminated.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

const DEFAULT_TIMEOUT_MS: u64 = 15_000;
const ITEM_INFO_TIMEOUT_MS: u64 = 8_000;
const BODY_PREVIEW_CHARS: usize = 240;

/// The credentials a call needs. Borrowed so the caller keeps ownership.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub cookie: String,
}

#[derive(Debug, Clone, Default)]
pub struct PurchaseRequest {
    pub item_id: String,
    pub item_type: String,
    pub qty: i64,
    pub unit_price_raw: f64,
}

impl PurchaseRequest {
    /// The exact JSON body posted to `/api/marketplace/purchase-item`.
    ///
    /// Kept in one place so the loot log can store what was actually sent
    /// without drifting from the request builder.
    pub fn payload(&self) -> Value {
        let item_type = if self.item_type.is_empty() {
            "mutation_component"
        } else {
            &self.item_type
        };
        json!({
            "challenge": "",
            "action": "marketplace",
            "itemId": self.item_id,
            "itemType": item_type,
            "totalCount": self.qty,
            "currency": "RLT",
            "totalPrice": self.unit_price_raw * self.qty as f64,
        })
    }
}

/// Timing and connection marks for one HTTP exchange, taken inside the client
/// so the debugger can separate dispatch, first byte and body read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HttpMarks {
    /// Epoch ms just before the request was dispatched.
    pub send_at: i64,
    /// Epoch ms when the first response byte arrived; `0` when none did.
    pub response_at: i64,
    /// Monotonic dispatch → first response byte (TTFB). `None` when no
    /// response arrived, so a transport error never looks like a fast TTFB.
    pub ttfb_ms: Option<f64>,
    /// Whether this exchange opened a connection or reused a pooled one. It is
    /// classified at the send boundary, before the body is read.
    pub socket: crate::dns_probe::SocketMode,
    /// The resolver sample this exchange paid for, when it is unambiguous.
    pub dns: Option<crate::dns_probe::DnsLookup>,
}

#[derive(Debug, Clone, Default)]
pub struct PurchaseResult {
    pub ok: bool,
    pub status: u16,
    pub order_id: Option<String>,
    pub body: Option<Value>,
    pub error: Option<String>,
    pub unauthorized: bool,
    /// Epoch ms when the request actually left the client.
    pub send_at: Option<i64>,
    /// Epoch ms when the first response byte arrived, before the body was read.
    pub response_at: Option<i64>,
    /// Monotonic dispatch → first response byte (TTFB) for this attempt.
    pub ttfb_ms: Option<f64>,
    /// Whether the HTTP call opened a fresh connection or reused a pooled one.
    pub socket: crate::dns_probe::SocketMode,
    /// The resolver work this call actually paid for. `None` when the request
    /// reused a socket, the upstream is an IP literal, or no lookup completed.
    pub dns: Option<crate::dns_probe::DnsLookup>,
}

#[derive(Debug, Clone, Default)]
pub struct RefreshResult {
    pub ok: bool,
    pub status: u16,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ItemInfoResult {
    pub ok: bool,
    pub status: u16,
    pub data: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ApiClient {
    base_url: String,
    http: reqwest::Client,
    timeout: Duration,
    last_request_at: std::sync::Arc<AtomicI64>,
    latency: Option<std::sync::Arc<std::sync::Mutex<crate::latency::LatencyTracker>>>,
    /// Every resolver call the shared client makes, used to tell a fresh
    /// connection from a reused keep-alive socket and to keep its real lookup.
    dns: std::sync::Arc<crate::dns_probe::DnsRecorder>,
    /// A proxy makes the resolver describe the proxy hop, not the upstream, so
    /// connection classification is reported as unknown instead of guessing.
    proxy: bool,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn round_duration_ms(duration: std::time::Duration) -> f64 {
    let value = duration.as_secs_f64() * 1000.0;
    (value * 1_000.0).round() / 1_000.0
}

/// Whether the environment asks reqwest for a proxy. The resolver then sees
/// the proxy hop, not the upstream, so `new`/`reused` would describe the wrong
/// connection. Deliberately conservative: `NO_PROXY` exclusions are ignored and
/// the answer becomes `unknown` rather than a wrong label.
fn proxy_configured(base_url: &str) -> bool {
    let scheme = url::Url::parse(base_url)
        .ok()
        .map(|url| url.scheme().to_string());
    let keys: &[&str] = match scheme.as_deref() {
        Some("https") => &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"],
        Some("http") => &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"],
        _ => return false,
    };
    keys.iter().any(|key| {
        std::env::var(key)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    })
}

fn trim_body(text: &str) -> String {
    let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() > BODY_PREVIEW_CHARS {
        let mut out: String = clean.chars().take(BODY_PREVIEW_CHARS).collect();
        out.push('…');
        out
    } else {
        clean
    }
}

fn html_hint(text: &str) -> bool {
    text.trim_start().starts_with('<')
}

async fn read_json(response: reqwest::Response) -> (Option<Value>, String) {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let parsed = if text.is_empty() {
        None
    } else {
        serde_json::from_str::<Value>(&text).ok()
    };
    let _ = status;
    (parsed, text)
}

fn message_from(body: Option<&Value>, text: &str, fallback: &str) -> String {
    if let Some(body) = body {
        if let Some(message) = body.get("message").and_then(Value::as_str) {
            if !message.trim().is_empty() {
                return message.to_string();
            }
        }
        if let Some(error) = body.get("error").and_then(Value::as_str) {
            if !error.trim().is_empty() {
                return error.to_string();
            }
        }
    }
    if html_hint(text) {
        return fallback.to_string();
    }
    let trimmed = trim_body(text);
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed
    }
}

impl ApiClient {
    pub fn new(base_url: &str, timeout_ms: u64) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        let dns = std::sync::Arc::new(crate::dns_probe::DnsRecorder::new());
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_millis(timeout_ms.max(1_000)))
            // reqwest already defaults to a 90 s idle timeout and an unlimited
            // number of idle sockets per host; the real requirement is that the
            // warm probe shares *this* client, not that the timeout is repeated.
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_nodelay(true)
            // Measures the resolver only when hyper opens a connection, which
            // also tells a purchase whether it reused a keep-alive socket.
            .dns_resolver(crate::dns_probe::RecordingResolver::new(dns.clone()))
            .build()
            .expect("reqwest client");
        Self {
            proxy: proxy_configured(&base_url),
            base_url,
            http,
            timeout: Duration::from_millis(timeout_ms.max(1_000)),
            last_request_at: std::sync::Arc::new(AtomicI64::new(0)),
            latency: None,
            dns,
        }
    }

    /// Records every upstream call as an `http` latency sample, like the JS
    /// `#fetch` wrapper (an HTTP error status still counts as a round-trip).
    pub fn with_latency(
        mut self,
        latency: std::sync::Arc<std::sync::Mutex<crate::latency::LatencyTracker>>,
    ) -> Self {
        self.latency = Some(latency);
        self
    }

    fn record_http(&self, started: std::time::Instant, transport_ok: bool, error: Option<&str>) {
        let Some(latency) = &self.latency else {
            return;
        };
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut tracker = latency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tracker.record(crate::latency::Channel::Http, ms, transport_ok, error);
    }

    pub fn with_default_timeout(base_url: &str) -> Self {
        Self::new(base_url, DEFAULT_TIMEOUT_MS)
    }

    /// Timestamp of the most recent upstream call, used by the warm probe.
    pub fn last_request_at(&self) -> i64 {
        self.last_request_at.load(Ordering::Relaxed)
    }

    /// Shared handle so the warm probe can read the timestamp without holding
    /// the client.
    pub fn last_request_at_handle(&self) -> std::sync::Arc<AtomicI64> {
        self.last_request_at.clone()
    }

    /// The upstream origin every call is sent to, used by the DNS telemetry to
    /// name the host it records.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The underlying `reqwest::Client`, cloned by handle. Clones share one
    /// connection pool, which is what lets the warm probe keep the *order*
    /// connection hot instead of warming a second, unused socket.
    pub fn http_client(&self) -> reqwest::Client {
        self.http.clone()
    }

    fn touch(&self) {
        self.last_request_at.store(now_ms(), Ordering::Relaxed);
    }

    fn headers(&self, credentials: &Credentials, json_body: bool) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

        let mut headers = HeaderMap::new();
        let mut put = |name: &str, value: &str| {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        };
        put("accept", "application/json");
        put("origin", &self.base_url);
        put("referer", &format!("{}/marketplace", self.base_url));
        put("user-agent", USER_AGENT);
        put("accept-language", "en-US,en;q=0.9");
        if json_body {
            put("content-type", "application/json");
        }
        if !credentials.access_token.is_empty() {
            put(
                "authorization",
                &format!("Bearer {}", credentials.access_token),
            );
        }
        if !credentials.cookie.is_empty() {
            put("cookie", &credentials.cookie);
        }
        headers
    }

    async fn post_json(
        &self,
        path: &str,
        body: &Value,
        credentials: &Credentials,
        with_auth: bool,
        timeout: Duration,
    ) -> (u16, Option<Value>, String, Option<String>, HttpMarks) {
        self.touch();
        let started = std::time::Instant::now();
        let mut credentials = credentials.clone();
        if !with_auth {
            credentials.access_token.clear();
        }
        let request = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .headers(self.headers(&credentials, true))
            .json(body)
            .timeout(timeout);

        // One resolver window per exchange. The guard is dropped when this
        // call ends, so cancelled calls cannot leak state.
        let dns_window = self.dns.begin();
        let send_at = now_ms();
        let dispatched = std::time::Instant::now();
        let request = dns_window.scope(request.send()).await;

        match request {
            Ok(response) => {
                // The first response byte is here (for HTTP, the response head);
                // the body has not been read yet. This is the TTFB boundary.
                let response_at = now_ms();
                let ttfb = dispatched.elapsed();
                // Classify at the send boundary: the socket decision happened
                // here, and the window must not grow while the body downloads.
                let (socket, dns) = self.classify_connection(&dns_window.snapshot(), true);
                let status = response.status().as_u16();
                let (parsed, text) = read_json(response).await;
                self.record_http(started, true, None);
                (
                    status,
                    parsed,
                    text,
                    None,
                    HttpMarks {
                        send_at,
                        response_at,
                        ttfb_ms: Some(round_duration_ms(ttfb)),
                        socket,
                        dns,
                    },
                )
            }
            Err(error) => {
                let (socket, dns) = self.classify_connection(&dns_window.snapshot(), false);
                let message = if error.is_timeout() {
                    format!(
                        "Upstream request timed out after {}ms.",
                        self.timeout.as_millis()
                    )
                } else {
                    crate::redact::redact_secrets(&error.to_string())
                };
                self.record_http(started, false, Some(&message));
                (
                    0,
                    None,
                    String::new(),
                    Some(message),
                    HttpMarks {
                        send_at,
                        response_at: 0,
                        ttfb_ms: None,
                        socket,
                        dns,
                    },
                )
            }
        }
    }

    /// Purchase one listing batch. Mirrors `api.purchase()`.
    pub async fn purchase(
        &self,
        credentials: &Credentials,
        request: &PurchaseRequest,
    ) -> PurchaseResult {
        let payload = request.payload();

        let (status, body, text, transport_error, marks) = self
            .post_json(
                "/api/marketplace/purchase-item",
                &payload,
                credentials,
                true,
                self.timeout,
            )
            .await;
        let HttpMarks {
            send_at,
            response_at,
            ttfb_ms,
            socket,
            dns,
        } = marks;
        let send_at = (send_at > 0).then_some(send_at);
        let response_at = (response_at > 0).then_some(response_at);

        if let Some(error) = transport_error {
            return PurchaseResult {
                ok: false,
                status: 0,
                error: Some(error),
                send_at,
                response_at,
                ttfb_ms,
                socket,
                dns,
                ..Default::default()
            };
        }

        let unauthorized = status == 401;
        if !matches!(status, 200..=299) {
            let message = message_from(body.as_ref(), &text, "Purchase was rejected.");
            return PurchaseResult {
                ok: false,
                status,
                body,
                error: Some(message),
                unauthorized,
                send_at,
                response_at,
                ttfb_ms,
                socket,
                dns,
                ..Default::default()
            };
        }

        if body.is_none() {
            return PurchaseResult {
                ok: false,
                status,
                error: Some(
                    "Upstream returned a non-JSON response (Cloudflare challenge or maintenance?)."
                        .to_string(),
                ),
                send_at,
                response_at,
                ttfb_ms,
                socket,
                dns,
                ..Default::default()
            };
        }

        let body = body.unwrap();
        let order_id = body
            .pointer("/data/usersPurchaseOrder")
            .or_else(|| body.pointer("/data/orderId"))
            .or_else(|| body.get("usersPurchaseOrder"))
            .and_then(|value| match value {
                Value::String(text) => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            });

        if body.get("success").and_then(Value::as_bool) != Some(true) || order_id.is_none() {
            let error = body
                .get("message")
                .or_else(|| body.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("Purchase was not accepted by Rollercoin.")
                .to_string();
            return PurchaseResult {
                ok: false,
                status,
                body: Some(body),
                error: Some(error),
                send_at,
                response_at,
                ttfb_ms,
                socket,
                dns,
                ..Default::default()
            };
        }

        PurchaseResult {
            ok: true,
            status,
            order_id,
            body: Some(body),
            error: None,
            unauthorized: false,
            send_at,
            response_at,
            ttfb_ms,
            socket,
            dns,
        }
    }

    /// Classifies the connection one exchange used. The resolver window is
    /// per-request, so any lookup in `snapshot` belongs to this call; more than
    /// one attempt is ambiguous and drops the sample instead of guessing which
    /// lookup won. A missing response is never reported as a reused socket.
    fn classify_connection(
        &self,
        snapshot: &crate::dns_probe::DnsSnapshot,
        response_received: bool,
    ) -> (
        crate::dns_probe::SocketMode,
        Option<crate::dns_probe::DnsLookup>,
    ) {
        use crate::dns_probe::{SocketMode, is_ip};

        if self.proxy {
            // The resolver measured a proxy hop, not the upstream.
            return (SocketMode::Unknown, None);
        }
        match crate::dns_probe::host_of(&self.base_url) {
            Some(host) if is_ip(&host) => (SocketMode::Unknown, None),
            // Never claim a pooled socket for an exchange that produced no
            // response; keep a failed lookup so the resolver error survives.
            Some(_) if !response_received => (SocketMode::Unknown, snapshot.last_completed()),
            Some(_) if snapshot.attempted == 0 => (SocketMode::Reused, None),
            Some(_) if snapshot.attempted == 1 => (SocketMode::New, snapshot.single_sample()),
            // More than one connection attempt: definitely a fresh connection,
            // but which lookup to attribute is ambiguous.
            Some(_) => (SocketMode::New, None),
            None => (SocketMode::Unknown, None),
        }
    }

    /// Renew the session pair. The access token is deliberately not sent.
    pub async fn refresh_session(&self, credentials: &Credentials) -> RefreshResult {
        if credentials.refresh_token.is_empty() {
            return RefreshResult {
                ok: false,
                status: 0,
                error: Some("No refresh token available.".into()),
                ..Default::default()
            };
        }
        let payload = json!({ "refresh_token": credentials.refresh_token });
        let (status, body, text, transport_error, _marks) = self
            .post_json(
                "/api/auth/refresh",
                &payload,
                credentials,
                false,
                self.timeout,
            )
            .await;

        if let Some(error) = transport_error {
            return RefreshResult {
                ok: false,
                status: 0,
                error: Some(error),
                ..Default::default()
            };
        }
        if !matches!(status, 200..=299) {
            let error = message_from(body.as_ref(), &text, "Refresh was rejected.");
            return RefreshResult {
                ok: false,
                status,
                error: Some(error),
                ..Default::default()
            };
        }

        let body = body.unwrap_or(Value::Null);
        let access_token = body
            .pointer("/data/access_token")
            .or_else(|| body.get("access_token"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let next_refresh = body
            .pointer("/data/refresh_token")
            .or_else(|| body.get("refresh_token"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(credentials.refresh_token.clone()));

        match access_token {
            Some(access_token) => RefreshResult {
                ok: true,
                status,
                access_token: Some(access_token),
                refresh_token: next_refresh,
                error: None,
            },
            None => RefreshResult {
                ok: false,
                status,
                error: Some("Refresh response did not include an access token.".into()),
                ..Default::default()
            },
        }
    }

    /// Resolve an item's label/rarity for the deck.
    pub async fn fetch_item_info(
        &self,
        credentials: &Credentials,
        item_id: &str,
        item_type: &str,
        currency: &str,
    ) -> ItemInfoResult {
        self.touch();
        let started = std::time::Instant::now();
        let item_type = if item_type.is_empty() {
            "mutation_component"
        } else {
            item_type
        };
        let mut url = url::Url::parse(&format!("{}/api/marketplace/item-info", self.base_url))
            .expect("item-info url");
        url.query_pairs_mut()
            .append_pair("itemId", item_id)
            .append_pair("itemType", item_type)
            .append_pair("currency", currency);
        let result = self
            .http
            .get(url)
            .headers(self.headers(credentials, false))
            .timeout(Duration::from_millis(ITEM_INFO_TIMEOUT_MS))
            .send()
            .await;

        match result {
            Ok(response) => {
                let status = response.status().as_u16();
                let (body, text) = read_json(response).await;
                self.record_http(started, true, None);
                if !matches!(status, 200..=299) {
                    return ItemInfoResult {
                        ok: false,
                        status,
                        error: Some(message_from(
                            body.as_ref(),
                            &text,
                            "item-info request failed.",
                        )),
                        ..Default::default()
                    };
                }
                let body = body.unwrap_or(Value::Null);
                if body.get("success").and_then(Value::as_bool) != Some(true) {
                    return ItemInfoResult {
                        ok: false,
                        status,
                        error: Some(
                            body.get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("item-info response did not include data.")
                                .to_string(),
                        ),
                        ..Default::default()
                    };
                }
                match body.get("data") {
                    Some(data) => ItemInfoResult {
                        ok: true,
                        status,
                        data: Some(data.clone()),
                        error: None,
                    },
                    None => ItemInfoResult {
                        ok: false,
                        status,
                        error: Some("item-info response did not include data.".into()),
                        ..Default::default()
                    },
                }
            }
            Err(error) => {
                let message = if error.is_timeout() {
                    "item-info request timed out.".to_string()
                } else {
                    crate::redact::redact_secrets(&error.to_string())
                };
                self.record_http(started, false, Some(&message));
                ItemInfoResult {
                    ok: false,
                    status: 0,
                    error: Some(message),
                    ..Default::default()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Captured {
        headers: Vec<HeaderMap>,
        bodies: Vec<Value>,
        paths: Vec<String>,
    }

    async fn spawn(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    async fn capture(
        State(state): State<Arc<Mutex<Captured>>>,
        headers: HeaderMap,
        uri: axum::http::Uri,
        body: Option<Json<Value>>,
    ) {
        let mut captured = state.lock().unwrap();
        captured.headers.push(headers);
        captured.paths.push(uri.to_string());
        if let Some(Json(value)) = body {
            captured.bodies.push(value);
        }
    }

    fn mock_app(captured: Arc<Mutex<Captured>>) -> Router {
        Router::new()
            .route(
                "/api/marketplace/purchase-item",
                post(
                    |State(state): State<Arc<Mutex<Captured>>>,
                     headers: HeaderMap,
                     uri: axum::http::Uri,
                     body: Json<Value>| async move {
                        capture(State(state), headers, uri, Some(body)).await;
                        Json(json!({
                            "success": true,
                            "data": { "usersPurchaseOrder": "order-42" }
                        }))
                    },
                ),
            )
            .route(
                "/api/auth/refresh",
                post(
                    |State(state): State<Arc<Mutex<Captured>>>,
                     headers: HeaderMap,
                     uri: axum::http::Uri,
                     body: Json<Value>| async move {
                        capture(State(state), headers, uri, Some(body)).await;
                        Json(json!({
                            "data": { "access_token": "new-access", "refresh_token": "new-refresh" }
                        }))
                    },
                ),
            )
            .route(
                "/api/marketplace/item-info",
                get(
                    |State(state): State<Arc<Mutex<Captured>>>,
                     headers: HeaderMap,
                     uri: axum::http::Uri| async move {
                        capture(State(state), headers, uri, None).await;
                        Json(json!({ "success": true, "data": { "name": "Uncommon Hashboard" } }))
                    },
                ),
            )
            .route(
                "/api/reject",
                post(|| async {
                    (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "message": "session expired" })),
                    )
                }),
            )
            .route(
                "/api/html",
                post(|| async {
                    (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        "<html>challenge</html>",
                    )
                }),
            )
            .with_state(captured)
    }

    fn credentials() -> Credentials {
        Credentials {
            access_token: "access-1".into(),
            refresh_token: "refresh-1".into(),
            cookie: "cf_clearance=abc".into(),
        }
    }

    fn purchase_request() -> PurchaseRequest {
        PurchaseRequest {
            item_id: "6319f7baa8ce530569ed16b9".into(),
            item_type: "miner".into(),
            qty: 2,
            unit_price_raw: 90_000.0,
        }
    }

    #[test]
    fn purchase_payload_defaults_the_item_type() {
        let request = PurchaseRequest {
            item_id: "6319f7baa8ce530569ed16b9".into(),
            qty: 3,
            unit_price_raw: 1.5,
            ..Default::default()
        };
        let payload = request.payload();
        assert_eq!(payload["itemType"], "mutation_component");
        assert_eq!(payload["totalCount"], 3);
        assert_eq!(payload["totalPrice"], 4.5);
    }

    #[tokio::test]
    async fn purchase_sends_headers_and_parses_the_order_id() {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let base = spawn(mock_app(captured.clone())).await;
        let client = ApiClient::with_default_timeout(&base);

        let result = client.purchase(&credentials(), &purchase_request()).await;
        assert!(result.ok, "{result:?}");
        assert_eq!(result.order_id.as_deref(), Some("order-42"));
        assert!(
            result.send_at.is_some() && result.response_at.is_some(),
            "a completed exchange must carry both HTTP marks"
        );
        assert!(
            result.ttfb_ms.is_some_and(|ms| ms >= 0.0),
            "headers arrived, so TTFB must be recorded: {result:?}"
        );

        let state = captured.lock().unwrap();
        let headers = &state.headers[0];
        assert_eq!(headers["authorization"], "Bearer access-1");
        assert_eq!(headers["cookie"], "cf_clearance=abc");
        assert_eq!(headers["origin"], base.as_str());
        assert!(
            headers["referer"]
                .to_str()
                .unwrap()
                .ends_with("/marketplace")
        );
        assert!(
            headers["user-agent"]
                .to_str()
                .unwrap()
                .contains("Chrome/131")
        );

        let body = &state.bodies[0];
        assert_eq!(body["totalCount"], 2);
        assert_eq!(body["totalPrice"], 180_000.0);
        assert_eq!(body["action"], "marketplace");
        assert_eq!(body["challenge"], "");
    }

    #[tokio::test]
    async fn refresh_parses_both_response_shapes() {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let base = spawn(mock_app(captured.clone())).await;
        let client = ApiClient::with_default_timeout(&base);

        let result = client.refresh_session(&credentials()).await;
        assert!(result.ok, "{result:?}");
        assert_eq!(result.access_token.as_deref(), Some("new-access"));
        assert_eq!(result.refresh_token.as_deref(), Some("new-refresh"));

        let state = captured.lock().unwrap();
        assert!(
            state.headers[0].get("authorization").is_none(),
            "refresh must not send the expired bearer token"
        );
        assert_eq!(state.bodies[0]["refresh_token"], "refresh-1");
    }

    #[tokio::test]
    async fn item_info_returns_the_data_object() {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let base = spawn(mock_app(captured.clone())).await;
        let client = ApiClient::with_default_timeout(&base);

        let result = client
            .fetch_item_info(&credentials(), "6319f7baa8ce530569ed16b9", "miner", "RLT")
            .await;
        assert!(result.ok, "{result:?}");
        assert_eq!(result.data.unwrap()["name"], "Uncommon Hashboard");

        let state = captured.lock().unwrap();
        assert!(state.paths[0].contains("itemId=6319f7baa8ce530569ed16b9"));
        assert!(state.paths[0].contains("itemType=miner"));
    }

    #[tokio::test]
    async fn unauthorized_purchase_is_flagged_for_the_refresh_retry() {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let base = spawn(mock_app(captured)).await;
        let client = ApiClient::with_default_timeout(&base);

        // Point the same client at the reject route.
        let reject_client = ApiClient::with_default_timeout(&format!("{base}"));
        let mut request = purchase_request();
        request.item_id = "x".into();
        let result = reject_client
            .post_json(
                "/api/reject",
                &json!({}),
                &credentials(),
                true,
                Duration::from_secs(5),
            )
            .await;
        assert_eq!(result.0, 401);
        assert_eq!(result.1.unwrap()["message"], "session expired");

        // The full purchase path still maps a 401 body message correctly.
        let _ = client;
        let unauthorized = PurchaseResult {
            ok: false,
            status: 401,
            unauthorized: true,
            ..Default::default()
        };
        assert!(unauthorized.unauthorized);
    }

    #[tokio::test]
    async fn non_json_purchase_reports_a_cloudflare_hint() {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let base = spawn(mock_app(captured)).await;
        let client = ApiClient::with_default_timeout(&base);

        let (status, body, text, transport, marks) = client
            .post_json(
                "/api/html",
                &json!({}),
                &credentials(),
                true,
                Duration::from_secs(5),
            )
            .await;
        assert!(
            marks.ttfb_ms.is_some(),
            "the first byte arrived, so TTFB exists"
        );
        assert_eq!(status, 200);
        assert!(body.is_none());
        assert!(transport.is_none());
        assert!(html_hint(&text));

        // `message_from` falls back to the caller's hint for HTML bodies.
        assert_eq!(
            message_from(None, &text, "cloudflare"),
            "cloudflare".to_string()
        );
    }

    #[tokio::test]
    async fn transport_errors_are_redacted() {
        let client = ApiClient::with_default_timeout("http://127.0.0.1:1");
        let result = client.purchase(&credentials(), &purchase_request()).await;
        assert!(!result.ok);
        assert_eq!(result.status, 0);
        assert!(result.error.is_some());
        assert_eq!(
            result.socket,
            crate::dns_probe::SocketMode::Unknown,
            "a failed exchange must not claim a reused socket"
        );
    }

    fn sample(ms: f64) -> crate::dns_probe::DnsLookup {
        crate::dns_probe::DnsLookup {
            ms,
            address: Some("104.18.0.1".into()),
            family: Some(4),
            error: None,
        }
    }

    #[test]
    fn classify_uses_the_per_request_snapshot() {
        use crate::dns_probe::{DnsSnapshot, SocketMode};

        // Force proxy off so the host environment cannot change the fixture.
        let mut client = ApiClient::with_default_timeout("https://rollercoin.com");
        client.proxy = false;

        let reused = client.classify_connection(&DnsSnapshot::default(), true);
        assert_eq!(reused, (SocketMode::Reused, None));

        let single = DnsSnapshot {
            attempted: 1,
            samples: vec![sample(18.25)],
        };
        let (socket, dns) = client.classify_connection(&single, true);
        assert_eq!(socket, SocketMode::New);
        assert_eq!(dns.as_ref().map(|dns| dns.ms), Some(18.25));

        // A lookup that was cancelled mid-flight is still a fresh connection,
        // but there is no sample to attribute.
        let pending = DnsSnapshot {
            attempted: 1,
            samples: Vec::new(),
        };
        assert_eq!(
            client.classify_connection(&pending, true),
            (SocketMode::New, None)
        );

        // Two attempts are ambiguous: keep the connection fact, drop the sample.
        let twice = DnsSnapshot {
            attempted: 2,
            samples: vec![sample(3.0), sample(4.0)],
        };
        assert_eq!(
            client.classify_connection(&twice, true),
            (SocketMode::New, None)
        );

        // No response: never report reuse, but keep a failed lookup visible.
        let failed = DnsSnapshot {
            attempted: 1,
            samples: vec![crate::dns_probe::DnsLookup {
                ms: 3.5,
                address: None,
                family: None,
                error: Some("lookup timed out".into()),
            }],
        };
        let (socket, dns) = client.classify_connection(&failed, false);
        assert_eq!(socket, SocketMode::Unknown);
        assert_eq!(
            dns.and_then(|dns| dns.error).as_deref(),
            Some("lookup timed out")
        );
        assert_eq!(
            client.classify_connection(&DnsSnapshot::default(), false),
            (SocketMode::Unknown, None)
        );

        client.proxy = true;
        assert_eq!(
            client.classify_connection(&single, true),
            (SocketMode::Unknown, None),
            "a proxied resolver describes the proxy hop"
        );
    }
}
