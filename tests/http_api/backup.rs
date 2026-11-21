//! Backup export/import trading-mode safety through the real HTTP surface.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::{ITEM_ID, TOKEN, spawn_upstream, spawn_worker, temp_dir};

fn live_backup() -> Value {
    let now = 1_777_982_400_000i64;
    json!({
        "kind": "turu-rc-backup",
        "formatVersion": 2,
        "schemaVersion": rollercoin_worker::db::SCHEMA_VERSION,
        "exportedAt": now,
        "data": {
            "createdAt": now,
            "reportSince": now,
            // A file written by an armed worker: the import must ignore every
            // mode-shaped key.
            "settings": {
                "delayPerOrder": 250,
                "maxOrdersPerMinute": 30,
                "mode": "live",
                "dryRun": false,
                "paused": false
            },
            "stats": { "since": now },
            "targets": [{
                "itemId": ITEM_ID,
                "targetPrice": 0.1,
                "desiredQty": 10,
                "highStockThreshold": 20,
                "minAcceptableQty": 1
            }],
            "orders": [],
            "reportProducts": []
        }
    })
}

#[tokio::test]
async fn backup_endpoints_never_export_or_import_live_mode() {
    let dir = temp_dir();
    let upstream = spawn_upstream(Arc::new(Mutex::new(0))).await;
    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    // A fresh worker starts armed live. The portable export must not carry it.
    let state: Value = client
        .get(format!("{base}/api/state"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["settings"]["mode"], "live");

    let response = client
        .get(format!("{base}/api/backup"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let backup: Value = response.json().await.unwrap();
    assert_eq!(backup["formatVersion"], 2);
    let settings = backup["data"]["settings"].as_object().unwrap();
    assert!(!settings.contains_key("mode"));
    assert!(!settings.contains_key("dryRun"));
    assert!(!settings.contains_key("paused"));

    // A live-mode document from disk imports in shadow mode.
    let response = client
        .post(format!("{base}/api/backup?credentials=0"))
        .bearer_auth(TOKEN)
        .json(&live_backup())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert!(
        body["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap_or("").contains("shadow mode")),
        "warnings: {body:?}"
    );

    let state: Value = client
        .get(format!("{base}/api/state"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["settings"]["mode"], "shadow");
    assert_eq!(state["settings"]["delayPerOrder"], 250);
    assert_eq!(state["configs"][0]["itemId"], ITEM_ID);

    // The downloadable .db copy is scrubbed too: swapping it in by hand lands
    // the next worker in shadow, with legacy mirrors that read as dry-run.
    let bytes = client
        .get(format!("{base}/api/backup?format=sqlite"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let path = dir.join("download.db");
    std::fs::write(&path, &bytes).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let (mode, dry_run, paused): (String, i64, i64) = conn
        .query_row(
            "SELECT mode, dry_run, paused FROM settings WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(mode, "shadow");
    assert_eq!((dry_run, paused), (1, 0));
    drop(conn);

    let _ = std::fs::remove_dir_all(&dir);
}
