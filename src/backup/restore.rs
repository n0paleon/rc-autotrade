use std::path::PathBuf;

use rusqlite::params;

use crate::store::Store;

use super::paths::{safe_prefix, timestamp};
use super::{BACKUP_DIRNAME, ParsedBackup, RestoreResult};

/// Writes a consistent copy of the live database with `VACUUM INTO`.
pub fn create_database_snapshot(store: &Store, prefix: &str, now: i64) -> Result<PathBuf, String> {
    let dir = store.data_dir.join(BACKUP_DIRNAME);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    let base = format!("{}-{}", safe_prefix(prefix), timestamp(now));
    let mut file = dir.join(format!("{base}.db"));
    for index in 2.. {
        if !file.exists() {
            break;
        }
        file = dir.join(format!("{base}-{index}.db"));
    }

    store
        .db
        .execute("VACUUM INTO ?1", params![file.to_string_lossy()])
        .map_err(|error| format!("Could not write the database snapshot: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

/// Replaces the portable tables with a validated backup inside one transaction.
pub fn restore_backup(
    store: &mut Store,
    backup: &ParsedBackup,
    restore_credentials: bool,
    snapshot: bool,
    now: i64,
) -> Result<RestoreResult, String> {
    let safety = if snapshot {
        Some(create_database_snapshot(store, "pre-import", now)?)
    } else {
        None
    };

    let settings = backup.settings;
    let stats = backup.stats;
    let targets = backup.targets.clone();
    let orders = backup.orders.clone();
    let products = backup.report_products.clone();
    let session = if restore_credentials {
        backup.credentials.clone()
    } else {
        None
    };

    let tx = store
        .db
        .unchecked_transaction()
        .map_err(|error| format!("could not start restore transaction: {error}"))?;

    for table in [
        "market_snapshot",
        "price_points",
        "orders",
        "report_products",
        "targets",
    ] {
        tx.execute(&format!("DELETE FROM {table}"), [])
            .map_err(|error| format!("could not clear {table}: {error}"))?;
    }

    let (dry_run, paused) = settings.legacy_flags();
    tx.execute(
        "UPDATE settings SET delay_per_order = ?1, max_orders_per_minute = ?2,
                dry_run = ?3, auto_refresh = ?4, paused = ?5, mode = ?6,
                total_price_markup_pct = ?7 WHERE id = 1",
        params![
            settings.delay_per_order,
            settings.max_orders_per_minute,
            dry_run as i64,
            settings.auto_refresh as i64,
            paused as i64,
            settings.mode.as_str(),
            settings.total_price_markup_pct
        ],
    )
    .map_err(|error| format!("could not restore settings: {error}"))?;

    tx.execute(
        "UPDATE stats SET since = ?1, profit_rlt = ?2, spent_rlt = ?3, bought = ?4,
                filled = ?5, failed = ?6, orders_sent = ?7 WHERE id = 1",
        params![
            stats.since,
            stats.profit_rlt,
            stats.spent_rlt,
            stats.bought,
            stats.filled,
            stats.failed,
            stats.orders_sent
        ],
    )
    .map_err(|error| format!("could not restore stats: {error}"))?;

    for target in &targets {
        tx.execute(
            "INSERT INTO targets
             (item_id, label, item_type, rarity_color, target_price, desired_qty,
              high_stock_threshold, min_acceptable_qty, allow_partial_order, active,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                target.item_id,
                target.label,
                target.item_type,
                target.rarity_color,
                target.target_price,
                target.desired_qty,
                target.high_stock_threshold,
                target.min_acceptable_qty,
                target.allow_partial_order as i64,
                target.active as i64,
                target.created_at,
                target.updated_at
            ],
        )
        .map_err(|error| format!("could not restore target: {error}"))?;
    }

    for order in &orders {
        tx.execute(
            "INSERT OR REPLACE INTO orders
             (id, order_id, item_id, label, qty, requested_qty, stock_at_trigger, unit_price, total_price, target_price,
              profit_rlt, status, at, latency_ms, timings, raw_events, note, message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                order.id,
                order.order_id,
                order.item_id,
                order.label,
                order.qty,
                order.requested_qty,
                order.stock_at_trigger,
                order.unit_price,
                order.total_price,
                order.target_price,
                order.profit_rlt,
                order.status,
                order.at,
                order.latency_ms,
                order
                    .timings
                    .as_ref()
                    .and_then(|timings| serde_json::to_string(timings).ok()),
                order
                    .raw_events
                    .as_ref()
                    .and_then(crate::raw_events::encode),
                order.note,
                order.message
            ],
        )
        .map_err(|error| format!("could not restore loot entry: {error}"))?;
    }

    for product in &products {
        tx.execute(
            "INSERT INTO report_products
             (product_key, item_id, label, fills, failed, qty, spent_rlt, profit_rlt,
              first_at, last_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                product.product_key,
                product.item_id,
                product.label,
                product.fills,
                product.failed,
                product.qty,
                product.spent_rlt,
                product.profit_rlt,
                product.first_at,
                product.last_at
            ],
        )
        .map_err(|error| format!("could not restore profit row: {error}"))?;
    }

    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('created_at', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![backup.created_at.to_string()],
    )
    .map_err(|error| format!("could not restore meta: {error}"))?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('report_since', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![backup.report_since.to_string()],
    )
    .map_err(|error| format!("could not restore meta: {error}"))?;

    if let Some(session) = &session {
        tx.execute(
            "UPDATE session SET access_token = ?1, refresh_token = ?2, cookie = ?3,
                    source = ?4, updated_at = ?5, last_refresh_at = ?6,
                    refresh_count = ?7, last_error = ?8 WHERE id = 1",
            params![
                session.access_token,
                session.refresh_token,
                session.cookie,
                session
                    .source
                    .clone()
                    .unwrap_or_else(|| "backup".to_string()),
                session.updated_at.unwrap_or(now),
                session.last_refresh_at,
                session.refresh_count,
                session.last_error
            ],
        )
        .map_err(|error| format!("could not restore session: {error}"))?;
    }

    tx.commit()
        .map_err(|error| format!("could not commit restore: {error}"))?;

    store.reload()?;

    let safety_backup = safety.as_ref().map(|path| {
        path.strip_prefix(&store.data_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    });

    Ok(RestoreResult {
        targets: targets.len() as i64,
        orders: orders.len() as i64,
        report_products: products.len() as i64,
        credentials_restored: session.is_some(),
        safety_backup,
        safety_backup_path: safety,
    })
}
