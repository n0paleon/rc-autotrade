use rusqlite::params;

use crate::clock::now_ms;
use crate::validation;

use super::Store;

impl Store {
    pub fn patch_settings(&mut self, patch: validation::SettingsPatch) -> Result<(), String> {
        let mut settings = self.state.settings;
        if let Some(value) = patch.delay_per_order {
            settings.delay_per_order = value;
        }
        if let Some(value) = patch.max_orders_per_minute {
            settings.max_orders_per_minute = value;
        }
        if let Some(value) = patch.mode {
            settings.mode = value;
        }
        if let Some(value) = patch.auto_refresh {
            settings.auto_refresh = value;
        }
        if let Some(value) = patch.total_price_markup_pct {
            settings.total_price_markup_pct = value;
        }
        self.state.settings = settings;
        self.write_settings()
            .map_err(|e| format!("could not persist settings: {e}"))
    }

    // ── persistence ─────────────────────────────────────────────────────────

    fn write_settings(&self) -> rusqlite::Result<()> {
        let settings = self.state.settings;
        let (dry_run, paused) = settings.legacy_flags();
        self.db.execute(
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
        )?;
        Ok(())
    }

    pub(super) fn write_stats(&self) -> rusqlite::Result<()> {
        let stats = self.state.stats;
        self.db.execute(
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
        )?;
        Ok(())
    }

    /// Persists only the counters the order path mutates.
    ///
    /// `flush()` also rewrites every target row, so a single order used to cost
    /// one upsert per configured item even though the target list only changes
    /// through the API that already writes it.
    pub fn flush_stats(&mut self) -> Result<(), String> {
        self.state.updated_at = now_ms();
        self.write_stats()
            .map_err(|e| format!("could not write stats: {e}"))
    }
}
