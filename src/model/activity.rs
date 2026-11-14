use serde::Serialize;

/// One item the market feed has shown, cached so the monitor can name it
/// without asking upstream. Filled by `item-info` the first time the id is
/// observed; `label` and `rarity` stay empty until that succeeds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogItem {
    pub item_id: String,
    pub item_type: String,
    pub label: String,
    pub rarity: Option<String>,
    pub rarity_color: Option<String>,
    pub first_seen_at: Option<i64>,
    pub last_seen_at: Option<i64>,
    pub resolved_at: Option<i64>,
    /// Times a lookup was attempted, success or not. Used to rotate the batch
    /// so a handful of unnameable ids cannot starve every other row forever.
    pub attempts: i64,
    /// Last time a lookup was attempted. A transport failure is not remembered
    /// as final, so a flaky network must still recover — but not on every
    /// refresh while other rows are waiting.
    pub last_attempt_at: Option<i64>,
}

impl CatalogItem {
    /// True while the display name still has to come from `item-info`.
    pub fn needs_resolution(&self) -> bool {
        self.resolved_at.is_none()
    }
}

/// One `(item, hour)` activity row, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemEventPoint {
    pub item_id: String,
    pub bucket_at: i64,
    pub item_type: Option<String>,
    pub events: i64,
    pub last_seen_at: Option<i64>,
}

/// One ranked row of the item monitor: aggregate over the requested range plus
/// the catalogue metadata the table displays.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemActivityRow {
    pub item_id: String,
    pub item_type: String,
    /// Resolved product name, empty until `item-info` answers.
    pub label: String,
    pub rarity: Option<String>,
    pub rarity_color: Option<String>,
    /// Market frames observed in the selected range.
    pub events: i64,
    /// Frames observed in the last 24 hours, regardless of the range filters:
    /// the heat column keeps working while the table is scoped to an old week.
    pub events_24h: i64,
    pub first_at: Option<i64>,
    pub last_at: Option<i64>,
    /// True when this id is on the target fleet.
    pub tracked: bool,
}

/// Frames and ids for one item type across the whole requested range.
/// Computed before the category/search filters, so the deck can keep every
/// category badge and menu entry visible while the table is narrowed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemTypeTotal {
    pub item_type: String,
    pub events: i64,
    pub items: i64,
}

/// One page of the monitor plus the totals that describe the entire range.
///
/// The totals are computed before filtering and paging, so the per-type summary
/// in the header does not change as the reader selects a category or moves
/// between pages.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemActivityReport {
    /// The requested page, already ranked and sliced.
    pub rows: Vec<ItemActivityRow>,
    /// How many ids matched the filters before the page was cut.
    pub total_rows: usize,
    pub total_events: i64,
    pub type_totals: Vec<ItemTypeTotal>,
}
