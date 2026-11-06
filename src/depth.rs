//! Depth-aware fill price, ported from `src/shared/depth.js`.
//!
//! The cheapest ask alone is a poor market reference: a quantity-1 listing can
//! sit far below the level a real order would fill at. This computes the
//! average unit price for the configured order size across the visible ladder,
//! and consumes the aggregated `rest` tail when the ladder runs out.

use serde::Serialize;

#[derive(Debug, Clone, Copy)]
pub struct Listing {
    pub price: f64,
    pub quantity: i64,
}

impl Listing {
    pub fn new(price: f64, quantity: f64) -> Option<Self> {
        if !price.is_finite() || price <= 0.0 || !quantity.is_finite() || quantity <= 0.0 {
            return None;
        }
        Some(Self {
            price,
            quantity: quantity.floor() as i64,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Fill {
    pub price: f64,
    pub filled: i64,
    pub requested: i64,
    pub partial: bool,
    pub approx: bool,
    pub levels: usize,
}

/// Estimates the average unit price for `qty` units from the visible ladder.
///
/// Returns `None` when nothing could be filled, matching the JavaScript
/// function so the engine can keep treating `null` as "no reference".
pub fn estimate_fill_price(listings: &[Listing], rest: Option<Listing>, qty: i64) -> Option<Fill> {
    let mut levels: Vec<Listing> = listings.to_vec();
    // Same ordering as the JS: cheapest first, deeper level first on ties.
    levels.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.quantity.cmp(&a.quantity))
    });

    let requested = qty.max(1);
    let mut remaining = requested;
    let mut cost = 0.0_f64;
    let mut filled: i64 = 0;

    for level in &levels {
        if remaining <= 0 {
            break;
        }
        let take = remaining.min(level.quantity);
        cost += take as f64 * level.price;
        filled += take;
        remaining -= take;
    }

    let mut approx = false;
    if remaining > 0 {
        if let Some(tail) = rest {
            let take = remaining.min(tail.quantity);
            cost += take as f64 * tail.price;
            filled += take;
            remaining -= take;
            approx = true;
        }
    }

    if filled <= 0 || cost <= 0.0 {
        return None;
    }

    Some(Fill {
        price: cost / filled as f64,
        filled,
        requested,
        partial: remaining > 0,
        approx,
        levels: levels.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(price: f64, quantity: i64) -> Listing {
        Listing { price, quantity }
    }

    #[test]
    fn weights_a_thin_listing_less_than_a_deep_one() {
        // 1 unit @ 0.01 + 9 units @ 0.10 → average 0.091
        let fill = estimate_fill_price(&[level(0.10, 9), level(0.01, 1)], None, 10).unwrap();
        assert_eq!(fill.filled, 10);
        assert!((fill.price - 0.091).abs() < 1e-12);
        assert!(!fill.partial);
        assert!(!fill.approx);
    }

    #[test]
    fn consumes_the_rest_tail_and_flags_it_approximate() {
        let fill = estimate_fill_price(&[level(0.10, 2)], Some(level(0.20, 100)), 10).unwrap();
        assert_eq!(fill.filled, 10);
        assert!((fill.price - 0.18).abs() < 1e-12);
        assert!(fill.approx);
        assert!(!fill.partial);
    }

    #[test]
    fn reports_partial_when_even_the_tail_is_too_small() {
        let fill = estimate_fill_price(&[level(0.10, 2)], Some(level(0.20, 3)), 10).unwrap();
        assert_eq!(fill.filled, 5);
        assert!(fill.partial);
        assert!(fill.approx);
    }

    #[test]
    fn returns_none_without_usable_levels() {
        assert!(estimate_fill_price(&[], None, 10).is_none());
        assert!(estimate_fill_price(&[level(0.0, 5)], None, 1).is_none());
    }
}
