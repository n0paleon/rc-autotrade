use std::collections::BTreeMap;

use crate::clock::now_ms;
use crate::constants::{EVENT_BUCKET_MS, ITEM_EVENT_BUCKET_MS};
use crate::model::{OrderEntry, Product, Session, Settings, Stats, Target};

#[derive(Debug, Clone)]
pub struct State {
    pub created_at: i64,
    pub updated_at: i64,
    pub worker_token_hash: Option<String>,
    pub worker_token_preview: Option<String>,
    pub session: Session,
    pub configs: Vec<Target>,
    pub settings: Settings,
    pub stats: Stats,
    pub report_since: i64,
    pub report: BTreeMap<String, Product>,
    pub history: Vec<OrderEntry>,
}

impl State {
    pub(super) fn fresh() -> Self {
        let now = now_ms();
        Self {
            created_at: now,
            updated_at: now,
            worker_token_hash: None,
            worker_token_preview: None,
            session: Session::default(),
            configs: Vec::new(),
            settings: Settings::default(),
            stats: Stats {
                since: now,
                profit_rlt: 0.0,
                spent_rlt: 0.0,
                bought: 0,
                filled: 0,
                failed: 0,
                orders_sent: 0,
            },
            report_since: now,
            report: BTreeMap::new(),
            history: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub entries: Vec<OrderEntry>,
    pub total: i64,
    pub limit: usize,
    pub offset: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    pub q: String,
    pub status: String,
    /// "" (all) | "positive" (profit_rlt > 0) | "negative" (profit_rlt < 0).
    /// Rows with exactly zero profit match neither bucket.
    pub profit: String,
    pub limit: Option<usize>,
    pub offset: i64,
    pub before_at: i64,
    pub before_id: String,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ItemEventAccum {
    pub(super) events: i64,
    pub(super) last_seen_at: Option<i64>,
    pub(super) item_type: Option<String>,
}

/// One `item_event_points` row on its way into the memory map.
#[derive(Debug, Clone)]
pub(super) struct ItemEventRow {
    pub(super) key: (i64, String),
    pub(super) accum: ItemEventAccum,
}

/// Start of the 5-minute bucket that owns `at`.
pub fn event_bucket_at(at: i64) -> i64 {
    at.div_euclid(EVENT_BUCKET_MS) * EVENT_BUCKET_MS
}

/// Meta key holding the first instant counted by the marketplace-only event
/// scope. A database written before the scope was narrowed has no key, so the
/// first boot stamps it and the API hides the older all-frame buckets.
pub(super) const MARKET_EVENT_SCOPE_KEY: &str = "market_event_scope_at";

/// Start of the 1-hour bucket that owns `at` for the per-item monitor.
pub fn item_event_bucket_at(at: i64) -> i64 {
    at.div_euclid(ITEM_EVENT_BUCKET_MS) * ITEM_EVENT_BUCKET_MS
}

/// One aggregation request for [`crate::store::Store::item_activity`]. Deserialised straight
/// from `GET /api/item-activity`: the deck sends a date range, an optional item
/// type, an optional search term and a row cap.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemActivityQuery {
    /// Inclusive start of the range, epoch ms. 0 means "retention start".
    #[serde(default)]
    pub since: i64,
    /// Inclusive end of the range, epoch ms. 0 means "now".
    #[serde(default)]
    pub until: i64,
    /// "" (all) or one of the item types the feed reports.
    #[serde(rename = "itemType", default)]
    pub item_type: String,
    /// Case-insensitive substring over the resolved name and the item id.
    #[serde(default)]
    pub q: String,
    /// Rows per page. Clamped to [`crate::constants::ITEM_ACTIVITY_LIMIT`].
    #[serde(default)]
    pub limit: usize,
    /// Rows to skip after ranking, for pagination.
    #[serde(default)]
    pub offset: usize,
}
