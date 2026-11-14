use std::time::Duration;

use super::*;

#[test]
fn classifies_sql_by_first_keyword() {
    assert_eq!(classify("SELECT * FROM orders"), Kind::Read);
    assert_eq!(classify("SeLeCt * FROM orders"), Kind::Read);
    assert_eq!(
        classify("  with x as (select 1) select * from x"),
        Kind::Read
    );
    assert_eq!(classify("INSERT INTO meta (key) VALUES ('a')"), Kind::Write);
    assert_eq!(classify("PRAGMA journal_mode = WAL"), Kind::Pragma);
    assert_eq!(classify("CREATE TABLE x (id INTEGER)"), Kind::Schema);
    assert_eq!(classify("BEGIN"), Kind::Transaction);
    assert_eq!(classify("COMMIT"), Kind::Transaction);
    assert_eq!(classify(""), Kind::Other);
}

#[test]
fn totals_and_window_track_statements() {
    let metrics = DbMetrics::new(std::env::temp_dir());
    metrics.record("SELECT 1", Duration::from_micros(1_500));
    metrics.record("INSERT INTO x VALUES (1)", Duration::from_micros(25_000));
    metrics.record("BEGIN", Duration::from_micros(30));

    let view = metrics.sample();
    assert_eq!(view["queries"]["statements"], 2);
    assert_eq!(view["queries"]["read"], 1);
    assert_eq!(view["queries"]["write"], 1);
    assert_eq!(view["queries"]["slow"], 1);
    assert_eq!(view["transactions"]["count"], 1);
    assert_eq!(view["window"]["count"], 2);
    assert!(view["window"]["p95Ms"].as_f64().unwrap() >= 20.0);
    assert_eq!(view["recentSlow"].as_array().unwrap().len(), 1);
    assert_eq!(view["instrumented"], true);
}

#[test]
fn rate_uses_the_previous_sample() {
    let metrics = DbMetrics::new(std::env::temp_dir());
    metrics.record("SELECT 1", Duration::from_micros(100));
    metrics.sample();
    for _ in 0..5 {
        metrics.record("SELECT 1", Duration::from_micros(100));
    }
    let view = metrics.sample();
    assert!(view["queries"]["rate"].as_f64().unwrap() >= 0.0);
    assert_eq!(view["queries"]["statements"], 6);
}

#[test]
fn reset_clears_counters_and_history() {
    let metrics = DbMetrics::new(std::env::temp_dir());
    metrics.record("SELECT 1", Duration::from_micros(100));
    metrics.sample();
    metrics.reset();
    let view = metrics.sample();
    assert_eq!(view["queries"]["statements"], 0);
    assert!(view["history"].as_array().unwrap().len() <= 1);
}

#[test]
fn errors_and_busy_are_reported_explicitly() {
    let metrics = DbMetrics::new(std::env::temp_dir());
    metrics.mark_error();
    metrics.mark_busy();
    let view = metrics.sample();
    assert_eq!(view["queries"]["errors"], 1);
    assert_eq!(view["queries"]["busy"], 1);
}
