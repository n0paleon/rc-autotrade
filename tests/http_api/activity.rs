use std::sync::{Arc, Mutex};

use serde_json::Value;

use rollercoin_worker::logger::Logger;
use rollercoin_worker::store::{Store, now_ms};

use crate::{TOKEN, spawn_upstream, spawn_worker, temp_dir};

#[tokio::test]
async fn item_activity_ranks_ids_outside_the_fleet_and_names_them_from_item_info() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls.clone()).await;

    // Two ids that are NOT on the target fleet: the monitor exists to surface
    // exactly those. The catalogue starts empty, so the names must come from
    // upstream during the request.
    {
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger).expect("store");
        let now = now_ms();
        for _ in 0..7 {
            store.note_item_event("stranger-busy", Some("mutation_component"), now);
        }
        for _ in 0..2 {
            store.note_item_event("stranger-quiet", Some("rack"), now);
        }
        store.flush_item_events().expect("flush");
    }

    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let unauthorized = client
        .get(format!("{base}/api/item-activity?days=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);

    let body: Value = client
        .get(format!("{base}/api/item-activity?days=1&limit=10"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["ok"], true);
    let activity = &body["activity"];
    assert_eq!(activity["historyDays"], 30);
    assert_eq!(
        activity["bucketMs"], 300_000,
        "counts are bucketed in 5 minutes"
    );
    assert_eq!(activity["totalEvents"], 9);
    // Pagination metadata describes the whole range, not the page.
    assert_eq!(activity["totalRows"], 2);
    assert_eq!(activity["page"], 1);
    assert_eq!(activity["pages"], 1);
    assert_eq!(activity["hasMore"], false);
    // Per-type summary for the header. The two ids were counted under different
    // feed types, so the total splits accordingly — a later `item-info` lookup
    // must not move a row between categories.
    let type_totals = activity["typeTotals"].as_array().expect("type totals");
    assert_eq!(type_totals.len(), 2);
    let busy_total = type_totals
        .iter()
        .find(|total| total["itemType"] == "mutation_component")
        .expect("parts total");
    assert_eq!(busy_total["events"], 7);
    assert_eq!(busy_total["items"], 1);
    let quiet_total = type_totals
        .iter()
        .find(|total| total["itemType"] == "rack")
        .expect("rack total");
    assert_eq!(quiet_total["events"], 2);
    assert_eq!(quiet_total["items"], 1);
    // The splits add up to the range total.
    let split: i64 = type_totals
        .iter()
        .map(|total| total["events"].as_i64().unwrap())
        .sum();
    assert_eq!(split, activity["totalEvents"].as_i64().unwrap());
    let rows = activity["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["itemId"], "stranger-busy");
    assert_eq!(rows[0]["events"], 7);
    assert_eq!(rows[0]["tracked"], false);
    assert_eq!(rows[0]["itemType"], "mutation_component");
    // The mock upstream answers every item-info call with the same payload, so
    // the row must be named from it rather than left as a raw id.
    assert_eq!(rows[0]["label"], "Rare Hashboard");
    assert_eq!(rows[0]["rarity"], "Rare");
    assert_eq!(activity["unresolved"], 0);
    assert!(
        *upstream_calls.lock().unwrap() > 0,
        "the first response must resolve names upstream"
    );

    // The second call must not hit upstream again: the catalogue remembered the
    // names, and a miss would be remembered too.
    let before = *upstream_calls.lock().unwrap();
    let cached: Value = client
        .get(format!("{base}/api/item-activity?days=1&limit=10"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(*upstream_calls.lock().unwrap(), before, "names are cached");
    assert_eq!(
        cached["activity"]["rows"][0]["label"], "Rare Hashboard",
        "the cached name still renders"
    );

    // Category filters read the same rows. Each id keeps the type its frames
    // were counted under, so the filter and the per-type summary agree even
    // after `item-info` supplies the names.
    let filtered: Value = client
        .get(format!(
            "{base}/api/item-activity?days=1&itemType=mutation_component"
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let typed_rows = filtered["activity"]["rows"].as_array().unwrap();
    assert_eq!(typed_rows.len(), 1);
    assert_eq!(typed_rows[0]["itemId"], "stranger-busy");
    assert_eq!(
        filtered["activity"]["totalEvents"], 7,
        "the totals follow the type filter"
    );
    // The header badges do not: they keep one count per type across the whole
    // range, so the rack badge stays visible while the table shows parts only.
    let filtered_totals = filtered["activity"]["typeTotals"].as_array().unwrap();
    assert_eq!(filtered_totals.len(), 2);
    assert_eq!(
        filtered_totals
            .iter()
            .find(|total| total["itemType"] == "rack")
            .expect("rack total")["events"],
        2
    );

    let rack: Value = client
        .get(format!("{base}/api/item-activity?days=1&itemType=rack"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rack_rows = rack["activity"]["rows"].as_array().unwrap();
    assert_eq!(rack_rows.len(), 1);
    assert_eq!(rack_rows[0]["itemId"], "stranger-quiet");

    let no_match: Value = client
        .get(format!("{base}/api/item-activity?days=1&itemType=battery"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        no_match["activity"]["rows"].as_array().unwrap().is_empty(),
        "a type nobody reported must match nothing"
    );

    let empty: Value = client
        .get(format!("{base}/api/item-activity?days=1&q=does-not-exist"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty["activity"]["rows"].as_array().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn item_activity_paginates_without_changing_the_range_totals() {
    let dir = temp_dir();
    let upstream_calls = Arc::new(Mutex::new(0));
    let upstream = spawn_upstream(upstream_calls).await;

    // Five ids with strictly decreasing volume, so the ranking is unambiguous:
    // p-4 (5 frames) down to p-0 (1 frame).
    {
        let logger = Arc::new(Logger::new("error"));
        let mut store = Store::open(dir.clone(), logger).expect("store");
        let now = now_ms();
        for index in 0..5 {
            let item_type = if index % 2 == 0 { "miner" } else { "rack" };
            for _ in 0..=index {
                store.note_item_event(&format!("p-{index}"), Some(item_type), now);
            }
        }
        store.flush_item_events().expect("flush");
    }

    let base = spawn_worker(dir.clone(), upstream).await;
    let client = reqwest::Client::new();

    let page_one: Value = client
        .get(format!("{base}/api/item-activity?days=1&limit=2&offset=0"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first = &page_one["activity"];
    assert_eq!(first["rows"].as_array().unwrap().len(), 2);
    assert_eq!(first["totalRows"], 5);
    assert_eq!(first["pages"], 3);
    assert_eq!(first["page"], 1);
    assert_eq!(first["hasMore"], true);
    assert_eq!(first["rows"][0]["itemId"], "p-4");
    assert_eq!(first["rows"][1]["itemId"], "p-3");
    // 5+4+3+2+1
    assert_eq!(first["totalEvents"], 15);
    // The share column divides every row by this, so it has to count the whole
    // range rather than the page: 5 of 15 must read 33%, not 100%.
    let split: i64 = first["typeTotals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|total| total["events"].as_i64().unwrap())
        .sum();
    assert_eq!(split, 15, "the type totals add up to the range total");
    assert!(
        first.get("peakEvents").is_none(),
        "the share column no longer needs a separate peak"
    );

    let page_three: Value = client
        .get(format!("{base}/api/item-activity?days=1&limit=2&offset=4"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let last = &page_three["activity"];
    assert_eq!(last["rows"].as_array().unwrap().len(), 1);
    assert_eq!(last["rows"][0]["itemId"], "p-0");
    assert_eq!(last["page"], 3);
    assert_eq!(last["hasMore"], false);
    // Paging never changes the totals: they describe the range.
    assert_eq!(last["totalEvents"], first["totalEvents"]);
    assert_eq!(last["totalRows"], first["totalRows"]);
    assert_eq!(last["typeTotals"], first["typeTotals"]);

    // An offset past the end is an empty page that still reports the real count.
    let past: Value = client
        .get(format!("{base}/api/item-activity?days=1&limit=2&offset=99"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(past["activity"]["rows"].as_array().unwrap().is_empty());
    assert_eq!(past["activity"]["totalRows"], 5);

    let _ = std::fs::remove_dir_all(&dir);
}
