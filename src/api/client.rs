use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::clock::now_ms;
use crate::lock::lock;

use super::parse::read_json;
use super::purchase::round_duration_ms;
use super::types::{Credentials, HttpMarks};
use super::{DEFAULT_TIMEOUT_MS, USER_AGENT};

#[derive(Debug, Clone)]
pub struct ApiClient {
    pub(super) base_url: String,
    pub(super) http: reqwest::Client,
    pub(super) timeout: Duration,
    pub(super) last_request_at: std::sync::Arc<AtomicI64>,
    pub(super) latency: Option<std::sync::Arc<std::sync::Mutex<crate::latency::LatencyTracker>>>,
    /// Every resolver call the shared client makes, used to tell a fresh
    /// connection from a reused keep-alive socket and to keep its real lookup.
    pub(super) dns: std::sync::Arc<crate::dns_probe::DnsRecorder>,
    /// A proxy makes the resolver describe the proxy hop, not the upstream, so
    /// connection classification is reported as unknown instead of guessing.
    pub(super) proxy: bool,
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

    /// Records every upstream call as an `http` latency sample (an HTTP error
    /// status still counts as a round-trip).
    pub fn with_latency(
        mut self,
        latency: std::sync::Arc<std::sync::Mutex<crate::latency::LatencyTracker>>,
    ) -> Self {
        self.latency = Some(latency);
        self
    }

    pub(super) fn record_http(
        &self,
        started: std::time::Instant,
        transport_ok: bool,
        error: Option<&str>,
    ) {
        let Some(latency) = &self.latency else {
            return;
        };
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut tracker = lock(latency);
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

    pub(super) fn touch(&self) {
        self.last_request_at.store(now_ms(), Ordering::Relaxed);
    }

    pub(super) fn headers(
        &self,
        credentials: &Credentials,
        json_body: bool,
    ) -> reqwest::header::HeaderMap {
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

    pub(super) async fn post_json(
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
}
