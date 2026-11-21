//! Portable backups.
//!
//! Two formats:
//! * a versioned JSON document (`kind: "turu-rc-backup"`, format v2) holding
//!   settings, targets, stats, the profit report, the loot log and — optionally
//!   — the session credentials. The trading mode is deliberately absent: a
//!   backup is data, not permission to trade;
//! * a consistent SQLite copy written with `VACUUM INTO` (safe while the
//!   worker is running, unlike copying the `.db` file), with its settings row
//!   reset to [`RESTORE_MODE`] before download.
//!
//! Every import lands in [`RESTORE_MODE`]; no code path lets a file arm live
//! trading on the receiving worker.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::model::{Mode, OrderEntry, Product, Settings, Stats, Target};

mod derive;
mod export;
mod json;
mod parse;
mod paths;
mod restore;

#[cfg(test)]
mod tests;

pub use export::build_backup;
pub use parse::parse_backup;
pub use paths::{backup_filename, database_filename, timestamp};
pub use restore::{create_database_snapshot, restore_backup, sanitize_snapshot_mode};

pub const BACKUP_KIND: &str = "turu-rc-backup";
/// v2 removed the trading mode from the settings block. The bump keeps a build
/// that still defaults a missing `mode` to live from importing a new file.
pub const BACKUP_FORMAT_VERSION: i64 = 2;
pub const BACKUP_MAX_BYTES: usize = 64 * 1024 * 1024;
pub const BACKUP_MAX_ORDERS: usize = 250_000;
pub const BACKUP_DIRNAME: &str = "backups";

/// The only trading mode a restore may produce.
///
/// A backup moves history, targets and settings between machines; it never
/// carries permission to spend money. Both the JSON restore and the downloaded
/// `.db` snapshot force this mode, so a migrated worker always starts in shadow
/// and has to be armed deliberately.
pub const RESTORE_MODE: Mode = Mode::Shadow;

#[derive(Debug, Clone)]
pub struct ReportProduct {
    pub product_key: String,
    pub item_id: String,
    pub label: String,
    pub fills: i64,
    pub failed: i64,
    pub qty: i64,
    /// Units asked for by failed orders and never obtained. Older backups do
    /// not carry it; those rows start at 0 and the next failed attempt adds to
    /// the column normally.
    pub failed_qty: i64,
    pub spent_rlt: f64,
    pub profit_rlt: f64,
    pub first_at: i64,
    pub last_at: i64,
}

impl From<ReportProduct> for Product {
    fn from(value: ReportProduct) -> Self {
        Self {
            item_id: value.item_id,
            label: value.label,
            fills: value.fills,
            // Backups carry no win counter; restore rebuilds it from the loot log.
            wins: 0,
            failed: value.failed,
            qty: value.qty,
            failed_qty: value.failed_qty,
            spent_rlt: value.spent_rlt,
            profit_rlt: value.profit_rlt,
            first_at: value.first_at,
            last_at: value.last_at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedBackup {
    pub format_version: i64,
    pub schema_version: i64,
    pub exported_at: i64,
    pub created_at: i64,
    pub report_since: i64,
    pub settings: Settings,
    pub targets: Vec<Target>,
    pub stats: Stats,
    pub report_products: Vec<ReportProduct>,
    pub orders: Vec<OrderEntry>,
    pub credentials: Option<crate::model::Session>,
}

#[derive(Debug, Clone)]
pub struct ParseOutcome {
    pub ok: bool,
    pub errors: BTreeMap<String, String>,
    pub warnings: Vec<String>,
    pub value: Option<ParsedBackup>,
}

#[derive(Debug, Clone)]
pub struct RestoreResult {
    pub targets: i64,
    pub orders: i64,
    pub report_products: i64,
    pub credentials_restored: bool,
    pub safety_backup: Option<String>,
    pub safety_backup_path: Option<PathBuf>,
}
