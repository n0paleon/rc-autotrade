use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::{ITEM_ID, TOKEN, spawn_upstream, spawn_worker, temp_dir};

#[tokio::test]
async fn creating_a_target_enriches_its_label_from_item_info() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls.clone()).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/api/configs"))
        .bearer_auth(TOKEN)
        .json(&json!({
            "itemId": ITEM_ID,
            "targetPrice": 0.1,
            "desiredQty": 10,
            "highStockThreshold": 20,
            "minAcceptableQty": 1
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.unwrap();
    let config = &body["configs"][0];
    assert_eq!(config["itemId"], ITEM_ID);
    // The 1.5 s enrichment budget resolved the label before the reply.
    assert_eq!(config["label"], "Rare Hashboard");
    assert_eq!(config["rarityColor"], "a52bff");
    assert!(*upstream_calls.lock().unwrap() >= 1);

    // The enrichment is persisted, not just decorated in the response.
    let state: Value = client
        .get(format!("{base}/api/state"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["configs"][0]["label"], "Rare Hashboard");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn invalid_targets_return_field_errors() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/api/configs"))
        .bearer_auth(TOKEN)
        .json(&json!({ "itemId": "x", "targetPrice": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["fields"]["itemId"].as_str().is_some());
    assert!(body["fields"]["targetPrice"].as_str().is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn control_walks_off_shadow_live_and_never_jumps() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let control = |action: &'static str| {
        let client = client.clone();
        let url = format!("{base}/api/control");
        async move {
            client
                .post(url)
                .bearer_auth(TOKEN)
                .json(&json!({ "action": action }))
                .send()
                .await
                .unwrap()
        }
    };

    // Default is live, so the first step is to stop.
    assert_eq!(control("off").await.status(), 200);

    // The guardrail: off cannot arm live in one hop.
    let refused = control("live").await;
    assert_eq!(refused.status(), 422);
    let body: Value = refused.json().await.unwrap();
    assert!(body["fields"]["mode"].as_str().is_some());

    // The documented path works.
    let shadow = control("shadow").await;
    assert_eq!(shadow.status(), 200);
    let body: Value = shadow.json().await.unwrap();
    assert_eq!(body["settings"]["mode"], "shadow");
    // The legacy mirror keeps an older reader on the safe side.
    assert_eq!(body["settings"]["dryRun"], true);
    assert_eq!(body["settings"]["paused"], false);

    let live = control("live").await;
    assert_eq!(live.status(), 200);
    let body: Value = live.json().await.unwrap();
    assert_eq!(body["settings"]["mode"], "live");
    assert_eq!(body["settings"]["dryRun"], false);
    assert_eq!(body["settings"]["paused"], false);

    // Turning live off is always allowed, in any state.
    assert_eq!(control("off").await.status(), 200);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn settings_patch_accepts_mode_and_the_legacy_aliases() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    // The legacy alias still means something: paused wins and maps to off.
    let body: Value = client
        .patch(format!("{base}/api/settings"))
        .bearer_auth(TOKEN)
        .json(&json!({ "paused": true, "dryRun": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["settings"]["mode"], "off");

    // An explicit mode is authoritative over the aliases.
    let body: Value = client
        .patch(format!("{base}/api/settings"))
        .bearer_auth(TOKEN)
        .json(&json!({ "mode": "shadow", "paused": false, "dryRun": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["settings"]["mode"], "shadow");

    let invalid = client
        .patch(format!("{base}/api/settings"))
        .bearer_auth(TOKEN)
        .json(&json!({ "mode": "armed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 422);

    // Fulfillment headroom: the deck writes it through the same patch route,
    // and the accepted value comes back in the settings object.
    let body: Value = client
        .patch(format!("{base}/api/settings"))
        .bearer_auth(TOKEN)
        .json(&json!({ "totalPriceMarkupPct": 10 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["settings"]["totalPriceMarkupPct"], 10.0);

    let out_of_range = client
        .patch(format!("{base}/api/settings"))
        .bearer_auth(TOKEN)
        .json(&json!({ "totalPriceMarkupPct": 101 }))
        .send()
        .await
        .unwrap();
    assert_eq!(out_of_range.status(), 422);
    let body: Value = out_of_range.json().await.unwrap();
    assert!(body["fields"]["totalPriceMarkupPct"].as_str().is_some());

    let _ = std::fs::remove_dir_all(&dir);
}
