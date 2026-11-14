use super::math::hampel;
use super::{HOUR, MINUTE, Point, TrendOptions, build_trend, classify_state, compute_trend_model};

const NOW: i64 = 1_777_982_400_000; // 2026-05-05T12:00:00Z

fn series(hours: i64, price_at: impl Fn(i64) -> f64) -> Vec<Point> {
    let start = NOW - hours * HOUR;
    let mut points = Vec::new();
    let mut t = start;
    while t <= NOW {
        points.push(Point { t, p: price_at(t) });
        t += MINUTE;
    }
    points
}

fn options(target: Option<f64>) -> TrendOptions {
    TrendOptions {
        now: NOW,
        target_price: target,
        ..Default::default()
    }
}

#[test]
fn flat_market_yields_a_stable_trend_and_below_target() {
    let history = series(6, |_| 2.0);
    let signal = build_trend(&history, &options(Some(1.8)));

    assert_eq!(signal.model.status, "ok");
    assert!((signal.model.trend.unwrap() - 2.0).abs() < 1e-6);
    assert!((signal.proximity_pct.unwrap() + 10.0).abs() < 0.01);
    assert_eq!(signal.state, "BELOW");
    assert!(signal.reliable);
    assert_eq!(signal.model.direction, "flat");
    assert_eq!(signal.model.outliers, 0);
}

#[test]
fn a_single_cheap_spike_is_filtered_before_the_trend() {
    let history = series(6, |t| {
        if (t - (NOW - 3 * HOUR)).abs() < MINUTE / 2 {
            0.5
        } else {
            2.0
        }
    });
    let model = compute_trend_model(&history, &options(None));

    assert!(model.outliers >= 1, "the spike must be flagged");
    assert!(model.trend.unwrap() > 1.99);
    assert!(model.momentum_pct.unwrap().abs() < 0.2);
}

#[test]
fn sustained_decline_reads_as_downward_and_above_trend() {
    let history = series(6, |t| {
        2.5 - (1.5 * (t - (NOW - 6 * HOUR)) as f64) / (6 * HOUR) as f64
    });
    let signal = build_trend(&history, &options(Some(2.0)));

    assert_eq!(signal.model.status, "ok");
    assert!(signal.model.slope_pct_per_hour.unwrap() < 0.0);
    assert_eq!(signal.model.direction, "down");
    assert_eq!(signal.state, "ABOVE");
    assert!(signal.model.confidence >= 50);
}

#[test]
fn hysteresis_keeps_a_boundary_target_inside_near() {
    let history = series(6, |_| 2.0);

    let fresh = build_trend(&history, &options(Some(2.05)));
    assert_eq!(fresh.state, "ABOVE");

    let held = build_trend(
        &history,
        &TrendOptions {
            previous_state: Some("NEAR".into()),
            ..options(Some(2.05))
        },
    );
    assert_eq!(held.state, "NEAR");

    let lower_held = build_trend(
        &history,
        &TrendOptions {
            previous_state: Some("NEAR".into()),
            ..options(Some(1.95))
        },
    );
    assert_eq!(lower_held.state, "NEAR");

    let lower_fresh = build_trend(&history, &options(Some(1.95)));
    assert_eq!(lower_fresh.state, "BELOW");
}

#[test]
fn classification_thresholds() {
    assert_eq!(classify_state(8.0, None, 2.0), "ABOVE");
    assert_eq!(classify_state(0.0, None, 2.0), "NEAR");
    assert_eq!(classify_state(-10.0, None, 2.0), "BELOW");
    assert_eq!(classify_state(-25.0, None, 2.0), "FAR_BELOW");
}

#[test]
fn too_few_samples_produce_no_data_instead_of_a_guess() {
    let history: Vec<Point> = series(1, |_| 2.0).into_iter().take(4).collect();
    let signal = build_trend(&history, &options(Some(1.9)));

    assert_eq!(signal.model.status, "no_data");
    assert_eq!(signal.state, "NO_DATA");
    assert!(signal.model.trend.is_none());
    assert!(!signal.reliable);
    assert_eq!(signal.action, "wait");
}

#[test]
fn a_frozen_feed_is_stale_and_cannot_be_reliable() {
    let history: Vec<Point> = series(6, |_| 2.0)
        .into_iter()
        .filter(|point| point.t <= NOW - 2 * HOUR)
        .collect();
    let signal = build_trend(&history, &options(Some(1.8)));

    assert_eq!(signal.model.status, "stale");
    assert_eq!(signal.state, "STALE");
    assert!(signal.model.stale);
    assert!(!signal.reliable);
    assert!(signal.model.confidence <= 20);
}

#[test]
fn confidence_grows_with_the_observation_window() {
    let short = build_trend(&series(1, |_| 2.0), &options(Some(1.9)));
    let long = build_trend(&series(8, |_| 2.0), &options(Some(1.9)));
    assert!(long.model.confidence > short.model.confidence);
}

#[test]
fn missing_target_is_reported_as_no_target_but_keeps_the_band() {
    let history = series(6, |_| 2.0);
    let signal = build_trend(&history, &options(None));

    assert_eq!(signal.state, "NO_TARGET");
    assert_eq!(signal.action, "set-target");
    assert!(signal.band_pct.is_some());
}

#[test]
fn hampel_ignores_a_series_shorter_than_five_points() {
    let (values, outliers) = hampel(&[1.0, 2.0, 1.0], 2, 3.0);
    assert_eq!(values, vec![1.0, 2.0, 1.0]);
    assert_eq!(outliers, 0);
}

#[test]
fn a_1440_point_series_stays_finite_and_fast_enough_to_be_usable() {
    let history = series(24, |t| 2.0 + ((t / MINUTE) as f64 / 40.0).sin() * 0.06);
    let signal = build_trend(&history, &options(Some(2.0)));
    assert_eq!(signal.model.status, "ok");
    assert!(signal.model.trend.unwrap().is_finite());
    assert!(signal.model.confidence > 0);
}

fn close(actual: f64, expected: f64, tolerance: f64) -> bool {
    (actual - expected).abs() <= tolerance
}

/// Recorded fixtures on the same three series. This is the model's regression
/// contract: if the model drifts, this test fails.
#[test]
fn matches_the_recorded_reference_series() {
    let flat = build_trend(&series(6, |_| 2.0), &options(Some(1.8)));
    assert!(close(flat.model.trend.unwrap(), 2.0, 1e-9));
    assert!(close(flat.model.fast.unwrap(), 2.0, 1e-9));
    assert_eq!(flat.model.confidence, 85);
    assert!(close(flat.model.error_band_pct.unwrap(), 2.4, 1e-9));
    assert!(close(flat.proximity_pct.unwrap(), -10.0, 1e-9));
    assert!(close(flat.gap_pct.unwrap(), 10.0, 1e-9));
    assert_eq!(flat.state, "BELOW");
    assert_eq!(flat.model.direction, "flat");
    assert_eq!(flat.model.points, 361);
    assert_eq!(flat.model.coverage_pct, 100);
    assert_eq!(flat.model.max_gap_ms, Some(60_000));
    assert!(flat.reliable);
    assert_eq!(flat.action, "hold");

    let decline = build_trend(
        &series(6, |t| {
            2.5 - (1.5 * (t - (NOW - 6 * HOUR)) as f64) / (6 * HOUR) as f64
        }),
        &options(Some(2.0)),
    );
    assert!(close(decline.model.trend.unwrap(), 1.25, 1e-9));
    assert!(close(decline.model.fast.unwrap(), 1.058721, 1e-6));
    assert!(close(decline.model.momentum_pct.unwrap(), -14.734, 1e-3));
    assert!(close(
        decline.model.slope_pct_per_hour.unwrap(),
        -18.464,
        1e-3
    ));
    assert!(close(decline.model.slope_t_stat.unwrap(), -184.82, 1e-2));
    assert!(close(decline.model.noise_pct.unwrap(), 17.949, 1e-3));
    assert_eq!(decline.model.confidence, 85);
    assert!(close(decline.model.error_band_pct.unwrap(), 20.0, 1e-9));
    assert!(close(decline.band_pct.unwrap(), 10.0, 1e-9));
    assert!(close(decline.proximity_pct.unwrap(), 60.0, 1e-9));
    assert_eq!(decline.state, "ABOVE");
    assert_eq!(decline.model.direction, "down");
    assert!(!decline.reliable);
    assert_eq!(decline.action, "wait");

    let sine = build_trend(
        &series(24, |t| 2.0 + ((t / MINUTE) as f64 / 40.0).sin() * 0.06),
        &options(Some(2.0)),
    );
    assert!(close(sine.model.trend.unwrap(), 1.959241, 1e-6));
    assert!(close(sine.model.fast.unwrap(), 2.022128, 1e-6));
    assert!(close(sine.model.momentum_pct.unwrap(), 1.475, 1e-3));
    assert!(close(sine.model.slope_pct_per_hour.unwrap(), 0.191, 1e-3));
    assert!(close(sine.model.slope_t_stat.unwrap(), 1.4, 1e-2));
    assert!(close(sine.model.noise_pct.unwrap(), 2.589, 1e-3));
    assert_eq!(sine.model.confidence, 85);
    assert!(close(sine.model.error_band_pct.unwrap(), 4.92, 1e-9));
    assert!(close(sine.proximity_pct.unwrap(), 2.08, 1e-9));
    assert_eq!(sine.state, "NEAR");
    assert_eq!(sine.model.direction, "flat");
    assert_eq!(sine.model.points, 1441);
    assert_eq!(sine.model.span_ms, 86_400_000);
    assert!(sine.reliable);
}
