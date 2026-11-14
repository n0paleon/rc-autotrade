//! Pure order planning.
//!
//! Keeping this file dependency-free is deliberate: it is the one place that
//! decides whether money is spent, so it must stay trivially testable and
//! auditable.

use crate::constants::{Limits, PRICE_SCALE};
use crate::num::js_round;
use serde::Serialize;

/// Rounds to the 6-decimal RLT precision used everywhere in the worker.
#[inline]
pub fn round_price(value: f64) -> f64 {
    js_round((value + f64::EPSILON) * PRICE_SCALE) / PRICE_SCALE
}

/// Adds the configured fulfillment headroom to a purchase request's
/// `totalPrice`.
///
/// The marketplace fills an order from the cheapest listings upwards and treats
/// `totalPrice` as the ceiling it may spend. Sending exactly `unitPrice × qty`
/// therefore leaves no room for the book to move between the market frame and
/// the purchase, which is what produces partial fills. Scaling the ceiling by
/// `markup_pct` buys the order room to complete: at 10%, a `1.000000` total is
/// sent as `1.100000`.
///
/// `total_price` is in raw micro-RLT and the result is rounded to the smallest
/// unit Rollercoin quotes. A non-positive or non-finite markup is ignored, so
/// `0` leaves the payload byte-identical to the pre-feature request.
#[inline]
pub fn apply_total_price_markup(total_price: f64, markup_pct: f64) -> f64 {
    if !total_price.is_finite() || total_price <= 0.0 {
        return total_price;
    }
    if !markup_pct.is_finite() || markup_pct <= 0.0 {
        return total_price;
    }
    let scaled = total_price * (1.0 + markup_pct / 100.0);
    if scaled.is_finite() {
        scaled.round()
    } else {
        total_price
    }
}

/// The subset of a target the planner needs. Taken by value so the planner
/// never touches the store and stays cheap to call on the frame path.
#[derive(Debug, Clone, Copy)]
pub struct PlanInput {
    pub active: bool,
    pub target_price: f64,
    pub desired_qty: i64,
    pub high_stock_threshold: i64,
    pub min_acceptable_qty: i64,
    pub allow_partial_order: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Buy,
    Skip,
}

/// Omits absent optional fields so the wire shape stays stable
/// (`{action:"skip", reason:"paused"}`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Plan {
    pub action: Action,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qty: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doubled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notional: Option<f64>,
}

impl Plan {
    fn skip(reason: &'static str) -> Self {
        Self {
            action: Action::Skip,
            reason: Some(reason),
            qty: None,
            doubled: None,
            notional: None,
        }
    }

    pub fn is_buy(&self) -> bool {
        self.action == Action::Buy
    }
}

/// Pure decision function that mirrors (and hardens) the original trade flow.
///
/// `plan_blocked` is the "stop deciding" gate (mode `off`). It short-circuits
/// before price and stock are read, which is what makes `off` free: no cooldown
/// or rate slot is spent. Callers that still want an honest log line can run the
/// same inputs a second time with `plan_blocked = false`; the function is pure,
/// so that costs nothing but a few nanoseconds.
pub fn plan_order(
    input: PlanInput,
    unit_price: f64,
    available: f64,
    plan_blocked: bool,
    target_price_override: Option<f64>,
) -> Plan {
    if plan_blocked {
        return Plan::skip("paused");
    }
    if !input.active {
        return Plan::skip("inactive");
    }
    if !unit_price.is_finite() || unit_price <= 0.0 {
        return Plan::skip("invalid-price");
    }
    if !available.is_finite() || available < 1.0 {
        return Plan::skip("no-stock");
    }

    let ceiling = target_price_override
        .filter(|value| value.is_finite())
        .unwrap_or(input.target_price);
    if unit_price > ceiling + 1e-9 {
        return Plan::skip("price-above-target");
    }

    let desired = input.desired_qty.max(1);
    let (qty, doubled) = if available >= desired as f64 {
        // The original script doubles the desired qty when the market is stacked.
        let doubled = available >= input.high_stock_threshold as f64;
        (if doubled { desired * 2 } else { desired }, doubled)
    } else if input.allow_partial_order && available >= input.min_acceptable_qty as f64 {
        (available.floor() as i64, false)
    } else {
        return Plan::skip("stock-below-minimum");
    };

    let qty = qty
        .max(1)
        .min(available.floor() as i64)
        .min(Limits::QTY_MAX);

    Plan {
        action: Action::Buy,
        reason: None,
        qty: Some(qty),
        doubled: Some(doubled),
        notional: Some(round_price(unit_price * qty as f64)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::num::js_round;

    fn input() -> PlanInput {
        PlanInput {
            active: true,
            target_price: 0.1,
            desired_qty: 10,
            high_stock_threshold: 20,
            min_acceptable_qty: 1,
            allow_partial_order: true,
        }
    }

    #[test]
    fn buys_when_price_is_at_or_below_target() {
        let plan = plan_order(input(), 0.09, 10.0, false, None);
        assert!(plan.is_buy());
        assert_eq!(plan.qty, Some(10));
        assert_eq!(plan.notional, Some(0.9));
        assert_eq!(plan.doubled, Some(false));
    }

    #[test]
    fn doubles_when_stock_reaches_the_high_threshold() {
        let plan = plan_order(input(), 0.09, 25.0, false, None);
        assert_eq!(plan.qty, Some(20));
        assert_eq!(plan.doubled, Some(true));
    }

    #[test]
    fn takes_partial_stock_when_allowed() {
        let plan = plan_order(input(), 0.09, 4.0, false, None);
        assert_eq!(plan.qty, Some(4));
        assert_eq!(plan.doubled, Some(false));
    }

    #[test]
    fn refuses_partial_stock_below_the_minimum() {
        let mut args = input();
        args.min_acceptable_qty = 5;
        let plan = plan_order(args, 0.09, 4.0, false, None);
        assert_eq!(plan.reason, Some("stock-below-minimum"));
    }

    #[test]
    fn stops_above_the_target_ceiling() {
        let plan = plan_order(input(), 0.11, 10.0, false, None);
        assert_eq!(plan.reason, Some("price-above-target"));
    }

    #[test]
    fn paused_and_inactive_win_over_everything_else() {
        assert_eq!(
            plan_order(input(), 0.09, 10.0, true, None).reason,
            Some("paused")
        );
        let mut off = input();
        off.active = false;
        assert_eq!(
            plan_order(off, 0.09, 10.0, false, None).reason,
            Some("inactive")
        );
    }

    #[test]
    fn rounds_half_up_towards_positive_infinity() {
        assert_eq!(round_price(0.1234567), 0.123457);
        assert_eq!(round_price(0.1 * 3.0), 0.3);
        assert_eq!(js_round(-2.5), -2.0); // Math.round(-2.5) === -2
        assert_eq!(js_round(2.5), 3.0);
    }

    #[test]
    fn total_price_markup_is_opt_in_and_rounds_to_micro_rlt() {
        // Disabled or invalid markup leaves the exact listing total alone.
        assert_eq!(apply_total_price_markup(1_000_000.0, 0.0), 1_000_000.0);
        assert_eq!(apply_total_price_markup(1_000_000.0, f64::NAN), 1_000_000.0);
        assert_eq!(apply_total_price_markup(1_000_000.0, -5.0), 1_000_000.0);
        assert_eq!(apply_total_price_markup(0.0, 10.0), 0.0);
        assert!(apply_total_price_markup(f64::NAN, 10.0).is_nan());

        // 1.000000 RLT at 10% becomes the 1.100000 RLT ceiling.
        assert_eq!(apply_total_price_markup(1_000_000.0, 10.0), 1_100_000.0);
        assert_eq!(apply_total_price_markup(900_000.0, 10.0), 990_000.0);
        // Fractional micro-RLT is rounded to the smallest quoted unit.
        assert_eq!(apply_total_price_markup(999_999.0, 7.5), 1_074_999.0);
        assert_eq!(apply_total_price_markup(1.0, 10.0), 1.0);
    }
}
