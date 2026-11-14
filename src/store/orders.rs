use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::clock::now_ms;
use crate::constants::{
    LOOT_LOG_MAX_PAGE_SIZE, LOOT_LOG_PAGE_SIZE, MAX_HISTORY, MAX_REPORT_PRODUCTS,
};
use crate::model::{OrderEntry, Product, ProductView, ReportTotals, ReportView};
use crate::rules::round_price;

use super::Store;
use super::rows::{row_to_entry, row_to_entry_lite};
use super::state::{HistoryPage, HistoryQuery};
use super::targets::{escape_like, product_key};

impl Store {
    /// Appends one confirmed/failed order to the permanent loot log.
    pub fn add_history(&mut self, entry: OrderEntry) -> Result<(), String> {
        self.state.history.insert(0, entry.clone());
        if self.state.history.len() > MAX_HISTORY {
            self.state.history.truncate(MAX_HISTORY);
        }

        self.record_product(&entry, true);

        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|e| format!("could not start transaction: {e}"))?;
        tx.execute(
            "INSERT OR REPLACE INTO orders
             (id, order_id, item_id, label, qty, requested_qty, stock_at_trigger, unit_price, total_price, target_price,
              profit_rlt, status, at, latency_ms, timings, raw_events, note, message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                entry.id,
                entry.order_id,
                entry.item_id,
                entry.label,
                entry.qty,
                entry.requested_qty,
                entry.stock_at_trigger,
                entry.unit_price,
                entry.total_price,
                entry.target_price,
                entry.profit_rlt,
                entry.status,
                entry.at,
                entry.latency_ms,
                entry
                    .timings
                    .as_ref()
                    .and_then(|t| serde_json::to_string(t).ok()),
                entry
                    .raw_events
                    .as_ref()
                    .and_then(crate::raw_events::encode),
                entry.note,
                entry.message
            ],
        )
        .map_err(|e| format!("could not write order {}: {e}", entry.id))?;
        let stats = self.state.stats;
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
        .map_err(|e| format!("could not write stats: {e}"))?;
        tx.commit()
            .map_err(|e| format!("could not commit order: {e}"))?;
        Ok(())
    }

    pub fn has_order(&self, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        self.db
            .query_row(
                "SELECT 1 FROM orders WHERE id = ?1",
                params![id],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Updates late, non-blocking diagnostics without rewriting the loot row.
    pub fn update_history_timings(&mut self, id: &str, raw: &Value) -> bool {
        let Some(incoming) = crate::timings::sanitize(raw) else {
            return false;
        };
        let existing: Option<String> = self
            .db
            .query_row(
                "SELECT timings FROM orders WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        let Some(existing) = existing else {
            return false;
        };

        // Merge: new marks win, old marks survive a partial update.
        let mut merged: Value = serde_json::from_str(&existing).unwrap_or(Value::Null);
        let patch_value = serde_json::to_value(&incoming).unwrap_or(Value::Null);
        if let (Some(base), Some(patch)) = (merged.as_object_mut(), patch_value.as_object()) {
            for (key, value) in patch {
                base.insert(key.clone(), value.clone());
            }
        }
        let Some(timings) = crate::timings::sanitize(&merged) else {
            return false;
        };
        let encoded = serde_json::to_string(&timings).unwrap_or_default();
        let changes = self
            .db
            .execute(
                "UPDATE orders SET timings = ?1 WHERE id = ?2",
                params![encoded, id],
            )
            .unwrap_or(0);
        if changes == 0 {
            return false;
        }
        if let Some(entry) = self.state.history.iter_mut().find(|row| row.id == id) {
            entry.timings = Some(timings);
        }
        true
    }

    pub fn history_count(&self) -> i64 {
        self.db
            .query_row("SELECT COUNT(*) FROM orders", [], |row| row.get(0))
            .unwrap_or(0)
    }

    /// One page of the permanent loot log, newest first.
    pub fn history_page(&self, query: HistoryQuery) -> Result<HistoryPage, String> {
        let term = query.q.trim().chars().take(64).collect::<String>();
        let like = if term.is_empty() {
            String::new()
        } else {
            format!("%{}%", escape_like(&term))
        };
        let status = if query.status == "filled" || query.status == "failed" {
            query.status.clone()
        } else {
            String::new()
        };
        let profit = if query.profit == "positive" || query.profit == "negative" {
            query.profit.clone()
        } else {
            String::new()
        };
        let page_size = query
            .limit
            .unwrap_or(LOOT_LOG_PAGE_SIZE)
            .clamp(1, LOOT_LOG_MAX_PAGE_SIZE);

        let cursor = query.before_at > 0 && !query.before_id.is_empty();
        let skip = if cursor { 0 } else { query.offset.max(0) };

        // `(?3 = 'positive' AND profit_rlt > 0) OR (?3 = 'negative' AND profit_rlt < 0)
        //  OR ?3 = ''` — empty means no profit filter.
        let profit_clause = "((?3 = '' OR (?3 = 'positive' AND profit_rlt > 0) OR (?3 = 'negative' AND profit_rlt < 0)))";
        let (sql, bindings): (String, Vec<Box<dyn rusqlite::ToSql>>) = if cursor {
            (
                format!(
                    "SELECT id, order_id, item_id, label, qty, requested_qty, stock_at_trigger, unit_price, total_price,
                            target_price, profit_rlt, status, at, latency_ms,
                            note, message
                     FROM orders
                     WHERE (?1 = '' OR label LIKE ?1 ESCAPE '\\' OR item_id LIKE ?1 ESCAPE '\\')
                       AND (?2 = '' OR status = ?2)
                       AND {profit_clause}
                       AND (at < ?4 OR (at = ?4 AND id < ?5))
                     ORDER BY at DESC, id DESC LIMIT ?6"
                ),
                vec![
                    Box::new(like),
                    Box::new(status),
                    Box::new(profit),
                    Box::new(query.before_at),
                    Box::new(query.before_id),
                    Box::new((page_size + 1) as i64),
                ],
            )
        } else {
            (
                format!(
                    "SELECT id, order_id, item_id, label, qty, requested_qty, stock_at_trigger, unit_price, total_price,
                            target_price, profit_rlt, status, at, latency_ms,
                            note, message
                     FROM orders
                     WHERE (?1 = '' OR label LIKE ?1 ESCAPE '\\' OR item_id LIKE ?1 ESCAPE '\\')
                       AND (?2 = '' OR status = ?2)
                       AND {profit_clause}
                     ORDER BY at DESC, id DESC LIMIT ?4 OFFSET ?5"
                ),
                vec![
                    Box::new(like),
                    Box::new(status),
                    Box::new(profit),
                    Box::new((page_size + 1) as i64),
                    Box::new(skip),
                ],
            )
        };

        let mut statement = self
            .db
            .prepare(&sql)
            .map_err(|e| format!("could not prepare history page: {e}"))?;
        let rows: Vec<OrderEntry> = {
            let refs: Vec<&dyn rusqlite::ToSql> = bindings.iter().map(|b| b.as_ref()).collect();
            let mapped = statement
                .query_map(refs.as_slice(), row_to_entry_lite)
                .map_err(|e| format!("could not query history: {e}"))?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("could not decode history: {e}"))?
        };

        let total: i64 = self
            .db
            .query_row(
                "SELECT COUNT(*) FROM orders
                 WHERE (?1 = '' OR label LIKE ?1 ESCAPE '\\' OR item_id LIKE ?1 ESCAPE '\\')
                   AND (?2 = '' OR status = ?2)
                   AND (?3 = '' OR (?3 = 'positive' AND profit_rlt > 0)
                        OR (?3 = 'negative' AND profit_rlt < 0))",
                params![
                    if term.is_empty() {
                        String::new()
                    } else {
                        format!("%{}%", escape_like(&term))
                    },
                    if query.status == "filled" || query.status == "failed" {
                        query.status
                    } else {
                        String::new()
                    },
                    if query.profit == "positive" || query.profit == "negative" {
                        query.profit
                    } else {
                        String::new()
                    }
                ],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let has_more = rows.len() > page_size;
        Ok(HistoryPage {
            entries: rows.into_iter().take(page_size).collect(),
            total,
            limit: page_size,
            offset: skip,
            has_more,
        })
    }

    /// Reads the two heavy JSON columns for a single row.
    ///
    /// Lists never select `timings`/`raw_events`: the SQLite profile timer keeps
    /// running until the statement resets, so decoding frames while iterating
    /// rows is reported as a slow SELECT even when the query itself is cheap.
    /// The deck asks for one entry at a time when its detail modal opens.
    pub fn order_detail(&self, id: &str) -> Result<Option<OrderEntry>, String> {
        self.db
            .query_row(
                "SELECT id, order_id, item_id, label, qty, requested_qty, stock_at_trigger, unit_price, total_price,
                        target_price, profit_rlt, status, at, latency_ms, timings, raw_events,
                        note, message
                 FROM orders WHERE id = ?1",
                params![id],
                row_to_entry,
            )
            .optional()
            .map_err(|e| format!("could not read order {id}: {e}"))
    }

    // ── report ──────────────────────────────────────────────────────────────

    fn record_product(&mut self, entry: &OrderEntry, persist: bool) {
        let key = product_key(&entry.item_id, &entry.label);
        let at = if entry.at > 0 { entry.at } else { now_ms() };
        let product = self
            .state
            .report
            .entry(key.clone())
            .or_insert_with(|| Product {
                item_id: entry.item_id.clone(),
                label: entry.label.chars().take(48).collect(),
                fills: 0,
                failed: 0,
                qty: 0,
                spent_rlt: 0.0,
                profit_rlt: 0.0,
                first_at: at,
                last_at: at,
            });

        if !entry.label.is_empty() {
            product.label = entry.label.chars().take(48).collect();
        }
        if !entry.item_id.is_empty() {
            product.item_id = entry.item_id.clone();
        }
        product.first_at = product.first_at.min(at);
        product.last_at = product.last_at.max(at);

        if entry.status == "failed" {
            product.failed += 1;
        } else {
            product.fills += 1;
            product.qty += entry.qty.max(0);
            product.spent_rlt = round_price(product.spent_rlt + entry.total_price.max(0.0));
            product.profit_rlt = round_price(product.profit_rlt + entry.profit_rlt);
        }

        self.trim_report(persist);
        if persist && let Some(product) = self.state.report.get(&key) {
            let _ = self.write_product(&key, product);
        }
    }

    fn trim_report(&mut self, persist: bool) {
        if self.state.report.len() <= MAX_REPORT_PRODUCTS {
            return;
        }
        let mut keys: Vec<(String, f64)> = self
            .state
            .report
            .iter()
            .map(|(key, product)| (key.clone(), product.profit_rlt))
            .collect();
        keys.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let drop_count = keys.len() - MAX_REPORT_PRODUCTS;
        for (key, _) in keys.into_iter().take(drop_count) {
            self.state.report.remove(&key);
            if persist {
                let _ = self.db.execute(
                    "DELETE FROM report_products WHERE product_key = ?1",
                    params![key],
                );
            }
        }
    }

    fn write_product(&self, key: &str, product: &Product) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO report_products
             (product_key, item_id, label, fills, failed, qty, spent_rlt, profit_rlt,
              first_at, last_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(product_key) DO UPDATE SET
               item_id = excluded.item_id,
               label = excluded.label,
               fills = excluded.fills,
               failed = excluded.failed,
               qty = excluded.qty,
               spent_rlt = excluded.spent_rlt,
               profit_rlt = excluded.profit_rlt,
               first_at = excluded.first_at,
               last_at = excluded.last_at",
            params![
                key,
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
        )?;
        Ok(())
    }

    /// All-time profit grouped by product.
    pub fn report_view(&self) -> ReportView {
        let mut items: Vec<ProductView> = self
            .state
            .report
            .values()
            .map(|product| {
                let completed = product.fills + product.failed;
                ProductView {
                    item_id: product.item_id.clone(),
                    label: product.label.clone(),
                    fills: product.fills,
                    failed: product.failed,
                    qty: product.qty,
                    spent_rlt: round_price(product.spent_rlt),
                    profit_rlt: round_price(product.profit_rlt),
                    first_at: product.first_at,
                    last_at: product.last_at,
                    avg_buy: if product.qty > 0 {
                        round_price(product.spent_rlt / product.qty as f64)
                    } else {
                        0.0
                    },
                    margin_pct: if product.spent_rlt > 0.0 {
                        Some((product.profit_rlt / product.spent_rlt * 1000.0).round() / 10.0)
                    } else {
                        None
                    },
                    success_pct: if completed > 0 {
                        Some((product.fills as f64 / completed as f64 * 100.0).round() as i64)
                    } else {
                        None
                    },
                }
            })
            .collect();

        items.sort_by(|a, b| {
            b.profit_rlt
                .partial_cmp(&a.profit_rlt)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    b.spent_rlt
                        .partial_cmp(&a.spent_rlt)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then_with(|| a.label.cmp(&b.label))
        });

        let mut totals = ReportTotals {
            products: items.len(),
            fills: 0,
            failed: 0,
            qty: 0,
            spent_rlt: 0.0,
            profit_rlt: 0.0,
            margin_pct: None,
        };
        for item in &items {
            totals.fills += item.fills;
            totals.failed += item.failed;
            totals.qty += item.qty;
            totals.spent_rlt += item.spent_rlt;
            totals.profit_rlt += item.profit_rlt;
        }
        totals.spent_rlt = round_price(totals.spent_rlt);
        totals.profit_rlt = round_price(totals.profit_rlt);
        totals.margin_pct = if totals.spent_rlt > 0.0 {
            Some((totals.profit_rlt / totals.spent_rlt * 1000.0).round() / 10.0)
        } else {
            None
        };

        ReportView {
            since: self.state.report_since,
            totals,
            items,
        }
    }
}
