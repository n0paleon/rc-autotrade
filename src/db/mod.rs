//! SQLite bootstrap: schema, forward-only migrations, file permissions and the
//! data-directory lock.
//!
//! The database format is the compatibility boundary across builds: same
//! tables, same `user_version`, same WAL settings.

use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::logger::Logger;

mod lock;
mod schema;

#[cfg(test)]
mod tests;

use lock::secure;
use schema::migrate;

pub use lock::lock_data_dir;
pub use schema::{MODE_LIVE, MODE_OFF, MODE_SHADOW, SCHEMA_SQL};

pub const SCHEMA_VERSION: i64 = 8;
pub const DB_FILENAME: &str = "rollercoin.db";

/// Opens (and bootstraps) the SQLite database.
pub fn open(
    data_dir: &Path,
    logger: &Logger,
    metrics: Option<std::sync::Arc<crate::db_metrics::DbMetrics>>,
) -> Result<Connection, String> {
    fs::create_dir_all(data_dir)
        .map_err(|error| format!("could not create {}: {error}", data_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700));
    }

    let file = data_dir.join(DB_FILENAME);
    let mut conn = Connection::open(&file).map_err(|error| {
        format!(
            "could not open SQLite database at {}: {error}",
            file.display()
        )
    })?;

    conn.busy_timeout(Duration::from_millis(5_000))
        .map_err(|error| format!("could not set busy_timeout: {error}"))?;
    // `journal_mode` returns a row, so it cannot go through `pragma_update`.
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|error| format!("could not enable WAL: {error}"))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|error| format!("could not set synchronous: {error}"))?;
    // WAL auto-checkpoints every ~1000 pages, but the file itself is only
    // truncated when a checkpoint finishes. Capping the journal keeps the
    // on-disk footprint predictable without changing any stored row.
    conn.pragma_update(None, "journal_size_limit", 32 * 1024 * 1024_i64)
        .map_err(|error| format!("could not set journal_size_limit: {error}"))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|error| format!("could not enable foreign keys: {error}"))?;

    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);
    if version > SCHEMA_VERSION {
        return Err(format!(
            "database schema v{version} is newer than this build (v{SCHEMA_VERSION}); upgrade the worker instead of downgrading the data"
        ));
    }

    conn.execute_batch(SCHEMA_SQL)
        .map_err(|error| format!("could not apply schema: {error}"))?;
    migrate(&conn).map_err(|error| format!("could not migrate schema: {error}"))?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|error| format!("could not stamp user_version: {error}"))?;

    for suffix in ["", "-wal", "-shm"] {
        secure(Path::new(&format!("{}{suffix}", file.display())));
    }

    if let Some(metrics) = metrics {
        use rusqlite::trace::{TraceEvent, TraceEventCodes};
        conn.trace_v2(
            TraceEventCodes::SQLITE_TRACE_PROFILE,
            Some(move |event: TraceEvent<'_>| {
                if let TraceEvent::Profile(statement, duration) = event {
                    metrics.record(&statement.sql(), duration);
                }
            }),
        )
        .map_err(|error| format!("could not install SQLite metrics: {error}"))?;
    }

    logger.debug(
        "db",
        format!(
            "opened {} via rusqlite (schema v{SCHEMA_VERSION}, journal {mode})",
            file.display()
        ),
    );
    Ok(conn)
}

/// Checkpoints the WAL so a copied `.db` file is self-contained.
pub fn checkpoint_and_close(conn: &Connection) {
    let _: Result<String, _> =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0));
}
