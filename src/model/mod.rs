//! Wire and storage types.
//!
//! Every struct serializes with `camelCase` so the Rust worker can serve the
//! existing React deck without a translation layer.

mod activity;
mod market;
mod orders;
mod settings;
mod stats;
mod target;

#[cfg(test)]
mod tests;

pub use activity::{
    CatalogItem, ItemActivityReport, ItemActivityRow, ItemEventPoint, ItemTypeTotal,
};
pub use market::{MarketSnapshot, PriceBand, PricePoint, PriceSeriesPoint};
pub use orders::{OrderEntry, Product, ProductView, ReportTotals, ReportView};
pub use settings::Settings;
pub use stats::{Session, Stats};
pub use target::{Mode, Target};

fn default_item_type() -> String {
    "mutation_component".to_string()
}

fn default_true() -> bool {
    true
}
