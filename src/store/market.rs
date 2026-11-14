use rusqlite::params;

use crate::clock::now_ms;
use crate::constants::PRICE_RETENTION_DAYS;
use crate::model::{MarketSnapshot, PriceBand, PricePoint, PriceSeriesPoint};

use super::Store;

impl Store {
    // ── market cache ────────────────────────────────────────────────────────

    pub fn load_market_snapshots(&self) -> Vec<MarketSnapshot> {
        let mut statement = match self.db.prepare(
            "SELECT item_id, last_price, last_stock, allowed_price_min, allowed_price_max,
                    allowed_at, item_type, seen_at FROM market_snapshot",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map([], |row| {
            Ok(MarketSnapshot {
                item_id: row.get(0)?,
                last_price: row.get(1)?,
                last_stock: row.get(2)?,
                allowed_price_min: row.get(3)?,
                allowed_price_max: row.get(4)?,
                allowed_at: row.get(5)?,
                item_type: row.get(6)?,
                seen_at: row.get(7)?,
            })
        });
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn save_market_snapshot(&self, snapshot: &MarketSnapshot) -> Result<(), String> {
        // Hot path: the statement is cached so a snapshot write does not pay a
        // prepare per frame.
        let mut statement = self
            .db
            .prepare_cached(
                "INSERT INTO market_snapshot
                 (item_id, last_price, last_stock, allowed_price_min, allowed_price_max,
                  allowed_at, item_type, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(item_id) DO UPDATE SET
                   last_price = excluded.last_price,
                   last_stock = excluded.last_stock,
                   allowed_price_min = excluded.allowed_price_min,
                   allowed_price_max = excluded.allowed_price_max,
                   allowed_at = excluded.allowed_at,
                   item_type = excluded.item_type,
                   seen_at = excluded.seen_at",
            )
            .map_err(|e| format!("could not cache market snapshot statement: {e}"))?;
        statement
            .execute(params![
                snapshot.item_id,
                snapshot.last_price,
                snapshot.last_stock,
                snapshot.allowed_price_min,
                snapshot.allowed_price_max,
                snapshot.allowed_at,
                snapshot.item_type,
                snapshot.seen_at
            ])
            .map(|_| ())
            .map_err(|e| format!("could not save market snapshot: {e}"))
    }

    pub fn save_market_snapshots(&self, snapshots: &[MarketSnapshot]) -> Result<(), String> {
        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|e| format!("could not start transaction: {e}"))?;
        for snapshot in snapshots {
            tx.execute(
                "INSERT INTO market_snapshot
                 (item_id, last_price, last_stock, allowed_price_min, allowed_price_max,
                  allowed_at, item_type, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(item_id) DO UPDATE SET
                   last_price = excluded.last_price,
                   last_stock = excluded.last_stock,
                   allowed_price_min = excluded.allowed_price_min,
                   allowed_price_max = excluded.allowed_price_max,
                   allowed_at = excluded.allowed_at,
                   item_type = excluded.item_type,
                   seen_at = excluded.seen_at",
                params![
                    snapshot.item_id,
                    snapshot.last_price,
                    snapshot.last_stock,
                    snapshot.allowed_price_min,
                    snapshot.allowed_price_max,
                    snapshot.allowed_at,
                    snapshot.item_type,
                    snapshot.seen_at
                ],
            )
            .map_err(|e| format!("could not save market snapshot: {e}"))?;
        }
        tx.commit()
            .map_err(|e| format!("could not commit market snapshots: {e}"))
    }

    pub fn load_price_history(&self, item_id: &str, limit: usize, since: i64) -> Vec<PricePoint> {
        let mut statement = match self.db.prepare(
            "SELECT item_id, bucket_at, price, low_price, high_price,
                    raw_low_price, raw_high_price, fill_price, fill_qty, fill_approx
             FROM price_points
             WHERE item_id = ?1 AND (?2 = 0 OR bucket_at >= ?2)
             ORDER BY bucket_at DESC LIMIT ?3",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map(params![item_id, since, limit as i64], |row| {
            Ok(PricePoint {
                item_id: row.get(0)?,
                bucket_at: row.get(1)?,
                price: row.get(2)?,
                low_price: row.get(3)?,
                high_price: row.get(4)?,
                raw_low_price: row.get(5)?,
                raw_high_price: row.get(6)?,
                fill_price: row.get(7)?,
                fill_qty: row.get(8)?,
                fill_approx: row.get::<_, i64>(9)? != 0,
            })
        });
        match rows {
            Ok(rows) => {
                let mut points: Vec<PricePoint> = rows.filter_map(Result::ok).collect();
                points.reverse();
                points
            }
            Err(_) => Vec::new(),
        }
    }

    /// Raw 1-minute closes (plus their held and raw low/high bands) for one
    /// item, oldest first. Unlike `load_price_history` this is not capped by
    /// the live chart window, so the 3d/7d periods can reach back into the
    /// retained week.
    pub fn price_series(&self, item_id: &str, since: i64) -> Vec<PriceSeriesPoint> {
        let mut statement = match self.db.prepare(
            "SELECT bucket_at, price,
                    COALESCE(low_price, price), COALESCE(high_price, price),
                    COALESCE(raw_low_price, low_price, price),
                    COALESCE(raw_high_price, high_price, price)
             FROM price_points
             WHERE item_id = ?1 AND bucket_at >= ?2
             ORDER BY bucket_at ASC",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map(params![item_id, since], |row| {
            Ok(PriceSeriesPoint {
                bucket_at: row.get(0)?,
                price: row.get(1)?,
                low: row.get(2)?,
                high: row.get(3)?,
                raw_low: row.get(4)?,
                raw_high: row.get(5)?,
            })
        });
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn record_price_point(
        &self,
        item_id: &str,
        bucket_at: i64,
        price: f64,
        band: PriceBand,
        fill: Option<(f64, f64, bool)>,
    ) -> Result<(), String> {
        let (fill_price, fill_qty, fill_approx) = match fill {
            Some((price, qty, approx)) => (Some(price), Some(qty), approx),
            None => (None, None, false),
        };
        let mut statement = self
            .db
            .prepare_cached(
                "INSERT INTO price_points
                 (item_id, bucket_at, price, low_price, high_price,
                  raw_low_price, raw_high_price, fill_price, fill_qty, fill_approx)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(item_id, bucket_at) DO UPDATE SET
                   price = excluded.price,
                   low_price = excluded.low_price,
                   high_price = excluded.high_price,
                   raw_low_price = excluded.raw_low_price,
                   raw_high_price = excluded.raw_high_price,
                   fill_price = COALESCE(excluded.fill_price, price_points.fill_price),
                   fill_qty = COALESCE(excluded.fill_qty, price_points.fill_qty),
                   fill_approx = excluded.fill_approx",
            )
            .map_err(|e| format!("could not cache price point statement: {e}"))?;
        statement
            .execute(params![
                item_id,
                bucket_at,
                price,
                band.low,
                band.high,
                band.raw_low,
                band.raw_high,
                fill_price,
                fill_qty,
                fill_approx as i64
            ])
            .map(|_| ())
            .map_err(|e| format!("could not record price point: {e}"))
    }

    /// Writes a batch of sampled closes in one transaction. A sample never
    /// carries a fill, so re-sampling a bucket that already holds a fill keeps
    /// the fill columns intact.
    pub fn record_price_samples(
        &self,
        samples: &[(&str, i64, f64, f64, f64, f64, f64)],
    ) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        let tx = self
            .db
            .unchecked_transaction()
            .map_err(|e| format!("could not start transaction: {e}"))?;
        {
            let mut statement = tx
                .prepare_cached(
                    "INSERT INTO price_points
                     (item_id, bucket_at, price, low_price, high_price,
                      raw_low_price, raw_high_price, fill_price, fill_qty, fill_approx)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, 0)
                     ON CONFLICT(item_id, bucket_at) DO UPDATE SET
                       price = excluded.price,
                       low_price = excluded.low_price,
                       high_price = excluded.high_price,
                       raw_low_price = excluded.raw_low_price,
                       raw_high_price = excluded.raw_high_price",
                )
                .map_err(|e| format!("could not cache price sample statement: {e}"))?;
            for (item_id, bucket_at, price, low, high, raw_low, raw_high) in samples {
                statement
                    .execute(params![
                        item_id, bucket_at, price, low, high, raw_low, raw_high
                    ])
                    .map_err(|e| format!("could not record price sample: {e}"))?;
            }
        }
        tx.commit()
            .map_err(|e| format!("could not commit price samples: {e}"))
    }

    pub fn prune_price_history(&self) -> Result<u64, String> {
        let cutoff = now_ms() - PRICE_RETENTION_DAYS * 86_400_000;
        self.db
            .execute(
                "DELETE FROM price_points WHERE bucket_at < ?1",
                params![cutoff],
            )
            .map(|n| n as u64)
            .map_err(|e| format!("could not prune price history: {e}"))
    }

    /// Non-blocking WAL checkpoint. `PASSIVE` never waits for readers and never
    /// truncates the log from under a writer, so it is safe to run on a timer.
    pub fn checkpoint_wal(&self) -> Result<(), String> {
        self.db
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_row| Ok(()))
            .map_err(|e| format!("could not checkpoint WAL: {e}"))
    }
}
