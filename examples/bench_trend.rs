//! Micro-benchmark for the trend model.
//!
//! Compares against the JavaScript baseline recorded in
//! `docs/rust-rewrite-feasibility.md` (1.2 ms p50 for a 1 440-point series).
//!
//! Run: cargo run --release --example bench_trend

use std::time::Instant;

use rollercoin_worker::trend::{Point, TrendOptions, build_trend};

const MINUTE: i64 = 60_000;

fn main() {
    let now = 1_777_982_400_000_i64;
    let mut history: Vec<Point> = Vec::with_capacity(1441);
    for index in 0..=1440 {
        let t = now - (1440 - index) * MINUTE;
        history.push(Point {
            t,
            p: 2.0 + ((index as f64) / 40.0).sin() * 0.06,
        });
    }

    let options = TrendOptions {
        now,
        target_price: Some(2.0),
        ..Default::default()
    };

    // Warm up.
    for _ in 0..200 {
        std::hint::black_box(build_trend(&history, &options));
    }

    let iterations = 20_000;
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        std::hint::black_box(build_trend(&history, &options));
        samples.push(started.elapsed().as_nanos() as f64 / 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)];
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;

    println!("build_trend, {} points", history.len());
    println!("  mean {:>8.2} µs", mean);
    println!("  p50  {:>8.2} µs", at(0.50));
    println!("  p95  {:>8.2} µs", at(0.95));
    println!("  p99  {:>8.2} µs", at(0.99));
    println!("  max  {:>8.2} µs", samples[samples.len() - 1]);
    println!();
    println!("JavaScript baseline (Bun): p50 1 248 µs / p95 2 158 µs");
}
