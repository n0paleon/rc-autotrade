//! Warm HTTP probe for the purchase endpoint.
//!
//! Keeps the Rollercoin edge connection hot while the sniper is armed. The probe
//! POSTs an empty body to the purchase endpoint: any HTTP response proves the
//! request reached upstream, and an empty body can only be rejected by
//! validation — it can never place an order.
//!
//! It is skipped when real traffic just happened, backs off on transport
//! failures, and pauses for 5 minutes after a 429.

mod probe;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::clock::now_ms;
use crate::gateway::CredentialsFn;
use crate::latency::LatencyTracker;
use crate::lock::lock;
use crate::logger::Logger;

pub use crate::constants::USER_AGENT;

pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_SUSPEND_MS: u64 = 5 * 60_000;
pub const MAX_BACKOFF_MS: u64 = 5 * 60_000;

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
        lock(&self.stats).enabled
    }

    pub fn stats_json(&self) -> Value {
        let stats = lock(&self.stats);
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
        let mut stats = lock(&self.stats);
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
            let stats = lock(&self.stats);
            stats.suspended_until
        };
        if let Some(until) = suspended_until {
            if now < until {
                return (until - now).max(0) as u64;
            }
            let mut stats = lock(&self.stats);
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
            let mut stats = lock(&self.stats);
            stats.last_skip_at = Some(now);
            return jittered(self.interval_ms as f64, self.jitter_ratio);
        }

        let last_real = self.last_request_at.load(Ordering::Relaxed);
        if last_real > 0 {
            let since_real = now - last_real;
            if since_real < (self.interval_ms as i64 * 9) / 10 {
                let mut stats = lock(&self.stats);
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
            let stats = lock(&self.stats);
            stats.consecutive_failures.max(0) as u32
        };
        let backoff = self.interval_ms.saturating_mul(1_u64 << consecutive.min(6));
        backoff.min(self.max_backoff_ms)
    }
}
