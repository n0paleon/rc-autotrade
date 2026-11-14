use rusqlite::params;

use crate::clock::now_ms;
use crate::model::Target;

use super::Store;

pub(super) fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

pub(super) fn product_key(item_id: &str, label: &str) -> String {
    let id = item_id.trim().to_ascii_lowercase();
    if !id.is_empty() {
        return id;
    }
    let name = label.trim().to_ascii_lowercase();
    if !name.is_empty() {
        return format!("label:{name}");
    }
    "unknown".to_string()
}

impl Store {
    /// Rebuilds the id → position map. Called after any change to the target
    /// list (add, edit, delete, bulk replace, reload).
    pub(super) fn reindex_configs(&mut self) {
        self.config_index.clear();
        for (index, config) in self.state.configs.iter().enumerate() {
            self.config_index
                .insert(config.item_id.to_ascii_lowercase(), index);
        }
    }

    pub fn upsert_config(&mut self, mut target: Target, original_id: Option<&str>) -> Target {
        let key = original_id.unwrap_or(&target.item_id).to_ascii_lowercase();
        let now = now_ms();
        target.updated_at = Some(now);
        if let Some(index) = self
            .state
            .configs
            .iter()
            .position(|config| config.item_id.to_ascii_lowercase() == key)
        {
            target.created_at = self.state.configs[index].created_at.or(Some(now));
            self.state.configs[index] = target.clone();
        } else {
            target.created_at = Some(now);
            self.state.configs.push(target.clone());
        }
        self.reindex_configs();
        // Best effort: the in-memory row is already authoritative for this call.
        if let Err(error) = self.write_target(&target) {
            self.report_write_error("could not save target", &error);
        }
        if let Err(error) = self.set_meta("updated_at", &now.to_string()) {
            self.report_write_error("could not stamp updated_at", &error);
        }
        target
    }

    pub fn remove_config(&mut self, item_id: &str) -> bool {
        let key = item_id.to_ascii_lowercase();
        let before = self.state.configs.len();
        self.state
            .configs
            .retain(|config| config.item_id.to_ascii_lowercase() != key);
        if self.state.configs.len() == before {
            return false;
        }
        self.reindex_configs();
        if let Err(error) = self
            .db
            .execute("DELETE FROM targets WHERE item_id = ?1", params![item_id])
        {
            self.report_write_error("could not delete target", &error);
        }
        true
    }

    pub fn set_configs(&mut self, configs: Vec<Target>) -> Result<(), String> {
        let now = now_ms();
        let stamped: Vec<Target> = configs
            .into_iter()
            .map(|mut config| {
                if config.created_at.is_none() {
                    config.created_at = Some(now);
                }
                config.updated_at = Some(now);
                config
            })
            .collect();

        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|e| format!("could not start transaction: {e}"))?;
        for config in &stamped {
            tx.execute(
                "INSERT INTO targets
                 (item_id, label, item_type, rarity_color, target_price, desired_qty,
                  high_stock_threshold, min_acceptable_qty, allow_partial_order, active,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(item_id) DO UPDATE SET
                   label = excluded.label,
                   item_type = excluded.item_type,
                   rarity_color = excluded.rarity_color,
                   target_price = excluded.target_price,
                   desired_qty = excluded.desired_qty,
                   high_stock_threshold = excluded.high_stock_threshold,
                   min_acceptable_qty = excluded.min_acceptable_qty,
                   allow_partial_order = excluded.allow_partial_order,
                   active = excluded.active,
                   updated_at = excluded.updated_at",
                params![
                    config.item_id,
                    config.label,
                    config.item_type,
                    config.rarity_color,
                    config.target_price,
                    config.desired_qty,
                    config.high_stock_threshold,
                    config.min_acceptable_qty,
                    config.allow_partial_order as i64,
                    config.active as i64,
                    config.created_at,
                    config.updated_at
                ],
            )
            .map_err(|e| format!("could not write target: {e}"))?;
        }

        // Delete rows that are no longer in the list (cascades market_snapshot).
        let ids: Vec<String> = stamped.iter().map(|c| c.item_id.clone()).collect();
        if ids.is_empty() {
            tx.execute("DELETE FROM targets", [])
                .map_err(|e| format!("could not clear targets: {e}"))?;
        } else {
            let placeholders = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("DELETE FROM targets WHERE item_id NOT IN ({placeholders})");
            tx.execute(&sql, rusqlite::params_from_iter(ids.iter()))
                .map_err(|e| format!("could not prune targets: {e}"))?;
        }
        tx.commit()
            .map_err(|e| format!("could not commit targets: {e}"))?;

        self.state.configs = stamped;
        self.reindex_configs();
        Ok(())
    }

    pub fn find_config(&self, item_id: &str) -> Option<&Target> {
        // Hot path: the frame normaliser already lowercased the id, so this is
        // one hash lookup with no allocation. The identity check keeps a stale
        // index harmless; anything else (mixed case, unknown id) falls back to
        // the case-insensitive scan.
        if let Some(index) = self.config_index.get(item_id)
            && let Some(config) = self.state.configs.get(*index)
            && config.item_id.eq_ignore_ascii_case(item_id)
        {
            return Some(config);
        }
        self.state
            .configs
            .iter()
            .find(|config| config.item_id.eq_ignore_ascii_case(item_id))
    }

    /// Writes a resolved `Rarity Name` label back to the target. Returns
    /// whether anything actually changed.
    pub fn apply_item_info(
        &mut self,
        item_id: &str,
        label: &str,
        item_type: &str,
        rarity_color: Option<&str>,
    ) -> bool {
        let key = item_id.to_ascii_lowercase();
        let Some(config) = self
            .state
            .configs
            .iter_mut()
            .find(|config| config.item_id.to_ascii_lowercase() == key)
        else {
            return false;
        };
        let changed = config.label != label
            || config.item_type != item_type
            || config.rarity_color.as_deref() != rarity_color;
        if !changed {
            return false;
        }
        config.label = label.to_string();
        config.item_type = item_type.to_string();
        config.rarity_color = rarity_color.map(str::to_string);
        config.updated_at = Some(now_ms());
        let stamped = config.clone();
        let item_id = stamped.item_id.clone();
        if let Err(error) = self.write_target(&stamped) {
            self.report_write_error("could not save item info", &error);
        }
        // The item monitor names products from the catalogue, not from the
        // target list, so a resolved target also updates its catalogue row.
        // This path only knows the colour, so any rarity the monitor already
        // resolved is kept instead of being blanked.
        let resolved_at = now_ms();
        let known = self.catalog_entry(&item_id).cloned().unwrap_or_default();
        self.apply_catalog_item(
            &item_id,
            label,
            item_type,
            known.rarity.as_deref(),
            rarity_color.or(known.rarity_color.as_deref()),
            resolved_at,
        );
        true
    }

    /// The socket knows the real item type (`miner`, `rack`, ...); the engine
    /// corrects configs that were added before item-info could infer it.
    pub fn update_item_type(&mut self, item_id: &str, item_type: &str) {
        let key = item_id.to_ascii_lowercase();
        let Some(config) = self
            .state
            .configs
            .iter_mut()
            .find(|config| config.item_id.to_ascii_lowercase() == key)
        else {
            return;
        };
        if config.item_type == item_type {
            return;
        }
        config.item_type = item_type.to_string();
        config.updated_at = Some(now_ms());
        let stamped = config.clone();
        if let Err(error) = self.write_target(&stamped) {
            self.report_write_error("could not save target item type", &error);
        }
    }

    pub fn config_ids(&self) -> Vec<String> {
        self.state
            .configs
            .iter()
            .map(|c| c.item_id.clone())
            .collect()
    }

    fn write_target(&self, config: &Target) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO targets
             (item_id, label, item_type, rarity_color, target_price, desired_qty,
              high_stock_threshold, min_acceptable_qty, allow_partial_order, active,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(item_id) DO UPDATE SET
               label = excluded.label,
               item_type = excluded.item_type,
               rarity_color = excluded.rarity_color,
               target_price = excluded.target_price,
               desired_qty = excluded.desired_qty,
               high_stock_threshold = excluded.high_stock_threshold,
               min_acceptable_qty = excluded.min_acceptable_qty,
               allow_partial_order = excluded.allow_partial_order,
               active = excluded.active,
               updated_at = excluded.updated_at",
            params![
                config.item_id,
                config.label,
                config.item_type,
                config.rarity_color,
                config.target_price,
                config.desired_qty,
                config.high_stock_threshold,
                config.min_acceptable_qty,
                config.allow_partial_order as i64,
                config.active as i64,
                config.created_at,
                config.updated_at
            ],
        )?;
        Ok(())
    }
}
