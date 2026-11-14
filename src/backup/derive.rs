use std::collections::BTreeMap;

use crate::model::{OrderEntry, Stats};
use crate::rules::round_price;

use super::ReportProduct;

pub(super) fn derive_stats(orders: &[OrderEntry], now: i64) -> Stats {
    let mut since = now;
    let mut spent = 0.0;
    let mut profit = 0.0;
    let mut bought = 0i64;
    let mut filled = 0i64;
    for order in orders {
        if order.status == "filled" {
            filled += 1;
            bought += order.qty;
            spent += order.total_price;
            profit += order.profit_rlt;
        }
        if order.at > 0 && order.at < since {
            since = order.at;
        }
    }
    Stats {
        since,
        profit_rlt: round_price(profit),
        spent_rlt: round_price(spent),
        bought,
        filled,
        failed: orders.len() as i64 - filled,
        orders_sent: orders.len() as i64,
    }
}

pub(super) fn aggregate_products(orders: &[OrderEntry]) -> Vec<ReportProduct> {
    let mut products: BTreeMap<String, ReportProduct> = BTreeMap::new();
    for order in orders {
        let key = {
            let id = order.item_id.trim().to_ascii_lowercase();
            if !id.is_empty() {
                id
            } else {
                let name = order.label.trim().to_ascii_lowercase();
                if name.is_empty() {
                    "unknown".to_string()
                } else {
                    format!("label:{name}")
                }
            }
        };
        let entry = products.entry(key.clone()).or_insert(ReportProduct {
            product_key: key,
            item_id: order.item_id.clone(),
            label: order.label.chars().take(48).collect(),
            fills: 0,
            failed: 0,
            qty: 0,
            spent_rlt: 0.0,
            profit_rlt: 0.0,
            first_at: order.at,
            last_at: order.at,
        });
        entry.first_at = entry.first_at.min(order.at);
        entry.last_at = entry.last_at.max(order.at);
        if order.status == "failed" {
            entry.failed += 1;
        } else {
            entry.fills += 1;
            entry.qty += order.qty.max(0);
            entry.spent_rlt = round_price(entry.spent_rlt + order.total_price.max(0.0));
            entry.profit_rlt = round_price(entry.profit_rlt + order.profit_rlt);
        }
    }
    products.into_values().collect()
}
