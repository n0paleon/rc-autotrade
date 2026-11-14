//! Wall clock shared by every subsystem.
//!
//! The engine takes a [`Clock`] so tests can drive time, while the rest of the
//! worker reads [`now_ms`] directly.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Injectable epoch-millisecond clock.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Current epoch milliseconds. Falls back to `0` before the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// The real clock, as an injectable [`Clock`].
pub fn wall_clock() -> Clock {
    Arc::new(now_ms)
}
