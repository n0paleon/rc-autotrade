use serde::Serialize;

use crate::constants::PRICE_SCALE;

/// How long the engine distrusts the stock in a market frame after it has
/// already booked a fill.
///
/// Upstream frames are unversioned (no `updatedAt`, no sequence number), so a
/// frame that was generated just before our purchase was applied can arrive
/// just after it — and it still shows the pre-purchase depth. Trusting it would
/// resurrect stock we have already bought and could trigger a second order for
/// goods that are gone. For this window the decision stock is therefore floored
/// at what we know locally; afterwards the frame wins again, so a genuine
/// restock is only ever missed for at most this long.
pub(super) const RECONCILE_WINDOW_MS: i64 = 1_500;

/// Micro-RLT key used to group equal prices without comparing floats.
pub(super) fn price_key(price: f64) -> i64 {
    (price * PRICE_SCALE).round() as i64
}

/// Locally committed book state for one target while an order is outstanding or
/// its fill is being reconciled against the live feed.
///
/// The engine cannot tell a stale frame (generated before upstream applied our
/// order) from a genuine restock: the frames carry no version at all. So it
/// keeps a floor for a short window instead of guessing: frames may not raise
/// the decision stock past [`Reconcile::floor`] until the window closes or one
/// of the proofs below shows the frame has caught up with us.
///
/// The ledger opens when the order is **submitted**, not when its fill lands,
/// so the units a request commits are never offered to a second order in the
/// gap between the HTTP response and the result frame. Scaling the ledger by
/// `floor` rather than by `stockAtTrigger - got` is what keeps the early rearm
/// honest: every path that spends from the book goes through the same floor.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Reconcile {
    /// Units the engine believes it can still buy at this level. Opening the
    /// window reserves the whole request out of it, a fill returns the part
    /// upstream did not take, and a result that obtained nothing returns the
    /// whole request, so a stale frame or a follow-up can never be handed the
    /// same units twice.
    pub(super) floor: i64,
    /// Wall-clock deadline. Past it the next frame is assumed fresh.
    pub(super) until: i64,
    /// Whole-book depth when the signal fired (`totalCount`). Lets a frame prove
    /// freshness by its total dropping by at least [`Reconcile::bought`].
    pub(super) total_at_trigger: Option<i64>,
    /// Units booked fills have actually taken from the level.
    pub(super) bought: i64,
    /// Best-level depth when the signal fired. A frame showing *more* than this
    /// has new listings the pre-purchase snapshot cannot contain.
    pub(super) trigger_stock: i64,
    /// Best-level price when the signal fired, as a micro-RLT key. A frame
    /// whose best price moved is a different level and carries new information.
    pub(super) price_key: i64,
}

impl Reconcile {
    /// Opens a ledger for the level a frame just advertised.
    pub(super) fn open(now: i64, stock: i64, price: f64, total: Option<i64>) -> Self {
        Self {
            floor: stock,
            until: now + RECONCILE_WINDOW_MS,
            total_at_trigger: total,
            bought: 0,
            trigger_stock: stock,
            price_key: price_key(price),
        }
    }

    /// True while the frame path must still defer to the local ledger.
    pub(super) fn live(&self, now: i64) -> bool {
        now < self.until
    }

    /// Removes the units a just-submitted order committed.
    pub(super) fn reserve(&mut self, qty: i64) {
        self.floor = self.floor.saturating_sub(qty).max(0);
    }

    /// Returns units an order did not actually take to the book.
    pub(super) fn release(&mut self, qty: i64) {
        self.floor = self
            .floor
            .saturating_add(qty.max(0))
            .min(self.trigger_stock);
    }
}
