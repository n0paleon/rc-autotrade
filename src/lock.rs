//! Poison-tolerant mutex locking.
//!
//! Every shared handle in the worker is a plain `std::sync::Mutex`. A poisoned
//! mutex only means a previous holder panicked while the guard was alive; the
//! data itself is still structurally valid for this process, and refusing to
//! lock would turn one panic into a permanently stuck worker. This helper keeps
//! that recovery policy in one place instead of repeating the `unwrap_or_else`
//! fallback at every call site.

use std::sync::{Mutex, MutexGuard};

/// Locks `mutex`, recovering the guard when the previous holder panicked.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
