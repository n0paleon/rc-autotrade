use std::time::Duration;

use super::*;

const PROC_STAT: &str = "cpu  100 0 50 1000 20 0 5 0 0 0\ncpu0 50 0 25 500 10 0 2 0 0 0\n";

#[test]
fn parses_host_cpu_times_with_idle_and_iowait() {
    let (total, idle) = parse_proc_cpu_times(PROC_STAT).unwrap();
    assert_eq!(total, 100 + 50 + 1000 + 20 + 5);
    assert_eq!(idle, 1000 + 20);
}

#[test]
fn parses_process_ticks_from_a_comm_with_spaces() {
    let line = "123 (my worker) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16";
    let (utime, stime) = parse_proc_stat_ticks(line).unwrap();
    assert_eq!(utime, 11);
    assert_eq!(stime, 12);
}

#[test]
fn parses_meminfo_kilobytes() {
    let text = "MemTotal: 16384000 kB\nMemFree: 1000 kB\nMemAvailable: 8192000 kB\nSwapTotal: 2048000 kB\nSwapFree: 1024000 kB\n";
    let info = parse_meminfo(text);
    assert_eq!(info.total_kb, 16_384_000);
    assert_eq!(info.available_kb, 8_192_000);
    assert_eq!(info.free_kb, 1_000);
    assert_eq!(info.swap_total_kb, 2_048_000);
    assert_eq!(info.swap_free_kb, 1_024_000);
}

#[test]
fn parses_loadavg() {
    assert_eq!(parse_loadavg("0.50 0.75 1.00 2/345 6789"), (0.5, 0.75, 1.0));
}

#[test]
fn health_escalates_on_memory_loop_and_db() {
    let (level, reasons) = classify_health(None, Some(90.0), Some(120.0), Some(99.0), None, 0, 0);
    assert_eq!(level, "warn");
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("system memory"))
    );
    assert!(reasons.iter().any(|reason| reason.contains("event loop")));
    assert!(reasons.iter().any(|reason| reason.contains("worker cpu")));

    let (level, reasons) = classify_health(None, Some(97.0), None, None, Some(120.0), 0, 0);
    assert_eq!(level, "critical");
    assert!(reasons.iter().any(|reason| reason.contains("db p95")));
}

#[test]
fn health_warns_on_wal_growth_and_db_errors() {
    let (level, reasons) = classify_health(
        None,
        None,
        None,
        None,
        None,
        (70.0 * 1024.0 * 1024.0) as i64,
        3,
    );
    assert_eq!(level, "warn");
    assert!(reasons.iter().any(|reason| reason.contains("WAL")));
    assert!(reasons.iter().any(|reason| reason.contains("db errors")));
}

#[test]
fn samples_this_process_without_panicking() {
    let mut monitor = SystemMonitor::new(2_000, crate::db_metrics::DbMetrics::new("."));
    let first = monitor.sample();
    assert_eq!(first["process"]["pid"], std::process::id());
    assert_eq!(first["system"]["source"], "procfs");
    assert!(first["memory"]["rssMb"].as_f64().unwrap() > 0.0);
    assert_eq!(first["health"]["level"], "ok");

    // Second sample produces CPU deltas.
    std::thread::sleep(Duration::from_millis(30));
    let second = monitor.sample();
    assert!(second["history"].as_array().unwrap().len() >= 2);
}

#[test]
fn lag_p95_needs_enough_samples_and_then_clears() {
    let mut monitor = SystemMonitor::new(2_000, crate::db_metrics::DbMetrics::new("."));
    assert!(monitor.lag_view().is_none());
    for _ in 0..20 {
        monitor.record_lag(5.0);
    }
    let view = monitor.lag_view().unwrap();
    assert_eq!(view["p95"], 5.0);
    assert!(monitor.lag_view().is_none(), "samples reset after a read");
}
