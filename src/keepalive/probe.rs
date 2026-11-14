//! The warm probe request itself.

use std::time::Duration;

use crate::clock::now_ms;
use crate::latency::Channel;
use crate::lock::lock;

use super::{KeepaliveService, ProbeResult, USER_AGENT};

impl KeepaliveService {
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
            let mut stats = lock(&self.stats);
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
                    let mut latency = lock(&self.latency);
                    latency.record(Channel::Keepalive, rtt_ms, true, None);
                }
                let suspended_until =
                    (status == 429).then_some(completed_at + self.suspend_ms as i64);
                {
                    let mut stats = lock(&self.stats);
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
                    let mut latency = lock(&self.latency);
                    latency.record(
                        Channel::Keepalive,
                        started.elapsed().as_secs_f64() * 1000.0,
                        false,
                        Some(&message),
                    );
                }
                let consecutive = {
                    let mut stats = lock(&self.stats);
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
