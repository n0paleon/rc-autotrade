//! Live item subscriptions for the control socket.
//!
//! The market feed sees every RLT item, but the engine only broadcasts full
//! ticks for configured targets. A target form needs the same freshness while
//! the operator is evaluating an arbitrary id, so a deck can watch one item and
//! receive a compact `quote` frame until it unwatches.
//!
//! Subscriptions are reference-counted: two control sockets may watch the same
//! item, and closing one must not stop the stream for the other. The last quote
//! timestamp is tracked per item so a busy ladder cannot flood the socket, and
//! it is dropped together with the final watcher.

use std::collections::HashMap;

use crate::constants::MARKET_BROADCAST_INTERVAL_MS;

#[derive(Debug, Default)]
pub(super) struct WatchRegistry {
    counts: HashMap<String, usize>,
    last_quote_at: HashMap<String, i64>,
}

impl WatchRegistry {
    /// Adds one watcher for `item_id`; ids are matched case-insensitively.
    pub(super) fn watch(&mut self, item_id: &str) {
        let key = item_id.to_ascii_lowercase();
        if key.is_empty() {
            return;
        }
        *self.counts.entry(key).or_insert(0) += 1;
    }

    /// Removes one watcher, releasing the quote throttle with the last one.
    pub(super) fn unwatch(&mut self, item_id: &str) {
        let key = item_id.to_ascii_lowercase();
        match self.counts.get_mut(&key) {
            Some(count) if *count > 1 => *count -= 1,
            Some(_) => {
                self.counts.remove(&key);
                self.last_quote_at.remove(&key);
            }
            None => {}
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    pub(super) fn contains(&self, item_id: &str) -> bool {
        self.counts.contains_key(&item_id.to_ascii_lowercase())
    }

    /// True when a quote may be sent for this item now.
    ///
    /// The first frame always passes. Later frames are limited to the same
    /// cadence as coalesced market ticks; stamps are only stored when the
    /// frame is actually due, so a burst of rejected frames cannot push the
    /// next accepted one further into the future.
    pub(super) fn quote_is_due(&mut self, item_id: &str, at: i64) -> bool {
        let key = item_id.to_ascii_lowercase();
        let due = self
            .last_quote_at
            .get(&key)
            .is_none_or(|last| at.saturating_sub(*last) >= MARKET_BROADCAST_INTERVAL_MS as i64);
        if due {
            self.last_quote_at.insert(key, at);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_are_reference_counted() {
        let mut watch = WatchRegistry::default();
        assert!(watch.is_empty());

        watch.watch("ABC123");
        watch.watch("abc123");
        assert!(watch.contains("AbC123"));

        watch.unwatch("abc123");
        assert!(watch.contains("ABC123"));

        watch.unwatch("abc123");
        assert!(watch.is_empty());
        assert!(!watch.contains("abc123"));
    }

    #[test]
    fn unwatching_an_unknown_item_does_not_underflow() {
        let mut watch = WatchRegistry::default();
        watch.unwatch("missing");
        assert!(watch.is_empty());
    }

    #[test]
    fn quotes_are_throttled_per_item() {
        let mut watch = WatchRegistry::default();
        watch.watch("abc123");

        assert!(watch.quote_is_due("abc123", 1_000));
        assert!(!watch.quote_is_due("abc123", 1_000 + 249));
        assert!(watch.quote_is_due("abc123", 1_000 + 250));

        // Another item has its own slot.
        assert!(watch.quote_is_due("other", 1_000 + 250));
    }

    #[test]
    fn releasing_the_last_watcher_resets_the_throttle() {
        let mut watch = WatchRegistry::default();
        watch.watch("abc123");
        assert!(watch.quote_is_due("abc123", 1_000));
        watch.unwatch("abc123");

        watch.watch("abc123");
        assert!(watch.quote_is_due("abc123", 1_001));
    }
}
