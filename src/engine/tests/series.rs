use super::*;

use crate::engine::Engine;
use crate::logger::Logger;
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[test]
fn hydrate_restores_the_snapshot_and_runs_the_model() {
    let mut h = harness();
    let frame = market_frame(90_000, 10);
    h.engine.handle_frame(&mut h.store, &frame, 1_000_001, true);
    h.engine.flush_market_cache(&h.store);

    let mut fresh = Engine::new(Arc::new(|| 1_000_010), Arc::new(Logger::new("error")));
    let restored = fresh.hydrate_market(&h.store);
    assert_eq!(restored, 1);
    let snapshot = fresh.runtime_snapshot(&h.store);
    let runtime = &snapshot["6319f7baa8ce530569ed16b9"];
    assert!((runtime["lastPrice"].as_f64().unwrap() - 0.09).abs() < 1e-12);
    assert_eq!(runtime["stale"], true);
}

#[test]
fn samples_quiet_minutes_from_the_last_live_close() {
    let mut h = harness();
    let bucket = 1_000_000_i64.div_euclid(60_000) * 60_000;

    // A target the feed has never seen live must not be sampled: that would
    // bridge the downtime this process just came out of.
    assert!(
        h.engine
            .sample_prices(&mut h.store, bucket + 60_000, true)
            .is_empty()
    );

    h.engine
        .handle_frame(&mut h.store, &market_frame(90_000, 4), 1_000_001, true);

    // The frame already owns its own minute, so there is nothing to add.
    assert!(
        h.engine
            .sample_prices(&mut h.store, bucket + 15_000, true)
            .is_empty()
    );

    // The next minute is quiet: the last live close is carried forward.
    let samples = h.engine.sample_prices(&mut h.store, bucket + 60_000, true);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].bucket, bucket + 60_000);
    assert!((samples[0].price - 0.09).abs() < 1e-12);
    let stored = h.store.price_series("6319f7baa8ce530569ed16b9", 0);
    assert_eq!(stored.len(), 2, "frame close + sampled close");

    // A feed that is not alive writes nothing at all.
    assert!(
        h.engine
            .sample_prices(&mut h.store, bucket + 120_000, false)
            .is_empty()
    );
    assert_eq!(h.store.price_series("6319f7baa8ce530569ed16b9", 0).len(), 2);
}

pub(super) fn stored_point(
    h: &Harness,
    item_id: &str,
    bucket: i64,
) -> crate::model::PriceSeriesPoint {
    h.store
        .price_series(item_id, 0)
        .into_iter()
        .find(|point| point.bucket_at == bucket)
        .expect("stored point for the bucket")
}

pub(super) fn stored_close(h: &Harness, item_id: &str, bucket: i64) -> f64 {
    stored_point(h, item_id, bucket).price
}

#[test]
fn a_transient_bait_price_never_reaches_the_chart() {
    let mut h = harness();
    // 1_000_000 ms falls inside this minute.
    let bucket = 960_000_i64;
    let item = "6319f7baa8ce530569ed16b9";

    // The real level holds, then a one-unit bait ask flashes for 2 s.
    h.now.store(bucket + 1_000, Ordering::SeqCst);
    h.engine.handle_frame(
        &mut h.store,
        &market_frame(100_000, 4),
        bucket + 1_000,
        true,
    );
    h.now.store(bucket + 2_000, Ordering::SeqCst);
    h.engine
        .handle_frame(&mut h.store, &market_frame(10_000, 1), bucket + 2_000, true);
    h.now.store(bucket + 4_000, Ordering::SeqCst);
    h.engine.handle_frame(
        &mut h.store,
        &market_frame(100_000, 4),
        bucket + 4_000,
        true,
    );

    // The live line never took the bait either.
    let snapshot = h.engine.runtime_snapshot(&h.store);
    let history = snapshot[item]["history"].as_array().unwrap();
    let last = history.last().unwrap()["p"].as_f64().unwrap();
    assert!((last - 0.10).abs() < 1e-12, "live line showed {last}");

    // Rolling the minute stores the sustained level as the close.
    h.now.store(bucket + 60_001, Ordering::SeqCst);
    h.engine.handle_frame(
        &mut h.store,
        &market_frame(100_000, 4),
        bucket + 60_001,
        true,
    );
    assert!((stored_close(&h, item, bucket) - 0.10).abs() < 1e-12);
    let point = stored_point(&h, item, bucket);
    // The held band ignores the 2 s bait; the raw band keeps it.
    assert_eq!((point.low, point.high), (Some(0.10), Some(0.10)));
    assert_eq!(
        (point.raw_low, point.raw_high),
        (Some(0.01), Some(0.10)),
        "the raw band must expose the print the dwell guard rejected"
    );
}

#[test]
fn a_spike_that_outlives_the_dwell_but_not_half_the_minute_loses() {
    let mut h = harness();
    let bucket = 960_000_i64;
    let item = "6319f7baa8ce530569ed16b9";
    let frame = |h: &mut Harness, at: i64, price: i64| {
        h.now.store(at, Ordering::SeqCst);
        h.engine
            .handle_frame(&mut h.store, &market_frame(price, 4), at, true);
    };

    frame(&mut h, bucket + 1_000, 100_000);
    frame(&mut h, bucket + 20_000, 100_000);
    frame(&mut h, bucket + 20_001, 10_000);
    // Passes the 3 s dwell, but only rules for 5 s of a 60 s minute.
    frame(&mut h, bucket + 25_001, 10_000);
    frame(&mut h, bucket + 30_001, 100_000);
    frame(&mut h, bucket + 35_001, 100_000);
    frame(&mut h, bucket + 60_001, 100_000);

    assert!(
        (stored_close(&h, item, bucket) - 0.10).abs() < 1e-12,
        "a 10 s wick must not own a 60 s close"
    );
    // The admitted wick still belongs in the band, which is what the
    // expanded chart draws as its low/high column.
    let point = stored_point(&h, item, bucket);
    assert_eq!((point.low, point.high), (Some(0.01), Some(0.10)));
}

#[test]
fn a_print_that_lasts_past_the_dwell_enters_the_band() {
    let mut h = harness();
    let bucket = 960_000_i64;
    let item = "6319f7baa8ce530569ed16b9";
    let frame = |h: &mut Harness, at: i64, price: i64| {
        h.now.store(at, Ordering::SeqCst);
        h.engine
            .handle_frame(&mut h.store, &market_frame(price, 4), at, true);
    };

    frame(&mut h, bucket + 1_000, 100_000);
    frame(&mut h, bucket + 10_000, 100_000);
    // The cheap print appears, then is still there 3.5 s later: it is real
    // enough to join the band even though it is too short to own the close.
    frame(&mut h, bucket + 20_000, 60_000);
    frame(&mut h, bucket + 23_500, 60_000);
    frame(&mut h, bucket + 24_000, 100_000);
    frame(&mut h, bucket + 29_000, 100_000);
    frame(&mut h, bucket + 60_001, 100_000);

    assert!((stored_close(&h, item, bucket) - 0.10).abs() < 1e-12);
    let point = stored_point(&h, item, bucket);
    assert_eq!((point.low, point.high), (Some(0.06), Some(0.10)));
}

#[test]
fn a_sustained_move_wins_the_minute_close() {
    let mut h = harness();
    let bucket = 960_000_i64;
    let item = "6319f7baa8ce530569ed16b9";
    let frame = |h: &mut Harness, at: i64, price: i64| {
        h.now.store(at, Ordering::SeqCst);
        h.engine
            .handle_frame(&mut h.store, &market_frame(price, 4), at, true);
    };

    frame(&mut h, bucket + 1_000, 100_000);
    frame(&mut h, bucket + 10_000, 80_000);
    frame(&mut h, bucket + 15_000, 80_000);
    // The new level holds for the remaining ~45 s.
    frame(&mut h, bucket + 59_000, 80_000);
    frame(&mut h, bucket + 60_001, 80_000);

    assert!(
        (stored_close(&h, item, bucket) - 0.08).abs() < 1e-12,
        "a sustained move must still read as a move"
    );
    let point = stored_point(&h, item, bucket);
    assert_eq!((point.low, point.high), (Some(0.08), Some(0.10)));
}
