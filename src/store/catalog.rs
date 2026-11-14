use rusqlite::params;

use crate::model::CatalogItem;

use super::Store;

impl Store {
    /// Marks a catalogue id as looked up, even when upstream did not know it.
    /// Recording the miss keeps a deleted marketplace item from being retried
    /// on every monitor refresh.
    pub fn mark_catalog_resolved(&mut self, item_id: &str, resolved_at: i64) {
        let key = item_id.to_ascii_lowercase();
        let Some(entry) = self.catalog.get_mut(&key) else {
            return;
        };
        entry.resolved_at = Some(resolved_at);
        entry.attempts += 1;
        entry.last_attempt_at = Some(resolved_at);
        let mut row = entry.clone();
        row.item_id = key;
        if let Err(error) = self.write_catalog_item(&row) {
            self.report_write_error("could not save item catalog", &error);
        }
    }

    /// Records that a lookup was attempted without deciding its outcome.
    ///
    /// Used for a transport failure: the id stays resolvable so a later refresh
    /// retries it, but the attempt count moves it to the back of the batch so it
    /// cannot starve every other row while the network is down. `None` performs
    /// no write, which is what a definitive success or miss needs — those are
    /// recorded by [`Self::apply_catalog_item`] / [`Self::mark_catalog_resolved`].
    pub fn note_catalog_attempts(&mut self, item_id: &str, at: i64) {
        let key = item_id.to_ascii_lowercase();
        let Some(entry) = self.catalog.get_mut(&key) else {
            return;
        };
        entry.attempts += 1;
        entry.last_attempt_at = Some(at);
        let mut row = entry.clone();
        row.item_id = key;
        if let Err(error) = self.write_catalog_item(&row) {
            self.report_write_error("could not save item catalog attempt", &error);
        }
    }

    /// Stores a resolved `item-info` payload on the catalogue row.
    pub fn apply_catalog_item(
        &mut self,
        item_id: &str,
        label: &str,
        item_type: &str,
        rarity: Option<&str>,
        rarity_color: Option<&str>,
        resolved_at: i64,
    ) -> bool {
        let key = item_id.to_ascii_lowercase();
        let entry = self.catalog.entry(key.clone()).or_default();
        if entry.item_id.is_empty() {
            entry.item_id = item_id.to_string();
        }
        // The first type seen for an id is sticky. Both the feed and item-info
        // report it, and they agree in practice — but resolution happens in
        // small batches, so letting a late lookup rewrite the type would move a
        // row between categories (and change the per-type totals) merely
        // because its name arrived.
        let next_type = if entry.item_type.is_empty() {
            item_type.to_string()
        } else {
            entry.item_type.clone()
        };
        let changed = entry.label != label
            || entry.item_type != next_type
            || entry.rarity.as_deref() != rarity
            || entry.rarity_color.as_deref() != rarity_color
            || entry.resolved_at.is_none();
        entry.label = label.to_string();
        entry.item_type = next_type;
        entry.rarity = rarity.map(str::to_string);
        entry.rarity_color = rarity_color.map(str::to_string);
        entry.resolved_at = Some(resolved_at);
        entry.attempts += 1;
        entry.last_attempt_at = Some(resolved_at);
        if changed {
            let mut row = entry.clone();
            row.item_id = key;
            if let Err(error) = self.write_catalog_item(&row) {
                self.report_write_error("could not save item catalog", &error);
            }
        }
        changed
    }

    /// Catalogue lookup by id, case-insensitive.
    pub fn catalog_entry(&self, item_id: &str) -> Option<&CatalogItem> {
        self.catalog.get(&item_id.to_ascii_lowercase())
    }

    /// Catalogue rows that still need an `item-info` lookup, newest first, at
    /// most `limit` of them.
    pub fn catalog_unresolved(&self, limit: usize) -> Vec<(String, String)> {
        let mut pending: Vec<(&String, &CatalogItem)> = self
            .catalog
            .iter()
            .filter(|(_, item)| item.needs_resolution())
            .collect();
        pending.sort_by_key(|(_, item)| std::cmp::Reverse(item.last_seen_at.unwrap_or(0)));
        pending
            .into_iter()
            .take(limit)
            .map(|(key, item)| {
                let item_type = if item.item_type.is_empty() {
                    "mutation_component".to_string()
                } else {
                    item.item_type.clone()
                };
                (key.clone(), item_type)
            })
            .collect()
    }

    pub(super) fn hydrate_catalog(&mut self) {
        let result = (|| -> Result<Vec<CatalogItem>, String> {
            let mut statement = self
                .db
                .prepare(
                    "SELECT item_id, item_type, label, rarity, rarity_color,
                            first_seen_at, last_seen_at, resolved_at, attempts, last_attempt_at
                     FROM item_catalog",
                )
                .map_err(|e| format!("could not prepare item catalog read: {e}"))?;
            let rows = statement
                .query_map([], |row| {
                    Ok(CatalogItem {
                        item_id: row.get(0)?,
                        item_type: row.get(1)?,
                        label: row.get(2)?,
                        rarity: row.get(3)?,
                        rarity_color: row.get(4)?,
                        first_seen_at: row.get(5)?,
                        last_seen_at: row.get(6)?,
                        resolved_at: row.get(7)?,
                        attempts: row.get(8)?,
                        last_attempt_at: row.get(9)?,
                    })
                })
                .map_err(|e| format!("could not read item catalog: {e}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("could not decode item catalog: {e}"))
        })();
        match result {
            Ok(rows) => {
                for row in rows {
                    self.catalog.insert(row.item_id.to_ascii_lowercase(), row);
                }
            }
            Err(error) => self.logger.warn("store", error),
        }
    }

    fn write_catalog_item(&self, item: &CatalogItem) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO item_catalog
                 (item_id, item_type, label, rarity, rarity_color,
                  first_seen_at, last_seen_at, resolved_at, attempts, last_attempt_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(item_id) DO UPDATE SET
               item_type = excluded.item_type,
               label = excluded.label,
               rarity = excluded.rarity,
               rarity_color = excluded.rarity_color,
               last_seen_at = excluded.last_seen_at,
               resolved_at = excluded.resolved_at,
               attempts = excluded.attempts,
               last_attempt_at = excluded.last_attempt_at",
            params![
                item.item_id,
                item.item_type,
                item.label,
                item.rarity,
                item.rarity_color,
                item.first_seen_at,
                item.last_seen_at,
                item.resolved_at,
                item.attempts,
                item.last_attempt_at
            ],
        )?;
        Ok(())
    }
}
