//! Micro-benchmark for the Rust frame path, comparable to the Bun numbers in
//! `docs/rust-rewrite-feasibility.md`.
//!
//! Two regimes, because they answer different questions:
//! * **burst** — the clock advances 100 ms per frame, so the 2 s snapshot
//!   throttle and the 10 s trend cache mostly skip: this is the "price tick
//!   arrives, decide, send" cost;
//! * **steady** — 1 s per frame, so one in two frames writes a snapshot and one
//!   in ten rebuilds the trend: this is the sustained cost of a busy feed.
//!
//! Run: cargo run --release --example bench_frame

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use serde_json::json;

use rollercoin_worker::engine::Engine;
use rollercoin_worker::logger::Logger;
use rollercoin_worker::model::{Mode, Settings, Target};
use rollercoin_worker::store::Store;

struct Stats {
    mean: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn measure(
    engine: &mut Engine,
    store: &mut Store,
    now: &Arc<AtomicI64>,
    step_ms: i64,
    frames: usize,
) -> Stats {
    let mut base = now.load(Ordering::SeqCst) + 60_000;
    let mut samples = Vec::with_capacity(frames);
    for _ in 0..frames {
        base += step_ms;
        now.store(base, Ordering::SeqCst);
        let frame = json!({
            "cmd": "marketplace_orders_update",
            "value": {
                "item_id": "6319f7baa8ce530569ed16b9",
                "currency": "RLT",
                "item_type": "mutation_component",
                "data": { "list": [{ "price": 90_000, "quantity": 10 }] }
            }
        });
        let started = Instant::now();
        engine.handle_frame(store, &frame, base, true);
        samples.push(started.elapsed().as_nanos() as f64 / 1000.0);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)];
    Stats {
        mean: samples.iter().sum::<f64>() / samples.len() as f64,
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        max: samples[samples.len() - 1],
    }
}

fn main() {
    let dir = std::env::temp_dir().join(format!("rc-bench-frame-{}", uuid::Uuid::new_v4()));
    let logger = Arc::new(Logger::new("error"));
    let mut store = Store::open(dir.clone(), logger.clone()).expect("store");

    let item_id = "6319f7baa8ce530569ed16b9";
    store
        .set_configs(vec![Target {
            item_id: item_id.into(),
            label: "Bench".into(),
            item_type: "mutation_component".into(),
            rarity_color: None,
            target_price: 0.1,
            desired_qty: 10,
            high_stock_threshold: 20,
            min_acceptable_qty: 1,
            allow_partial_order: true,
            active: true,
            created_at: None,
            updated_at: None,
        }])
        .expect("targets");
    store.state.settings = Settings {
        delay_per_order: 0,
        max_orders_per_minute: 600,
        mode: Mode::Shadow,
        auto_refresh: false,
    };

    let now = Arc::new(AtomicI64::new(1_777_982_400_000));
    let clock_now = now.clone();
    let mut engine = Engine::new(Arc::new(move || clock_now.load(Ordering::SeqCst)), logger);

    // Build a full 24 h history one minute at a time.
    let start = now.load(Ordering::SeqCst);
    for minute in 0..1_500 {
        let at = start + minute * 60_000;
        now.store(at, Ordering::SeqCst);
        engine.handle_frame(
            &mut store,
            &json!({
                "cmd": "marketplace_orders_update",
                "value": {
                    "item_id": item_id,
                    "currency": "RLT",
                    "item_type": "mutation_component",
                    "data": { "list": [{ "price": 90_000, "quantity": 10 }] }
                }
            }),
            at,
            true,
        );
    }

    for (label, step_ms, frames) in [
        ("burst (100 ms/frame, cache warm)", 100_i64, 5_000_usize),
        (
            "steady (1 s/frame, writes + rebuilds)",
            1_000_i64,
            5_000_usize,
        ),
    ] {
        let stats = measure(&mut engine, &mut store, &now, step_ms, frames);
        println!("{label}");
        println!("  mean {:>8.2} µs", stats.mean);
        println!("  p50  {:>8.2} µs", stats.p50);
        println!("  p95  {:>8.2} µs", stats.p95);
        println!("  p99  {:>8.2} µs", stats.p99);
        println!("  max  {:>8.2} µs", stats.max);
        println!();
    }
    println!("Bun baseline (hot path, trend cached): p50 12.2 µs / p99 27.4 µs");
    println!("Bun trend rebuild:                     p50 1 248 µs / p95 2 158 µs");
    println!("Rust trend rebuild:                    p50 161 µs / p95 248 µs");

    store.close();
    let _ = std::fs::remove_dir_all(&dir);
}
