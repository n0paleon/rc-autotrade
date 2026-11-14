use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::logger::Logger;
use crate::model::Target;
use crate::store::Store;

mod activity;
mod history;
mod market;
mod state;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rc-rust-store-{tag}-{nanos}"))
}

fn store(tag: &str) -> Store {
    let logger = Arc::new(Logger::new("error"));
    Store::open(temp_dir(tag), logger).expect("store")
}

fn target(item_id: &str) -> Target {
    Target {
        item_id: item_id.to_string(),
        label: "Hashboard".into(),
        item_type: "miner".into(),
        rarity_color: None,
        target_price: 0.1,
        desired_qty: 10,
        high_stock_threshold: 20,
        min_acceptable_qty: 1,
        allow_partial_order: true,
        active: true,
        created_at: None,
        updated_at: None,
    }
}
