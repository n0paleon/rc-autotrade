//! Running blocking work without stalling the async runtime.
//!
//! The worker keeps SQLite behind a `std::sync::Mutex`, so a handler that holds
//! it runs on a reactor thread. Frame-sized work (a snapshot upsert, a price
//! point) is microseconds and does not care. Backup, restore and retention
//! prunes are different: `VACUUM INTO` or a large `DELETE` can take hundreds of
//! milliseconds and would park a tokio worker.
//!
//! `block_in_place` hands the current thread to the scheduler for the duration
//! of the closure, so other tasks keep running on the remaining workers while
//! the guard stays exactly where it is (nothing is moved between threads).
//! It is only legal on the multi-threaded runtime, so current-thread runtimes
//! (unit and HTTP tests) run the closure inline instead.

/// Runs `work`, yielding the reactor thread on a multi-threaded runtime.
pub fn run<R>(work: impl FnOnce() -> R) -> R {
    let multi_thread = tokio::runtime::Handle::try_current()
        .map(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    if multi_thread {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `#[tokio::test]` uses the current-thread runtime, where `block_in_place`
    /// would panic. The helper must fall back to an inline call.
    #[tokio::test]
    async fn runs_inline_on_a_current_thread_runtime() {
        assert_eq!(run(|| 40 + 2), 42);
    }
}
