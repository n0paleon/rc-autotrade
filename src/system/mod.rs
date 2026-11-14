//! Worker/host health sampler.
//!
//! Reports process CPU from `/proc/self/stat`, host CPU from `/proc/stat`, RSS
//! from `/proc/self/statm`, system RAM from `/proc/meminfo`, and a scheduler-lag
//! p95 measured by a 100 ms watchdog task.
//!
//! The snapshot keys match the deck (`process`, `cpu`, `memory`, `system`,
//! `eventLoop`, `db`, `health`, `history`); fields that have no Rust equivalent
//! are `null` rather than invented.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::clock::now_ms;
use crate::lock::lock;
use crate::logger::Logger;
use crate::num::round_finite;

mod procfs;

#[cfg(test)]
mod tests;

use procfs::{percent_of, process_rss_bytes, read_file, system_uptime_ms};

pub use procfs::{parse_loadavg, parse_meminfo, parse_proc_cpu_times, parse_proc_stat_ticks};

pub const DEFAULT_INTERVAL_MS: u64 = 2_000;
pub const MIN_INTERVAL_MS: u64 = 500;
pub const MAX_INTERVAL_MS: u64 = 60_000;
pub const DEFAULT_HISTORY: usize = 120;
pub const MAX_HISTORY: usize = 120;
const LAG_SAMPLES: usize = 480;
const MIN_LAG_SAMPLES: usize = 12;

pub const HEAP_WARN: f64 = 80.0;
pub const HEAP_CRITICAL: f64 = 92.0;
pub const SYSTEM_WARN: f64 = 88.0;
pub const SYSTEM_CRITICAL: f64 = 96.0;
pub const LOOP_WARN_MS: f64 = 100.0;
pub const LOOP_CRITICAL_MS: f64 = 250.0;
pub const CPU_WARN: f64 = 95.0;
pub const DB_P95_WARN_MS: f64 = 25.0;
pub const DB_P95_CRITICAL_MS: f64 = 100.0;
pub const WAL_WARN_BYTES: f64 = 64.0 * 1024.0 * 1024.0;
pub const WAL_CRITICAL_BYTES: f64 = 256.0 * 1024.0 * 1024.0;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MemInfo {
    pub total_kb: u64,
    pub available_kb: u64,
    pub free_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
}

// ── health ──────────────────────────────────────────────────────────────────

pub fn classify_health(
    heap_percent: Option<f64>,
    system_percent: Option<f64>,
    loop_p95: Option<f64>,
    cpu_percent: Option<f64>,
    db_p95: Option<f64>,
    wal_bytes: i64,
    db_errors: i64,
) -> (String, Vec<String>) {
    let mut level = "ok";
    let mut reasons = Vec::new();
    let mut escalate = |next: &str, reason: String| {
        if next == "critical" {
            level = "critical";
        } else if next == "warn" && level != "critical" {
            level = "warn";
        }
        reasons.push(reason);
    };

    if let Some(heap) = heap_percent.filter(|value| value.is_finite()) {
        if heap >= HEAP_CRITICAL {
            escalate("critical", format!("heap {heap:.1}% of limit"));
        } else if heap >= HEAP_WARN {
            escalate("warn", format!("heap {heap:.1}% of limit"));
        }
    }

    if let Some(system) = system_percent.filter(|value| value.is_finite()) {
        if system >= SYSTEM_CRITICAL {
            escalate("critical", format!("system memory {system:.1}% used"));
        } else if system >= SYSTEM_WARN {
            escalate("warn", format!("system memory {system:.1}% used"));
        }
    }

    if let Some(lag) = loop_p95.filter(|value| value.is_finite()) {
        if lag >= LOOP_CRITICAL_MS {
            escalate("critical", format!("event loop p95 {lag:.0}ms"));
        } else if lag >= LOOP_WARN_MS {
            escalate("warn", format!("event loop p95 {lag:.0}ms"));
        }
    }

    if let Some(cpu) = cpu_percent.filter(|value| value.is_finite())
        && cpu >= CPU_WARN
    {
        escalate("warn", format!("worker cpu {cpu:.0}%"));
    }

    if let Some(p95) = db_p95.filter(|value| value.is_finite() && *value > 0.0) {
        if p95 >= DB_P95_CRITICAL_MS {
            escalate("critical", format!("db p95 {p95:.0}ms"));
        } else if p95 >= DB_P95_WARN_MS {
            escalate("warn", format!("db p95 {p95:.0}ms"));
        }
    }

    if wal_bytes > 0 {
        let bytes = wal_bytes as f64;
        if bytes >= WAL_CRITICAL_BYTES {
            escalate(
                "critical",
                format!("WAL {:.0}MB", bytes / (1024.0 * 1024.0)),
            );
        } else if bytes >= WAL_WARN_BYTES {
            escalate("warn", format!("WAL {:.0}MB", bytes / (1024.0 * 1024.0)));
        }
    }

    if db_errors > 0 {
        escalate("warn", format!("db errors {db_errors}"));
    }

    (level.to_string(), reasons)
}

// ── monitor ─────────────────────────────────────────────────────────────────

struct Previous {
    at: i64,
    proc_ticks: u64,
    host_total: u64,
    host_idle: u64,
}

pub struct SystemMonitor {
    interval_ms: u64,
    history_limit: usize,
    metrics: Arc<crate::db_metrics::DbMetrics>,
    started_at: i64,
    previous: Option<Previous>,
    lag_samples: VecDeque<f64>,
    history: VecDeque<Value>,
    latest: Option<Value>,
}

impl SystemMonitor {
    pub fn new(interval_ms: u64, metrics: Arc<crate::db_metrics::DbMetrics>) -> Self {
        Self {
            interval_ms: interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS),
            history_limit: DEFAULT_HISTORY,
            metrics,
            started_at: now_ms(),
            previous: None,
            lag_samples: VecDeque::with_capacity(LAG_SAMPLES),
            history: VecDeque::with_capacity(MAX_HISTORY),
            latest: None,
        }
    }

    /// Scheduler lag recorded by the 100 ms watchdog task.
    pub fn record_lag(&mut self, lag_ms: f64) {
        if lag_ms.is_finite() && lag_ms >= 0.0 {
            self.lag_samples.push_back(lag_ms);
            while self.lag_samples.len() > LAG_SAMPLES {
                self.lag_samples.pop_front();
            }
        }
    }

    fn lag_view(&mut self) -> Option<Value> {
        if self.lag_samples.len() < MIN_LAG_SAMPLES {
            return None;
        }
        let mut samples: Vec<f64> = self.lag_samples.iter().copied().collect();
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let count = samples.len();
        let at = |q: f64| samples[((q * count as f64) as usize).min(count - 1)];
        let mean = samples.iter().sum::<f64>() / count as f64;
        // The lag histogram resets each sample, so p95 describes the last
        // interval instead of all history.
        self.lag_samples.clear();
        Some(json!({
            "count": count,
            "mean": round_finite(mean, 1),
            "p50": round_finite(at(0.5), 1),
            "p95": round_finite(at(0.95), 1),
            "p99": round_finite(at(0.99), 1),
            "max": round_finite(samples[count - 1], 1),
        }))
    }

    fn db_view(&self) -> Value {
        self.metrics.sample()
    }

    /// Takes one sample and updates history.
    pub fn sample(&mut self) -> Value {
        let at = now_ms();

        let proc_ticks = read_file("/proc/self/stat")
            .and_then(|text| parse_proc_stat_ticks(&text))
            .map(|(utime, stime)| utime + stime);
        let host = read_file("/proc/stat").and_then(|text| parse_proc_cpu_times(&text));

        let mut process_percent = None;
        let mut host_percent = None;
        if let (Some(previous), Some(current)) = (&self.previous, proc_ticks) {
            let wall_ms = (at - previous.at).max(1) as f64;
            let used_ticks = current.saturating_sub(previous.proc_ticks);
            // Linux USER_HZ is 100 on every mainstream kernel.
            let used_ms = used_ticks as f64 * 10.0;
            process_percent = Some((used_ms / wall_ms * 100.0).max(0.0));
        }
        if let (Some(previous), Some((total, idle))) = (&self.previous, host) {
            let total_delta = total.saturating_sub(previous.host_total);
            let idle_delta = idle.saturating_sub(previous.host_idle);
            if total_delta > 0 {
                host_percent = Some(
                    ((total_delta.saturating_sub(idle_delta)) as f64 / total_delta as f64 * 100.0)
                        .clamp(0.0, 100.0),
                );
            }
        }
        self.previous = Some(Previous {
            at,
            proc_ticks: proc_ticks.unwrap_or(0),
            host_total: host.map(|(total, _)| total).unwrap_or(0),
            host_idle: host.map(|(_, idle)| idle).unwrap_or(0),
        });

        let meminfo = read_file("/proc/meminfo")
            .map(|text| parse_meminfo(&text))
            .unwrap_or_default();
        let load = read_file("/proc/loadavg")
            .map(|text| parse_loadavg(&text))
            .unwrap_or((0.0, 0.0, 0.0));

        let total_bytes = (meminfo.total_kb * 1024) as f64;
        let available_bytes = (meminfo.available_kb * 1024) as f64;
        let used_bytes = (total_bytes - available_bytes).max(0.0);
        let swap_total = (meminfo.swap_total_kb * 1024) as f64;
        let swap_free = (meminfo.swap_free_kb * 1024) as f64;
        let swap_used = (swap_total - swap_free).max(0.0);

        let rss = process_rss_bytes().map(|bytes| bytes as f64);
        let rss_mb = rss.map(|bytes| round_finite(bytes / (1024.0 * 1024.0), 1).unwrap_or(0.0));

        let lag = self.lag_view();
        let lag_p95 = lag.as_ref().and_then(|value| value["p95"].as_f64());

        let cpu = json!({
            "processPercent": process_percent.and_then(|value| round_finite(value, 1)),
            "hostPercent": host_percent.and_then(|value| round_finite(value, 1)),
            "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            "model": read_file("/proc/cpuinfo")
                .and_then(|text| {
                    text.lines()
                        .find(|line| line.starts_with("model name"))
                        .and_then(|line| line.split_once(':'))
                        .map(|(_, value)| value.trim().to_string())
                })
                .unwrap_or_else(|| "unknown".to_string()),
            "load1": round_finite(load.0, 2),
            "load5": round_finite(load.1, 2),
            "load15": round_finite(load.2, 2),
        });

        let memory = json!({
            "rss": rss,
            "rssMb": rss_mb,
            "heapTotal": Value::Null,
            "heapUsed": Value::Null,
            "heapUsedMb": Value::Null,
            "heapLimit": Value::Null,
            "external": Value::Null,
            "arrayBuffers": Value::Null,
            "heapPercent": Value::Null,
            "rssPercent": percent_of(rss, Some(total_bytes)),
        });

        let system = json!({
            "source": "procfs",
            "total": round_finite(total_bytes, 0),
            "available": round_finite(available_bytes, 0),
            "used": round_finite(used_bytes, 0),
            "free": Some((meminfo.free_kb * 1024) as f64),
            "usedPercent": percent_of(Some(used_bytes), Some(total_bytes)),
            "swapTotal": round_finite(swap_total, 0),
            "swapUsed": round_finite(swap_used, 0),
            "swapPercent": percent_of(Some(swap_used), Some(swap_total)),
        });

        let event_loop = json!({
            "utilization": Value::Null,
            "lag": lag,
        });

        let db = self.db_view();
        let (level, reasons) = classify_health(
            None,
            system["usedPercent"].as_f64(),
            lag_p95,
            process_percent,
            db["window"]["p95Ms"].as_f64(),
            db["files"]["wal"].as_i64().unwrap_or(0),
            db["window"]["errors"].as_i64().unwrap_or(0)
                + db["window"]["busy"].as_i64().unwrap_or(0),
        );
        let health = json!({ "level": level, "reasons": reasons });

        let snapshot = json!({
            "sampledAt": at,
            "intervalMs": self.interval_ms,
            "process": {
                "pid": std::process::id(),
                "runtime": format!("rust-worker {}", env!("CARGO_PKG_VERSION")),
                "node": Value::Null,
                "platform": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "uptimeMs": at - self.started_at,
                "systemUptimeMs": system_uptime_ms(),
                "activeHandles": Value::Null,
                "activeRequests": Value::Null,
            },
            "cpu": cpu,
            "memory": memory,
            "system": system,
            "eventLoop": event_loop,
            "db": db,
            "health": health,
            "history": [],
            "historyLimit": self.history_limit,
        });

        self.history.push_back(json!({
            "at": at,
            "cpu": snapshot["cpu"]["processPercent"],
            "rss": rss_mb,
            "heap": Value::Null,
            "loop": lag_p95,
        }));
        while self.history.len() > self.history_limit {
            self.history.pop_front();
        }

        self.latest = Some(snapshot);
        self.snapshot()
    }

    pub fn snapshot(&self) -> Value {
        let mut value = self
            .latest
            .clone()
            .unwrap_or_else(|| json!({ "sampledAt": now_ms(), "intervalMs": self.interval_ms }));
        value["history"] = Value::Array(self.history.iter().cloned().collect());
        value
    }
}

/// Starts the periodic sampler and the 100 ms lag watchdog.
pub fn spawn(
    monitor: Arc<Mutex<SystemMonitor>>,
    interval_ms: u64,
    logger: Arc<Logger>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let sample_monitor = monitor.clone();
    let sample_logger = logger.clone();
    let sampler = tokio::spawn(async move {
        // Baseline sample immediately.
        {
            let mut guard = lock(&sample_monitor);
            guard.sample();
        }
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let mut guard = lock(&sample_monitor);
            guard.sample();
        }
    });

    let lag_monitor = monitor;
    let watchdog = tokio::spawn(async move {
        let period = Duration::from_millis(100);
        let mut ticker = tokio::time::interval(period);
        ticker.tick().await;
        let mut expected = Instant::now() + period;
        loop {
            ticker.tick().await;
            let now = Instant::now();
            let lag = now.saturating_duration_since(expected).as_secs_f64() * 1000.0;
            expected += period;
            if lag > 1_000.0 {
                // A huge gap means the process was suspended; restart the clock.
                expected = now + period;
                continue;
            }
            let mut guard = lock(&lag_monitor);
            guard.record_lag(lag);
        }
    });

    let _ = sample_logger;
    vec![sampler, watchdog]
}
