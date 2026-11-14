//! SQLite statement metrics.
//!
//! Instead of wrapping every call site, this uses SQLite's own trace interface
//! (`sqlite3_trace_v2` with `SQLITE_TRACE_PROFILE`) through rusqlite's
//! `trace_v2`. Every executed statement reports its SQL text and duration, so
//! the counters describe the real workload without touching the store.
//!
//! What is not available from the profile events: failed prepares and
//! `SQLITE_BUSY` retries. Those stay at zero until the store starts reporting
//! them explicitly (`mark_error`/`mark_busy` exist for that).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

use crate::clock::now_ms;
use crate::lock::lock;
use crate::num::round_to;

#[cfg(test)]
mod tests;

pub const SLOW_MS: f64 = 20.0;
pub const P95_WARN_MS: f64 = 25.0;
pub const P95_CRITICAL_MS: f64 = 100.0;
pub const WAL_WARN_BYTES: f64 = 64.0 * 1024.0 * 1024.0;
pub const WAL_CRITICAL_BYTES: f64 = 256.0 * 1024.0 * 1024.0;
pub const DEFAULT_HISTORY: usize = 120;
const WINDOW_MS: i64 = 60_000;
const DEFAULT_BUFFER: usize = 2_000;

fn round(value: f64, digits: i32) -> f64 {
    if value.is_finite() {
        round_to(value, digits)
    } else {
        0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
    Pragma,
    Schema,
    Other,
    Transaction,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Read => "read",
            Kind::Write => "write",
            Kind::Pragma => "pragma",
            Kind::Schema => "schema",
            Kind::Other => "other",
            Kind::Transaction => "transaction",
        }
    }
}

/// First-keyword classification, same buckets as the JS `statementKind`.
/// Comparison is case-insensitive without allocating: this runs on every
/// traced statement, so a `to_ascii_lowercase()` per call was pure churn.
pub fn classify(sql: &str) -> Kind {
    let keyword = sql
        .trim_start()
        .split(|ch: char| !ch.is_ascii_alphabetic())
        .find(|part| !part.is_empty())
        .unwrap_or("");
    let is = |candidate: &str| keyword.eq_ignore_ascii_case(candidate);
    if is("select") || is("with") || is("values") || is("explain") {
        Kind::Read
    } else if is("insert") || is("update") || is("delete") || is("replace") {
        Kind::Write
    } else if is("pragma") {
        Kind::Pragma
    } else if is("create") || is("alter") || is("drop") || is("vacuum") || is("reindex") {
        Kind::Schema
    } else if is("begin")
        || is("commit")
        || is("rollback")
        || is("savepoint")
        || is("release")
        || is("end")
    {
        Kind::Transaction
    } else {
        Kind::Other
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Totals {
    statements: i64,
    read: i64,
    write: i64,
    pragma: i64,
    schema: i64,
    other: i64,
    errors: i64,
    busy: i64,
    slow: i64,
    total_ms: f64,
    max_ms: f64,
    last_ms: Option<f64>,
    last_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default)]
struct TransactionTotals {
    count: i64,
    errors: i64,
    total_ms: f64,
    max_ms: f64,
    last_ms: Option<f64>,
    last_at: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    at: i64,
    ms: f64,
}

#[derive(Debug, Clone)]
struct SlowQuery {
    at: i64,
    ms: f64,
    sql: String,
    kind: Kind,
}

pub struct DbMetrics {
    data_dir: PathBuf,
    totals: Mutex<Totals>,
    transactions: Mutex<TransactionTotals>,
    samples: Mutex<VecDeque<Sample>>,
    slow_queries: Mutex<VecDeque<SlowQuery>>,
    history: Mutex<VecDeque<Value>>,
    previous: Mutex<(Option<i64>, Option<i64>)>,
    history_limit: usize,
    buffer_limit: usize,
}

impl std::fmt::Debug for DbMetrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let totals = lock(&self.totals);
        formatter
            .debug_struct("DbMetrics")
            .field("statements", &totals.statements)
            .field("data_dir", &self.data_dir)
            .finish()
    }
}

impl DbMetrics {
    pub fn new(data_dir: impl Into<PathBuf>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            data_dir: data_dir.into(),
            totals: Mutex::new(Totals::default()),
            transactions: Mutex::new(TransactionTotals::default()),
            samples: Mutex::new(VecDeque::with_capacity(DEFAULT_BUFFER)),
            slow_queries: Mutex::new(VecDeque::with_capacity(32)),
            history: Mutex::new(VecDeque::with_capacity(DEFAULT_HISTORY)),
            previous: Mutex::new((None, None)),
            history_limit: DEFAULT_HISTORY,
            buffer_limit: DEFAULT_BUFFER,
        })
    }

    /// Records one finished statement. Called from the SQLite profile callback.
    pub fn record(&self, sql: &str, duration: Duration) {
        let ms = duration.as_secs_f64() * 1000.0;
        let at = now_ms();
        let kind = classify(sql);

        if kind == Kind::Transaction {
            let mut transactions = lock(&self.transactions);
            transactions.count += 1;
            transactions.total_ms += ms;
            transactions.max_ms = transactions.max_ms.max(ms);
            transactions.last_ms = Some(round(ms, 3));
            transactions.last_at = Some(at);
            return;
        }

        {
            let mut totals = lock(&self.totals);
            totals.statements += 1;
            match kind {
                Kind::Read => totals.read += 1,
                Kind::Write => totals.write += 1,
                Kind::Pragma => totals.pragma += 1,
                Kind::Schema => totals.schema += 1,
                _ => totals.other += 1,
            }
            totals.total_ms += ms;
            totals.max_ms = totals.max_ms.max(ms);
            totals.last_ms = Some(round(ms, 3));
            totals.last_at = Some(at);
            if ms >= SLOW_MS {
                totals.slow += 1;
            }
        }

        if ms >= SLOW_MS {
            let mut slow = lock(&self.slow_queries);
            slow.push_back(SlowQuery {
                at,
                ms: round(ms, 3),
                sql: sql.chars().take(120).collect(),
                kind,
            });
            while slow.len() > 50 {
                slow.pop_front();
            }
        }

        {
            let mut samples = lock(&self.samples);
            samples.push_back(Sample { at, ms });
            while samples.len() > self.buffer_limit {
                samples.pop_front();
            }
            while samples
                .front()
                .map(|sample| sample.at < at - WINDOW_MS - 60_000)
                .unwrap_or(false)
            {
                samples.pop_front();
            }
        }
    }

    /// Explicit failure reporting (profile events cannot see failed prepares).
    pub fn mark_error(&self) {
        let mut totals = lock(&self.totals);
        totals.errors += 1;
    }

    pub fn mark_busy(&self) {
        let mut totals = lock(&self.totals);
        totals.busy += 1;
    }

    fn file_size(&self, suffix: &str) -> i64 {
        let path = if suffix.is_empty() {
            self.data_dir.join("rollercoin.db")
        } else {
            self.data_dir.join(format!("rollercoin.db{suffix}"))
        };
        std::fs::metadata(path)
            .map(|meta| meta.len() as i64)
            .unwrap_or(0)
    }

    fn window(&self, at: i64) -> Value {
        let samples = lock(&self.samples);
        let recent: Vec<&Sample> = samples
            .iter()
            .filter(|sample| sample.at >= at - WINDOW_MS)
            .collect();
        if recent.is_empty() {
            return json!({
                "count": 0, "avgMs": 0.0, "p95Ms": 0.0, "maxMs": 0.0,
                "errors": 0, "busy": 0, "slow": 0,
            });
        }
        let mut values: Vec<f64> = recent.iter().map(|sample| sample.ms).collect();
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let count = values.len();
        let at_index = |q: f64| values[((q * count as f64) as usize).min(count - 1)];
        let sum: f64 = values.iter().sum();
        let slow = values.iter().filter(|ms| **ms >= SLOW_MS).count() as i64;
        json!({
            "count": count,
            "avgMs": round(sum / count as f64, 3),
            "p95Ms": round(at_index(0.95), 3),
            "maxMs": round(values[count - 1], 3),
            "errors": 0,
            "busy": 0,
            "slow": slow,
        })
    }

    /// Samples the view: rates, file sizes, window and history.
    pub fn sample(&self) -> Value {
        let at = now_ms();
        let (previous_at, previous_statements) = {
            let previous = lock(&self.previous);
            *previous
        };
        let statements = {
            let totals = lock(&self.totals);
            totals.statements
        };
        let elapsed = previous_at.map(|previous| at - previous).unwrap_or(0);
        let delta = previous_statements
            .map(|previous| statements - previous)
            .unwrap_or(0);
        let rate = if elapsed > 0 {
            round(delta as f64 / elapsed as f64 * 1000.0, 1)
        } else {
            0.0
        };
        {
            let mut previous = lock(&self.previous);
            *previous = (Some(at), Some(statements));
        }

        let files = json!({
            "db": self.file_size(""),
            "wal": self.file_size("-wal"),
            "shm": self.file_size("-shm"),
        });
        let total = files["db"].as_i64().unwrap_or(0)
            + files["wal"].as_i64().unwrap_or(0)
            + files["shm"].as_i64().unwrap_or(0);

        let queries = {
            let totals = *lock(&self.totals);
            json!({
                "statements": totals.statements,
                "read": totals.read,
                "write": totals.write,
                "pragma": totals.pragma,
                "schema": totals.schema,
                "other": totals.other,
                "errors": totals.errors,
                "busy": totals.busy,
                "slow": totals.slow,
                "totalMs": round(totals.total_ms, 3),
                "maxMs": round(totals.max_ms, 3),
                "lastMs": totals.last_ms,
                "lastAt": totals.last_at,
                "avgMs": if totals.statements > 0 {
                    round(totals.total_ms / totals.statements as f64, 3)
                } else { 0.0 },
                "rate": rate,
            })
        };

        let transactions = {
            let tx = *lock(&self.transactions);
            json!({
                "count": tx.count,
                "errors": tx.errors,
                "totalMs": round(tx.total_ms, 3),
                "maxMs": round(tx.max_ms, 3),
                "lastMs": tx.last_ms,
                "lastAt": tx.last_at,
            })
        };

        let window = self.window(at);
        let recent_slow: Vec<Value> = {
            let slow = lock(&self.slow_queries);
            slow.iter()
                .rev()
                .take(10)
                .map(|item| {
                    json!({
                        "at": item.at,
                        "ms": item.ms,
                        "sql": item.sql,
                        "kind": item.kind.as_str(),
                    })
                })
                .collect()
        };

        let view = json!({
            "instrumented": true,
            "files": { "db": files["db"], "wal": files["wal"], "shm": files["shm"], "total": total },
            "storage": {
                "dbBytes": files["db"],
                "walBytes": files["wal"],
                "shmBytes": files["shm"],
                "totalBytes": total,
            },
            "sqlite": {
                "version": rusqlite::version(),
                "driver": "rusqlite",
            },
            "queries": queries,
            "transactions": transactions,
            "window": window,
            "recentSlow": recent_slow,
            "thresholds": {
                "slowMs": SLOW_MS,
                "p95WarnMs": P95_WARN_MS,
                "p95CriticalMs": P95_CRITICAL_MS,
                "walWarnBytes": WAL_WARN_BYTES,
                "walCriticalBytes": WAL_CRITICAL_BYTES,
            },
        });

        {
            let mut history = lock(&self.history);
            history.push_back(json!({
                "at": at,
                "qps": rate,
                "avgMs": view["window"]["avgMs"],
                "p95Ms": view["window"]["p95Ms"],
                "dbBytes": files["db"],
                "walBytes": files["wal"],
            }));
            while history.len() > self.history_limit {
                history.pop_front();
            }
            let mut out = view;
            out["history"] = Value::Array(history.iter().cloned().collect());
            out
        }
    }

    pub fn snapshot(&self) -> Value {
        let mut view = self.sample();
        view["sqlite"]["version"] = json!(rusqlite::version());
        view
    }

    pub fn reset(&self) {
        *lock(&self.totals) = Totals::default();
        *lock(&self.transactions) = TransactionTotals::default();
        lock(&self.samples).clear();
        lock(&self.slow_queries).clear();
        lock(&self.history).clear();
        *lock(&self.previous) = (None, None);
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}
