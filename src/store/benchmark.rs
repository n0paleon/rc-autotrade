use rusqlite::params;

use super::Store;

impl Store {
    /// Newest completed orders with their timing marks, for the benchmark
    /// window. Rows without a decodable `timings` object are skipped: they
    /// predate the latency debugger and have nothing to aggregate.
    pub fn benchmark_samples(&self, limit: usize) -> Vec<crate::benchmark::Sample> {
        let limit = limit.clamp(1, 1_000) as i64;
        let Ok(mut statement) = self.db.prepare(
            "SELECT at, status, timings FROM orders
             WHERE timings IS NOT NULL AND timings != ''
             ORDER BY at DESC, id DESC LIMIT ?1",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = statement.query_map(params![limit], |row| {
            let at: i64 = row.get(0)?;
            let status: String = row.get(1)?;
            let timings: Option<String> = row.get(2)?;
            Ok((at, status, timings))
        }) else {
            return Vec::new();
        };
        rows.flatten()
            .filter_map(|(at, status, text)| {
                let timings = crate::timings::decode(text.as_deref())?;
                Some(crate::benchmark::Sample {
                    at,
                    status,
                    timings,
                })
            })
            .collect()
    }

    /// Completed orders with timing marks inside a time range, newest first.
    /// Used by the bucketed history chart; the range is inclusive.
    pub fn benchmark_samples_between(
        &self,
        since: i64,
        until: i64,
        limit: usize,
    ) -> Vec<crate::benchmark::Sample> {
        let limit = limit.clamp(1, crate::benchmark::HISTORY_MAX_SAMPLES) as i64;
        let Ok(mut statement) = self.db.prepare(
            "SELECT at, status, timings FROM orders
             WHERE at >= ?1 AND at <= ?2
               AND timings IS NOT NULL AND timings != ''
             ORDER BY at DESC, id DESC LIMIT ?3",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = statement.query_map(params![since, until, limit], |row| {
            let at: i64 = row.get(0)?;
            let status: String = row.get(1)?;
            let timings: Option<String> = row.get(2)?;
            Ok((at, status, timings))
        }) else {
            return Vec::new();
        };
        rows.flatten()
            .filter_map(|(at, status, text)| {
                let timings = crate::timings::decode(text.as_deref())?;
                Some(crate::benchmark::Sample {
                    at,
                    status,
                    timings,
                })
            })
            .collect()
    }
}
