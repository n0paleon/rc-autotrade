use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use super::paths::safe_prefix;
use super::{
    BACKUP_FORMAT_VERSION, BACKUP_KIND, backup_filename, build_backup, create_database_snapshot,
    database_filename, parse_backup, restore_backup, sanitize_snapshot_mode, timestamp,
};
use crate::logger::Logger;
use crate::model::{Mode, Target};
use crate::store::Store;

const NOW: i64 = 1_777_982_400_000; // 2026-05-05T12:00:00Z

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rc-rust-backup-{tag}-{nanos}"))
}

fn store(tag: &str) -> (Store, PathBuf) {
    let dir = temp_dir(tag);
    let store = Store::open(dir.clone(), Arc::new(Logger::new("error"))).unwrap();
    (store, dir)
}

fn target(item_id: &str) -> Target {
    Target {
        item_id: item_id.into(),
        label: "Hashboard".into(),
        item_type: "miner".into(),
        rarity_color: Some("8a2be2".into()),
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

#[test]
fn formats_backup_filenames() {
    assert_eq!(timestamp(NOW), "20260505T120000Z");
    assert_eq!(backup_filename(NOW), "turu-rc-backup-20260505T120000Z.json");
    assert_eq!(database_filename(NOW), "turu-rc-db-20260505T120000Z.db");
    assert_eq!(safe_prefix("pre-import"), "pre-import");
    assert_eq!(safe_prefix("  weird/name  "), "weird-name");
    assert_eq!(safe_prefix(""), "snapshot");
}

#[test]
fn round_trips_a_backup_into_a_fresh_store() {
    let (mut source, source_dir) = store("roundtrip-src");
    source.upsert_config(target("6319f7baa8ce530569ed16b9"), None);
    source.flush().unwrap();
    let mut entry = crate::store::make_entry(
        "order-1",
        "6319f7baa8ce530569ed16b9",
        "Hashboard",
        "filled",
        10,
        NOW,
    );
    entry.total_price = 0.9;
    entry.profit_rlt = 0.1;
    source.add_history(entry).unwrap();
    // The engine updates stats before recording the loot row.
    source.state.stats.filled = 1;
    source.state.stats.bought = 10;
    source.state.stats.spent_rlt = 0.9;
    source.state.stats.profit_rlt = 0.1;
    source.state.stats.orders_sent = 1;
    // The source worker is armed live: that is exactly what must not travel.
    source.state.settings.mode = Mode::Live;
    source.flush().unwrap();
    source
        .apply_session_patch(crate::validation::SessionPatch {
            access_token: Some("access-token-123456".into()),
            refresh_token: Some("refresh-token-123456".into()),
            ..Default::default()
        })
        .unwrap();

    let payload = build_backup(&source, true, NOW).unwrap();
    assert_eq!(payload["kind"], BACKUP_KIND);
    assert_eq!(payload["counts"]["targets"], 1);
    assert_eq!(payload["counts"]["orders"], 1);
    assert_eq!(payload["counts"]["reportProducts"], 1);
    let exported_settings = payload["data"]["settings"]
        .as_object()
        .expect("settings block");
    assert!(
        !exported_settings.contains_key("mode"),
        "a backup must not carry the trading mode: {exported_settings:?}"
    );
    assert!(!exported_settings.contains_key("dryRun"));
    assert!(!exported_settings.contains_key("paused"));
    assert_eq!(
        exported_settings["delayPerOrder"],
        source.state.settings.delay_per_order
    );
    assert_eq!(
        payload["credentials"]["session"]["accessToken"],
        "access-token-123456"
    );

    let parsed = parse_backup(&payload, NOW);
    assert!(parsed.ok, "parse errors: {:?}", parsed.errors);
    let value = parsed.value.expect("parsed value");

    let (mut destination, destination_dir) = store("roundtrip-dst");
    let result = restore_backup(&mut destination, &value, true, false, NOW).unwrap();
    assert_eq!(result.targets, 1);
    assert_eq!(result.orders, 1);
    assert_eq!(result.report_products, 1);
    assert!(result.credentials_restored);

    assert_eq!(destination.state.configs.len(), 1);
    assert_eq!(
        destination.state.configs[0].rarity_color.as_deref(),
        Some("8a2be2")
    );
    assert!(destination.has_order("order-1"));
    assert_eq!(
        destination.state.session.access_token,
        "access-token-123456"
    );
    assert_eq!(destination.state.stats.filled, 1);
    assert!(
        destination
            .state
            .report
            .contains_key("6319f7baa8ce530569ed16b9")
    );
    assert_eq!(
        destination.state.settings.mode,
        Mode::Shadow,
        "a restore never inherits the source worker's live mode"
    );

    let _ = std::fs::remove_dir_all(source_dir);
    let _ = std::fs::remove_dir_all(destination_dir);
}

#[test]
fn import_ignores_the_file_trading_mode_and_forces_shadow() {
    let payload = json!({
        "kind": BACKUP_KIND,
        // A pre-bump (v1) backup from an armed worker: the old format still
        // carried `settings.mode`, and migration is exactly where this matters.
        "formatVersion": 1,
        "schemaVersion": crate::db::SCHEMA_VERSION,
        "exportedAt": NOW,
        "data": {
            "createdAt": NOW,
            "reportSince": NOW,
            // A source worker armed live: every mode-shaped key says so. None
            // of it may survive the import.
            "settings": {
                "delayPerOrder": 250,
                "maxOrdersPerMinute": 30,
                "mode": "live",
                "dryRun": false,
                "paused": false,
                "autoRefresh": false,
                "totalPriceMarkupPct": 2.5
            },
            "stats": { "since": NOW },
            "targets": [],
            "orders": [],
            "reportProducts": []
        }
    });

    let parsed = parse_backup(&payload, NOW);
    assert!(parsed.ok, "parse errors: {:?}", parsed.errors);
    assert!(
        parsed
            .warnings
            .iter()
            .any(|warning| warning.contains("shadow mode")),
        "warnings: {:?}",
        parsed.warnings
    );
    let mut value = parsed.value.expect("parsed value");
    assert_eq!(value.settings.mode, Mode::Shadow);
    // The non-trading settings still round-trip.
    assert_eq!(value.settings.delay_per_order, 250);
    assert_eq!(value.settings.max_orders_per_minute, 30);
    assert!(!value.settings.auto_refresh);
    assert_eq!(value.settings.total_price_markup_pct, 2.5);

    // Defense in depth: even a hand-built document that still says live must be
    // written back as shadow, with legacy mirrors that read as dry-run.
    value.settings.mode = Mode::Live;
    let (mut destination, destination_dir) = store("import-mode");
    restore_backup(&mut destination, &value, false, false, NOW).unwrap();
    assert_eq!(destination.state.settings.mode, Mode::Shadow);
    let (mode, dry_run, paused): (String, i64, i64) = destination
        .db
        .query_row(
            "SELECT mode, dry_run, paused FROM settings WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(mode, "shadow");
    assert_eq!((dry_run, paused), (1, 0));

    let _ = std::fs::remove_dir_all(destination_dir);
}

#[test]
fn sqlite_export_snapshot_is_forced_to_shadow() {
    let (mut source, dir) = store("snapshot-mode");
    source.state.settings.mode = Mode::Live;
    source.flush().unwrap();

    let path = create_database_snapshot(&source, "export", NOW).unwrap();
    sanitize_snapshot_mode(&path).unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let (mode, dry_run, paused): (String, i64, i64) = conn
        .query_row(
            "SELECT mode, dry_run, paused FROM settings WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        mode, "shadow",
        "a downloadable .db snapshot must not carry live mode"
    );
    assert_eq!((dry_run, paused), (1, 0));
    drop(conn);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn rejects_a_document_that_is_not_a_backup() {
    let parsed = parse_backup(&json!({ "kind": "something-else" }), NOW);
    assert!(!parsed.ok);
    assert!(parsed.errors.contains_key("kind"));
    assert!(parsed.value.is_none());
}

#[test]
fn rejects_a_newer_format_or_schema() {
    let mut payload = json!({
        "kind": BACKUP_KIND,
        "formatVersion": BACKUP_FORMAT_VERSION + 1,
        "schemaVersion": crate::db::SCHEMA_VERSION,
        "data": {},
    });
    let parsed = parse_backup(&payload, NOW);
    assert!(!parsed.ok);
    assert!(parsed.errors["formatVersion"].contains("newer"));

    payload["formatVersion"] = json!(BACKUP_FORMAT_VERSION);
    payload["schemaVersion"] = json!(crate::db::SCHEMA_VERSION + 1);
    let parsed = parse_backup(&payload, NOW);
    assert!(!parsed.ok);
    assert!(parsed.errors["schemaVersion"].contains("Upgrade the worker"));
}

#[test]
fn skips_invalid_targets_and_rebuilds_the_report_from_orders() {
    let payload = json!({
        "kind": BACKUP_KIND,
        "formatVersion": 1,
        "schemaVersion": crate::db::SCHEMA_VERSION,
        "exportedAt": NOW,
        "data": {
            "createdAt": NOW,
            "reportSince": NOW,
            "settings": { "delayPerOrder": 250, "totalPriceMarkupPct": 12.5 },
            "stats": { "since": NOW },
            "targets": [
                {
                    "itemId": "6319f7baa8ce530569ed16b9",
                    "targetPrice": 0.1,
                    "desiredQty": 10,
                    "highStockThreshold": 20,
                    "minAcceptableQty": 1
                },
                { "itemId": "bad", "targetPrice": 0 }
            ],
            "orders": [
                {
                    "id": "o-1",
                    "itemId": "6319f7baa8ce530569ed16b9",
                    "label": "Hashboard",
                    "qty": 2,
                    "totalPrice": 0.2,
                    "profitRlt": 0.05,
                    "status": "filled",
                    "at": NOW
                }
            ],
            "reportProducts": []
        }
    });

    let parsed = parse_backup(&payload, NOW);
    assert!(parsed.ok, "errors: {:?}", parsed.errors);
    assert!(
        parsed
            .warnings
            .iter()
            .any(|warning| warning.contains("Skipped target")),
        "warnings: {:?}",
        parsed.warnings
    );
    let value = parsed.value.unwrap();
    assert_eq!(value.targets.len(), 1);
    assert_eq!(value.settings.delay_per_order, 250);
    assert_eq!(value.settings.total_price_markup_pct, 12.5);
    assert_eq!(value.report_products.len(), 1);
    assert_eq!(value.report_products[0].fills, 1);
    assert_eq!(value.report_products[0].qty, 2);
    assert!(
        parsed
            .warnings
            .iter()
            .any(|warning| warning.contains("rebuilt from the loot log"))
    );
}

#[test]
fn old_backups_rebuild_win_counters_from_the_loot_log() {
    let payload = json!({
        "kind": BACKUP_KIND,
        "formatVersion": 1,
        "schemaVersion": crate::db::SCHEMA_VERSION,
        "exportedAt": NOW,
        "data": {
            "createdAt": NOW,
            "reportSince": NOW,
            "settings": { "delayPerOrder": 250 },
            "stats": { "since": NOW },
            "targets": [],
            "orders": [
                { "id": "o1", "itemId": "item-1", "label": "Hashboard", "qty": 1,
                  "totalPrice": 0.2, "profitRlt": 0.5, "status": "filled", "at": NOW },
                { "id": "o2", "itemId": "item-1", "label": "Hashboard", "qty": 1,
                  "totalPrice": 0.2, "profitRlt": 0.0, "status": "filled", "at": NOW + 1 },
                { "id": "o3", "itemId": "item-1", "label": "Hashboard", "qty": 1,
                  "totalPrice": 0.2, "profitRlt": -0.25, "status": "filled", "at": NOW + 2 },
                { "id": "o4", "itemId": "item-1", "label": "Hashboard", "qty": 0,
                  "totalPrice": 0.0, "profitRlt": 0.0, "status": "failed", "at": NOW + 3 }
            ],
            // A pre-fix backup: no `wins` field on the aggregate row.
            "reportProducts": [
                { "productKey": "item-1", "itemId": "item-1", "label": "Hashboard",
                  "fills": 3, "failed": 1, "qty": 3, "profitRlt": 0.25 }
            ]
        }
    });

    let parsed = parse_backup(&payload, NOW);
    assert!(parsed.ok, "errors: {:?}", parsed.errors);
    let value = parsed.value.unwrap();

    let (mut destination, _dir) = store("old-winrate-backup");
    restore_backup(&mut destination, &value, false, false, NOW).unwrap();

    let report = destination.report_view();
    assert_eq!(report.totals.fills, 3);
    assert_eq!(report.totals.wins, 2);
    assert_eq!(report.totals.success_pct, Some(50));

    // The session counters come from the same old backup and get the same fix.
    assert_eq!(destination.state.stats.filled, 3);
    assert_eq!(destination.state.stats.wins, 2);
    assert_eq!(destination.state.stats.failed, 1);
    assert_eq!(destination.state.stats.orders_sent, 4);
}

#[test]
fn creates_a_consistent_sqlite_snapshot() {
    let (store, dir) = store("snapshot");
    let path = create_database_snapshot(&store, "pre-import", NOW).unwrap();
    assert!(path.exists());
    let bytes = std::fs::metadata(&path).unwrap().len();
    assert!(bytes > 0);
    assert!(path.to_string_lossy().contains("pre-import-"));
    let _ = std::fs::remove_dir_all(dir);
}
