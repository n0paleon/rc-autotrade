use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use rollercoin_worker::logger::Logger;
use rollercoin_worker::store::{Store, now_ms};

use crate::{ITEM_ID, TOKEN, spawn_upstream, spawn_worker, temp_dir};

#[tokio::test]
async fn benchmark_requires_the_key_and_aggregates_seeded_timings() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;

    // Seed one timed fill before the worker boots; the endpoint rebuilds the
    // benchmark from the loot log rather than an in-memory ring.
    {
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger).expect("store");
        let mut entry = rollercoin_worker::store::make_entry(
            "bench-filled",
            ITEM_ID,
            "Hashboard",
            "filled",
            1,
            now_ms(),
        );
        entry.timings = rollercoin_worker::timings::sanitize(&json!({
            "signalAt": 1_000,
            "handleAt": 1_004,
            "orderAt": 1_010,
            "dispatchAt": 1_012,
            "sendAt": 1_013,
            "responseAt": 1_093,
            "completeAt": 1_095,
            "acceptedAt": 1_096,
            "fillAt": 1_200,
            "fillHandledAt": 1_210,
            "ttfbMs": 70.0
        }));
        store.add_history(entry).expect("seed row");
    }

    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let unauthorized = client
        .get(format!("{base}/api/benchmark"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let body: Value = client
        .get(format!("{base}/api/benchmark"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], true);
    let benchmark = &body["benchmark"];
    assert_eq!(benchmark["available"], 1);
    assert_eq!(benchmark["totals"]["filled"], 1);
    assert_eq!(benchmark["windows"]["25"]["stages"]["total"]["count"], 1);
    assert_eq!(benchmark["windows"]["25"]["stages"]["total"]["avg"], 210.0);
    // The monotonic TTFB wins over the wall-clock send→response delta.
    assert_eq!(benchmark["windows"]["25"]["stages"]["ttfb"]["avg"], 70.0);
    // Fill wait starts at that same TTFB boundary (1013 + 70), not at the
    // wall-clock response mark (1093).
    assert_eq!(
        benchmark["windows"]["25"]["stages"]["fillWait"]["avg"],
        117.0
    );
    assert_eq!(
        benchmark["recent"]["count"],
        rollercoin_worker::benchmark::RECENT_ORDERS
    );
    assert_eq!(benchmark["recent"]["orders"], 1);
    assert_eq!(benchmark["recent"]["avg"], 210.0);

    let history: Value = client
        .get(format!(
            "{base}/api/benchmark/history?hours=1&bucket=5m&tzOffsetMs=0&stage=total"
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(history["ok"], true);
    assert_eq!(history["history"]["stage"], "total");
    assert_eq!(history["history"]["summary"]["orders"], 1);
    assert_eq!(history["history"]["summary"]["avg"], 210.0);
    assert_eq!(history["history"]["points"][0]["n"], 1);
    assert_eq!(history["history"]["bucketMs"], 300_000);

    let state: Value = client
        .get(format!("{base}/api/state"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["benchmark"]["available"], 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn benchmark_averages_the_newest_orders_regardless_of_age() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let two_hours_ago = now_ms() - 2 * 3_600_000;
    {
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger).expect("store");
        let mut entry = rollercoin_worker::store::make_entry(
            "bench-old",
            ITEM_ID,
            "Hashboard",
            "filled",
            1,
            two_hours_ago,
        );
        entry.timings = rollercoin_worker::timings::sanitize(&json!({
            "signalAt": 1_000,
            "fillHandledAt": 1_210
        }));
        store.add_history(entry).expect("seed row");
    }

    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();
    let body: Value = client
        .get(format!("{base}/api/benchmark"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let recent = &body["benchmark"]["recent"];
    // Count-based window: order age is irrelevant now, so the 2-hour-old order
    // is still the "recent" one with no fallback flag needed.
    assert!(recent.get("fallback").is_none());
    assert_eq!(recent["until"], two_hours_ago);
    assert_eq!(recent["orders"], 1);
    assert_eq!(recent["avg"], 210.0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn deleting_history_wipes_the_loot_log() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;

    {
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger).expect("store");
        store
            .add_history(rollercoin_worker::store::make_entry(
                "order-reset",
                ITEM_ID,
                "Hashboard",
                "filled",
                1,
                now_ms(),
            ))
            .expect("seed row");
    }

    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let before: Value = client
        .get(format!("{base}/api/history?limit=10"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["total"], 1);

    let reset = client
        .delete(format!("{base}/api/history"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(reset.status(), 200);
    let reset_body: Value = reset.json().await.unwrap();
    assert_eq!(reset_body["ok"], true);
    assert_eq!(reset_body["stats"]["filled"], 0);

    let after: Value = client
        .get(format!("{base}/api/history?limit=10"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["total"], 0, "the loot log must stay wiped");
    assert!(after["entries"].as_array().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}
