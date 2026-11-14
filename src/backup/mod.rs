//! Portable backups.
//!
//! Two formats:
//! * a versioned JSON document (`kind: "turu-rc-backup"`, format v1) holding
//!   settings, targets, stats, the profit report, the loot log and — optionally
//!   — the session credentials;
//! * a consistent SQLite copy written with `VACUUM INTO` (safe while the
//!   worker is running, unlike copying the `.db` file).

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::model::{OrderEntry, Product, Settings, Stats, Target};

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
pub use restore::{create_database_snapshot, restore_backup};

pub const BACKUP_KIND: &str = "turu-rc-backup";
pub const BACKUP_FORMAT_VERSION: i64 = 1;
pub const BACKUP_MAX_BYTES: usize = 64 * 1024 * 1024;
pub const BACKUP_MAX_ORDERS: usize = 250_000;
pub const BACKUP_DIRNAME: &str = "backups";

#[derive(Debug, Clone)]
pub struct ReportProduct {
    pub product_key: String,
    pub item_id: String,
    pub label: String,
    pub fills: i64,
    pub failed: i64,
    pub qty: i64,
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
            failed: value.failed,
            qty: value.qty,
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
