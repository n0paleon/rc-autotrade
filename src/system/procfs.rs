//! `/proc` readers and their pure parsers.

use crate::num::round_finite;

use super::MemInfo;

pub(super) fn percent_of(value: Option<f64>, total: Option<f64>) -> Option<f64> {
    match (value, total) {
        (Some(value), Some(total)) if total > 0.0 => round_finite(value / total * 100.0, 1),
        _ => None,
    }
}

// ── /proc parsing (pure, unit-tested) ───────────────────────────────────────

/// `(total, idle)` jiffies from the aggregate `cpu` line of `/proc/stat`.
pub fn parse_proc_cpu_times(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse::<u64>().ok())
        .collect();
    if values.len() < 4 {
        return None;
    }
    let total: u64 = values.iter().sum();
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    Some((total, idle))
}

/// `(utime, stime)` jiffies for this process from `/proc/self/stat`.
pub fn parse_proc_stat_ticks(text: &str) -> Option<(u64, u64)> {
    let tail = text.rsplit_once(')')?.1;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    // After `comm)`, field 3 (state) is index 0, so utime (14) is index 11.
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some((utime, stime))
}

pub fn parse_meminfo(text: &str) -> MemInfo {
    let mut info = MemInfo::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        match key {
            "MemTotal" => info.total_kb = value,
            "MemAvailable" => info.available_kb = value,
            "MemFree" => info.free_kb = value,
            "SwapTotal" => info.swap_total_kb = value,
            "SwapFree" => info.swap_free_kb = value,
            _ => {}
        }
    }
    info
}

pub fn parse_loadavg(text: &str) -> (f64, f64, f64) {
    let mut values = text
        .split_whitespace()
        .filter_map(|value| value.parse::<f64>().ok());
    (
        values.next().unwrap_or(0.0),
        values.next().unwrap_or(0.0),
        values.next().unwrap_or(0.0),
    )
}

pub(super) fn read_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn page_size() -> u64 {
    4096
}

pub(super) fn process_rss_bytes() -> Option<u64> {
    let text = read_file("/proc/self/statm")?;
    let pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * page_size())
}

pub(super) fn system_uptime_ms() -> Option<i64> {
    let text = read_file("/proc/uptime")?;
    let seconds: f64 = text.split_whitespace().next()?.parse().ok()?;
    Some((seconds * 1000.0) as i64)
}
