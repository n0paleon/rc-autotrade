//! Replays a parity dataset through the Rust engine and records decisions.
//!
//! Mirrors `scripts/parity-run-js.mjs`: the same frames, the same dry-run
//! settings, and the same broadcast-derived decision shape.
//!
//! Run: cargo run --release --example replay -- .qa/parity-dataset.json .qa/parity-rust.json

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{Value, json};

use rollercoin_worker::engine::{Engine, EngineEvent};
use rollercoin_worker::logger::Logger;
use rollercoin_worker::model::{Settings, Target};
use rollercoin_worker::store::Store;

fn main() {
    let input = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".qa/parity-dataset.json".to_string());
    let output = std::env::args()
        .nth(2)
        .unwrap_or_else(|| ".qa/parity-rust.json".to_string());

    let text = std::fs::read_to_string(&input).expect("read dataset");
    let dataset: Value = serde_json::from_str(&text).expect("parse dataset");
    let frames = dataset["frames"].as_array().expect("frames array").clone();

    let dir = std::env::temp_dir().join(format!("rc-parity-{}", uuid::Uuid::new_v4()));
    let logger = Arc::new(Logger::new("error"));
    let mut store = Store::open(dir.clone(), logger.clone()).expect("store");

    let targets: Vec<Target> = serde_json::from_value(dataset["targets"].clone()).expect("targets");
    store.set_configs(targets).expect("seed targets");
    let settings: Settings = serde_json::from_value(dataset["settings"].clone()).expect("settings");
    store.state.settings = settings;
    let _ = store.flush();

    let now = Arc::new(AtomicI64::new(
        frames
            .first()
            .and_then(|frame| frame["at"].as_i64())
            .unwrap_or(1),
    ));
    let clock_now = now.clone();
    let mut engine = Engine::new(Arc::new(move || clock_now.load(Ordering::SeqCst)), logger);

    let mut decisions: Vec<Value> = Vec::with_capacity(frames.len());
    for frame in &frames {
        let at = frame["at"].as_i64().unwrap_or(0);
        now.store(at, Ordering::SeqCst);
        let raw = &frame["raw"];

        let mut entry = json!({
            "at": at,
            "itemId": Value::Null,
            "hot": Value::Null,
            "state": Value::Null,
            "trend": Value::Null,
            "proximityPct": Value::Null,
            "bandPct": Value::Null,
            "dry": Value::Null,
        });

        let events = engine.handle_frame(&mut store, raw, at, true);
        for event in events {
            if let EngineEvent::Broadcast { kind, data } = event {
                if kind == "market" {
                    entry["itemId"] = data["itemId"].clone();
                    entry["hot"] = data["hot"].clone();
                    entry["state"] = data["trend"]["state"].clone();
                    entry["trend"] = data["trend"]["trend"].clone();
                    entry["proximityPct"] = data["trend"]["proximityPct"].clone();
                    entry["bandPct"] = data["trend"]["bandPct"].clone();
                } else if kind == "order" && data["phase"] == "dry" {
                    entry["dry"] = json!({ "qty": data["qty"], "notional": data["notional"] });
                }
            }
        }
        decisions.push(entry);
    }

    std::fs::write(
        &output,
        serde_json::to_string_pretty(&Value::Array(decisions.clone())).expect("serialize"),
    )
    .expect("write output");

    let hot = decisions
        .iter()
        .filter(|entry| entry["hot"].as_bool() == Some(true))
        .count();
    let dry = decisions
        .iter()
        .filter(|entry| !entry["dry"].is_null())
        .count();
    println!(
        "rust: {} frames, {} hot, {} dry orders -> {}",
        decisions.len(),
        hot,
        dry,
        output
    );

    store.close();
    let _ = std::fs::remove_dir_all(&dir);
}
