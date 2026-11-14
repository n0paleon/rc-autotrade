use rusqlite::params;

use crate::clock::now_ms;
use crate::constants::EVENT_RETENTION_DAYS;

use super::Store;
use super::state::{MARKET_EVENT_SCOPE_KEY, event_bucket_at};

impl Store {
    // ── marketplace event activity ──────────────────────────────────────────

    /// Counts one marketplace command into its 5-minute bucket. Stays in
    /// memory: the flusher writes the batch, so the frame path never waits on
    /// SQLite.
    pub fn note_market_event(&mut self, at: i64) {
        let bucket = event_bucket_at(at);
        *self.event_points.entry(bucket).or_insert(0) += 1;
        self.event_dirty.insert(bucket);
    }

    /// Buckets at or after `since`, oldest first, including the live one.
    pub fn event_series(&self, since: i64) -> Vec<(i64, i64)> {
        self.event_points
            .range(since..)
            .map(|(bucket, count)| (*bucket, *count))
            .collect()
    }

    /// First bucket the marketplace-only counter owns; older rows have no
    /// command breakdown and must not be served as market activity.
    pub fn event_scope_at(&self) -> i64 {
        self.event_scope_at
    }

    /// Writes every dirty bucket in one transaction, then drops expiry.
    pub fn flush_event_points(&mut self) -> Result<u64, String> {
        let pending: Vec<(i64, i64)> = self
            .event_dirty
            .iter()
            .map(|bucket| (*bucket, self.event_points.get(bucket).copied().unwrap_or(0)))
            .collect();

        if !pending.is_empty() {
            let tx = self
                .db
                .unchecked_transaction()
                .map_err(|e| format!("could not start event flush: {e}"))?;
            {
                let mut statement = tx
                    .prepare_cached(
                        "INSERT INTO event_points (bucket_at, events) VALUES (?1, ?2)
                         ON CONFLICT(bucket_at) DO UPDATE SET events = excluded.events",
                    )
                    .map_err(|e| format!("could not cache event point statement: {e}"))?;
                for (bucket, count) in &pending {
                    statement
                        .execute(params![bucket, count])
                        .map_err(|e| format!("could not record event point: {e}"))?;
                }
            }
            tx.commit()
                .map_err(|e| format!("could not commit event points: {e}"))?;
            self.event_dirty.clear();
        }

        self.prune_event_points()?;
        Ok(pending.len() as u64)
    }

    pub(super) fn hydrate_events(&mut self) {
        let now = now_ms();
        let stored_scope = match self.get_meta(MARKET_EVENT_SCOPE_KEY) {
            Ok(Some(raw)) => raw.parse::<i64>().ok().filter(|at| *at > 0),
            Ok(None) => None,
            Err(error) => {
                self.logger.warn("store", error);
                None
            }
        };
        self.event_scope_at = match stored_scope {
            Some(at) => at,
            None => {
                // First boot after the scope narrowed. The live bucket may
                // already hold all-frame counts from before the upgrade, so
                // drop just that bucket and let the corrected counter refill
                // it. Older buckets stay in SQLite for retention but the API
                // hides them.
                let bucket = event_bucket_at(now);
                if let Err(error) = self.db.execute(
                    "DELETE FROM event_points WHERE bucket_at = ?1",
                    params![bucket],
                ) {
                    self.logger.warn(
                        "store",
                        format!("could not clear the pre-scope event bucket: {error}"),
                    );
                }
                if let Err(error) = self.set_meta(MARKET_EVENT_SCOPE_KEY, &bucket.to_string()) {
                    self.logger.warn("store", error);
                }
                bucket
            }
        };
        let since = now - EVENT_RETENTION_DAYS * 86_400_000;
        let mut statement = match self.db.prepare(
            "SELECT bucket_at, events FROM event_points WHERE bucket_at >= ?1 ORDER BY bucket_at",
        ) {
            Ok(statement) => statement,
            Err(error) => {
                self.logger
                    .warn("store", format!("could not read event history: {error}"));
                return;
            }
        };
        let rows = statement.query_map(params![since], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        });
        let rows = match rows {
            Ok(rows) => rows,
            Err(error) => {
                self.logger
                    .warn("store", format!("could not read event history: {error}"));
                return;
            }
        };
        for (bucket, count) in rows.flatten() {
            self.event_points.insert(bucket, count);
        }
    }

    fn prune_event_points(&mut self) -> Result<(), String> {
        let now = now_ms();
        let cutoff = now - EVENT_RETENTION_DAYS * 86_400_000;
        let expired = self
            .event_points
            .keys()
            .next()
            .map(|bucket| *bucket < cutoff)
            .unwrap_or(false);
        // A week of 5-minute buckets is ~2 300 rows; hourly is plenty. An
        // expiry that appears between sweeps (a clock jump, restored data)
        // still forces the sweep immediately.
        if !expired && now - self.event_pruned_at < 3_600_000 {
            return Ok(());
        }
        self.event_pruned_at = now;
        self.event_points = self.event_points.split_off(&cutoff);
        self.event_dirty.retain(|bucket| *bucket >= cutoff);
        self.db
            .execute(
                "DELETE FROM event_points WHERE bucket_at < ?1",
                params![cutoff],
            )
            .map(|_| ())
            .map_err(|e| format!("could not prune event history: {e}"))
    }
}
