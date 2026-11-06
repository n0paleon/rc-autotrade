//! Warm HTTP probe, ported from `src/worker/keepalive.js`.
//!
//! Keeps the Rollercoin edge connection hot while the sniper is armed. The probe
//! POSTs an empty body to the purchase endpoint: any HTTP response proves the
//! request reached upstream, and an empty body can only be rejected by
//! validation — it can never place an order.
//!
//! It is skipped when real traffic just happened, backs off on transport
//! failures, and pauses for 5 minutes after a 429.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::gateway::CredentialsFn;
use crate::latency::{Channel, LatencyTracker};
use crate::logger::Logger;

pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_SUSPEND_MS: u64 = 5 * 60_000;
pub const MAX_BACKOFF_MS: u64 = 5 * 60_000;
pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn jittered(ms: f64, ratio: f64) -> u64 {
    let spread = ms * ratio;
    let value = ms - spread + rand::random::<f64>() * spread * 2.0;
    value.round().max(1.0) as u64
}

#[derive(Debug, Clone, Default)]
pub struct ProbeResult {
    pub ok: bool,
    pub status: u16,
    pub rtt_ms: Option<f64>,
    pub error: Option<String>,
    pub suspended_until: Option<i64>,
    pub skipped: bool,
    pub aborted: bool,
}

impl ProbeResult {
    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok,
            "status": if self.status == 0 { Value::Null } else { json!(self.status) },
            "rttMs": self.rtt_ms,
            "error": self.error,
            "suspendedUntil": self.suspended_until,
            "skipped": self.skipped,
        })
    }
}

#[derive(Debug, Clone)]
pub struct KeepaliveStats {
    pub enabled: bool,
    pub running: bool,
    pub url: String,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub probes: i64,
    pub failures: i64,
    pub consecutive_failures: i64,
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub last_skip_at: Option<i64>,
    pub last_status: Option<u16>,
    pub last_rtt_ms: Option<f64>,
    pub last_error: Option<String>,
    pub next_probe_at: Option<i64>,
    pub suspended_until: Option<i64>,
}

pub struct KeepaliveConfig {
    pub url: String,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub suspend_ms: u64,
    pub jitter_ratio: f64,
    pub credentials: CredentialsFn,
    pub should_probe: Arc<dyn Fn() -> bool + Send + Sync>,
    pub logger: Arc<Logger>,
    pub latency: Arc<Mutex<LatencyTracker>>,
    pub last_request_at: Arc<AtomicI64>,
    /// The order path's client. Probes must reuse its pool or the warm-up is
    /// pointless: a separate `Client` gets a separate socket and TLS session.
    pub http: reqwest::Client,
}

pub struct KeepaliveService {
    url: String,
    interval_ms: u64,
    timeout_ms: u64,
    suspend_ms: u64,
    jitter_ratio: f64,
    max_backoff_ms: u64,
    http: reqwest::Client,
    credentials: CredentialsFn,
    should_probe: Arc<dyn Fn() -> bool + Send + Sync>,
    logger: Arc<Logger>,
    latency: Arc<Mutex<LatencyTracker>>,
    last_request_at: Arc<AtomicI64>,
    stats: Mutex<KeepaliveStats>,
    run_lock: tokio::sync::Mutex<()>,
    wake: Notify,
    running: std::sync::atomic::AtomicBool,
}

impl KeepaliveService {
    pub fn new(config: KeepaliveConfig) -> Arc<Self> {
        let enabled = !config.url.trim().is_empty() && config.interval_ms > 0;
        let interval_ms = if config.interval_ms > 0 {
            config.interval_ms
        } else {
            30_000
        };
        let timeout_ms = config.timeout_ms.max(1_000);
        let suspend_ms = config.suspend_ms.max(1_000);
        let jitter_ratio = config.jitter_ratio.clamp(0.0, 0.5);

        Arc::new(Self {
            url: config.url.trim().to_string(),
            interval_ms,
            timeout_ms,
            suspend_ms,
            jitter_ratio,
            max_backoff_ms: interval_ms.max(MAX_BACKOFF_MS),
            http: config.http,
            credentials: config.credentials,
            should_probe: config.should_probe,
            logger: config.logger,
            latency: config.latency,
            last_request_at: config.last_request_at,
            stats: Mutex::new(KeepaliveStats {
                enabled,
                running: false,
                url: config.url.trim().to_string(),
                interval_ms,
                timeout_ms,
                probes: 0,
                failures: 0,
                consecutive_failures: 0,
                last_attempt_at: None,
                last_success_at: None,
                last_skip_at: None,
                last_status: None,
                last_rtt_ms: None,
                last_error: None,
                next_probe_at: None,
                suspended_until: None,
            }),
            run_lock: tokio::sync::Mutex::new(()),
            wake: Notify::new(),
            running: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn enabled(&self) -> bool {
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).enabled
    }

    pub fn stats_json(&self) -> Value {
        let stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        json!({
            "enabled": stats.enabled,
            "running": stats.running,
            "url": stats.url,
            "intervalMs": stats.interval_ms,
            "timeoutMs": stats.timeout_ms,
            "probes": stats.probes,
            "failures": stats.failures,
            "consecutiveFailures": stats.consecutive_failures,
            "lastAttemptAt": stats.last_attempt_at,
            "lastSuccessAt": stats.last_success_at,
            "lastSkipAt": stats.last_skip_at,
            "lastStatus": stats.last_status,
            "lastRttMs": stats.last_rtt_ms,
            "lastError": stats.last_error,
            "nextProbeAt": stats.next_probe_at,
            "suspendedUntil": stats.suspended_until,
        })
    }

    fn set_running(&self, running: bool) {
        let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        stats.running = running;
    }

    /// Starts the scheduling loop. No-op when disabled or already running.
    pub fn start(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        if !self.enabled() {
            self.logger.debug("keepalive", "warm probe disabled");
            return None;
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return None;
        }
        self.set_running(true);
        self.logger.info(
            "keepalive",
            format!("probing {} every ~{}ms", self.url, self.interval_ms),
        );

        let service = self.clone();
        Some(tokio::spawn(async move {
            // The first scheduled probe waits one interval, like
            // `#schedule(this.intervalMs)`; `wake()` can preempt it.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(service.interval_ms)) => {}
                _ = service.wake.notified() => {}
            }
            loop {
                let delay = service.tick().await;
                let wait = Duration::from_millis(delay);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = service.wake.notified() => {}
                }
            }
        }))
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.set_running(false);
        self.wake.notify_waiters();
    }

    /// Run a probe immediately instead of waiting for the next slot.
    pub fn wake(&self, reason: &str) {
        if !self.enabled() {
            return;
        }
        self.logger.debug("keepalive", format!("wake: {reason}"));
        self.wake.notify_one();
    }

    /// One scheduling decision. Returns how long to sleep before the next tick.
    /// Public for tests; the loop calls it too.
    pub async fn tick(&self) -> u64 {
        if !self.enabled() {
            return self.interval_ms;
        }
        let now = now_ms();

        let suspended_until = {
            let stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.suspended_until
        };
        if let Some(until) = suspended_until {
            if now < until {
                return (until - now).max(0) as u64;
            }
            let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.suspended_until = None;
        }

        let allowed = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.should_probe)()
        })) {
            Ok(allowed) => allowed,
            Err(_) => {
                self.logger.warn("keepalive", "shouldProbe panicked");
                false
            }
        };
        if !allowed {
            let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.last_skip_at = Some(now);
            return jittered(self.interval_ms as f64, self.jitter_ratio);
        }

        let last_real = self.last_request_at.load(Ordering::Relaxed);
        if last_real > 0 {
            let since_real = now - last_real;
            if since_real < (self.interval_ms as i64 * 9) / 10 {
                let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                stats.last_skip_at = Some(now);
                return (self.interval_ms as i64 - since_real).max(1_000) as u64;
            }
        }

        let result = self.probe_once().await;
        if let Some(until) = result.suspended_until {
            return (until - now_ms()).max(1_000) as u64;
        }
        if result.ok {
            return jittered(self.interval_ms as f64, self.jitter_ratio);
        }

        let consecutive = {
            let stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.consecutive_failures.max(0) as u32
        };
        let backoff = (self.interval_ms as u64).saturating_mul(1_u64 << consecutive.min(6));
        backoff.min(self.max_backoff_ms)
    }

    /// Coalesced probe: concurrent callers share the same in-flight request.
    pub async fn probe_once(&self) -> ProbeResult {
        if !self.enabled() {
            return ProbeResult {
                skipped: true,
                error: Some("disabled".into()),
                ..Default::default()
            };
        }
        let Ok(_guard) = self.run_lock.try_lock() else {
            return ProbeResult {
                skipped: true,
                ..Default::default()
            };
        };
        self.probe().await
    }

    async fn probe(&self) -> ProbeResult {
        let started = std::time::Instant::now();
        let started_at = now_ms();
        {
            let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.probes += 1;
            stats.last_attempt_at = Some(started_at);
        }

        let origin = url::Url::parse(&self.url)
            .map(|url| format!("{}://{}", url.scheme(), url.host_str().unwrap_or("")))
            .unwrap_or_default();

        let (token, cookie) = (self.credentials)();
        let mut request = self
            .http
            .post(&self.url)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header(
                "origin",
                if origin.is_empty() {
                    "https://rollercoin.com".to_string()
                } else {
                    origin.clone()
                },
            )
            .header("referer", format!("{origin}/marketplace"))
            .header("user-agent", USER_AGENT)
            .header("accept-language", "en-US,en;q=0.9")
            .body("{}")
            .timeout(Duration::from_millis(self.timeout_ms));
        if !token.is_empty() {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        if !cookie.is_empty() {
            request = request.header("cookie", cookie);
        }

        match request.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                // Drain the body so the connection can be reused.
                let _ = response.bytes().await;
                let completed_at = now_ms();
                let rtt_ms = started.elapsed().as_secs_f64() * 1000.0;
                {
                    let mut latency = self.latency.lock().unwrap_or_else(|e| e.into_inner());
                    latency.record(Channel::Keepalive, rtt_ms, true, None);
                }
                let suspended_until =
                    (status == 429).then_some(completed_at + self.suspend_ms as i64);
                {
                    let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                    stats.last_status = Some(status);
                    stats.last_success_at = Some(completed_at);
                    stats.last_rtt_ms = Some((rtt_ms * 10.0).round() / 10.0);
                    stats.consecutive_failures = 0;
                    stats.last_error = None;
                    stats.suspended_until = suspended_until;
                }
                if status == 429 {
                    self.logger.warn(
                        "keepalive",
                        format!("HTTP 429; pausing probes for {}s", self.suspend_ms / 1000),
                    );
                } else {
                    self.logger
                        .debug("keepalive", format!("warm HTTP {status} in {rtt_ms:.1}ms"));
                }
                ProbeResult {
                    ok: true,
                    status,
                    rtt_ms: Some((rtt_ms * 10.0).round() / 10.0),
                    suspended_until,
                    ..Default::default()
                }
            }
            Err(error) => {
                let message = if error.is_timeout() {
                    format!("timed out after {}ms", self.timeout_ms)
                } else {
                    crate::redact::redact_secrets(&error.to_string())
                };
                {
                    let mut latency = self.latency.lock().unwrap_or_else(|e| e.into_inner());
                    latency.record(
                        Channel::Keepalive,
                        started.elapsed().as_secs_f64() * 1000.0,
                        false,
                        Some(&message),
                    );
                }
                let consecutive = {
                    let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
                    stats.failures += 1;
                    stats.consecutive_failures += 1;
                    stats.last_error = Some(message.clone());
                    stats.consecutive_failures
                };
                if consecutive == 1 || consecutive % 10 == 0 {
                    self.logger.warn(
                        "keepalive",
                        format!("probe failed: {message} ({consecutive}x)"),
                    );
                }
                ProbeResult {
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
    use axum::routing::post;
    use axum::{Json, Router};

    async fn spawn_server(status: u16) -> String {
        let app = Router::new().route(
            "/probe",
            post(move || async move {
                if status == 429 {
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        Json(json!({ "error": "slow down" })),
                    )
                } else {
                    (axum::http::StatusCode::OK, Json(json!({ "ok": true })))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}/probe")
    }

    fn service(url: String, tracker: Arc<Mutex<LatencyTracker>>) -> Arc<KeepaliveService> {
        KeepaliveService::new(KeepaliveConfig {
            url,
            interval_ms: 30_000,
            timeout_ms: 2_000,
            suspend_ms: 60_000,
            jitter_ratio: 0.0,
            credentials: Arc::new(|| ("token".into(), String::new())),
            should_probe: Arc::new(|| true),
            logger: Arc::new(Logger::new("error")),
            latency: tracker,
            last_request_at: Arc::new(AtomicI64::new(0)),
            http: reqwest::Client::new(),
        })
    }

    fn tracker() -> Arc<Mutex<LatencyTracker>> {
        Arc::new(Mutex::new(LatencyTracker::new(120, Arc::new(now_ms))))
    }

    #[tokio::test]
    async fn a_200_probe_records_a_latency_sample() {
        let tracker = tracker();
        let service = service(spawn_server(200).await, tracker.clone());
        let result = service.probe_once().await;
        assert!(result.ok);
        assert_eq!(result.status, 200);
        assert!(result.rtt_ms.is_some());

        let snapshot = tracker.lock().unwrap().snapshot();
        assert_eq!(snapshot["channels"]["keepalive"]["count"], 1);
        assert_eq!(snapshot["channels"]["keepalive"]["lastOk"], true);

        let stats = service.stats_json();
        assert_eq!(stats["probes"], 1);
        assert_eq!(stats["lastStatus"], 200);
        assert_eq!(stats["failures"], 0);
    }

    #[tokio::test]
    async fn a_429_suspends_further_probes() {
        let service = service(spawn_server(429).await, tracker());
        let result = service.probe_once().await;
        assert!(result.ok, "any HTTP status counts as a reached upstream");
        assert_eq!(result.status, 429);
        assert!(result.suspended_until.is_some());
        let stats = service.stats_json();
        assert!(stats["suspendedUntil"].as_i64().is_some());

        // The next tick must wait for the suspension window, not probe again.
        let delay = service.tick().await;
        assert!(delay > 30_000, "expected suspension delay, got {delay}");
    }

    #[tokio::test]
    async fn transport_failure_backs_off() {
        let service = service("http://127.0.0.1:1/probe".into(), tracker());
        let result = service.probe_once().await;
        assert!(!result.ok);
        assert!(result.error.is_some());
        let stats = service.stats_json();
        assert_eq!(stats["failures"], 1);
        assert_eq!(stats["consecutiveFailures"], 1);

        let delay = service.tick().await;
        assert!(delay >= 60_000, "backoff should double, got {delay}");
    }

    #[tokio::test]
    async fn skips_when_real_traffic_just_happened() {
        let last = Arc::new(AtomicI64::new(now_ms()));
        let mut config_service = KeepaliveService::new(KeepaliveConfig {
            url: "http://127.0.0.1:1/probe".into(),
            interval_ms: 30_000,
            timeout_ms: 1_000,
            suspend_ms: 60_000,
            jitter_ratio: 0.0,
            credentials: Arc::new(|| (String::new(), String::new())),
            should_probe: Arc::new(|| true),
            logger: Arc::new(Logger::new("error")),
            latency: tracker(),
            last_request_at: last.clone(),
            http: reqwest::Client::new(),
        });
        let delay = config_service.tick().await;
        assert!(delay > 0);
        let stats = config_service.stats_json();
        assert!(stats["lastSkipAt"].as_i64().is_some());
        assert_eq!(stats["probes"], 0);
        let _ = &mut config_service;
    }

    #[tokio::test]
    async fn disabled_service_reports_skipped() {
        let service = KeepaliveService::new(KeepaliveConfig {
            url: String::new(),
            interval_ms: 0,
            timeout_ms: 1_000,
            suspend_ms: 60_000,
            jitter_ratio: 0.0,
            credentials: Arc::new(|| (String::new(), String::new())),
            should_probe: Arc::new(|| true),
            logger: Arc::new(Logger::new("error")),
            latency: tracker(),
            last_request_at: Arc::new(AtomicI64::new(0)),
            http: reqwest::Client::new(),
        });
        assert!(!service.enabled());
        let result = service.probe_once().await;
        assert!(result.skipped);
        assert!(service.start().is_none());
    }
}
