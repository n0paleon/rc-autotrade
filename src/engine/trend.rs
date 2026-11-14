use std::sync::Arc;

use serde_json::value::RawValue;

use crate::model::Target;
use crate::trend::{FillRef, Point, TrendOptions, TrendSignal, build_trend};

use super::runtime::Runtime;

impl super::Engine {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn compute_trend(
        config: &Target,
        runtime: &mut Runtime,
        now: i64,
        force: bool,
    ) -> Option<TrendSignal> {
        let target_price = config.target_price;
        if !force
            && runtime.trend.is_some()
            && runtime.trend_target == target_price
            && now - runtime.trend_at < super::TREND_REFRESH_MS
        {
            return runtime.trend.clone();
        }

        let previous_state = if runtime.trend_target == target_price {
            runtime.trend.as_ref().map(|trend| trend.state.to_string())
        } else {
            None
        };

        let fill_points = &runtime.fill_history;
        let fill_span = match (fill_points.first(), fill_points.last()) {
            (Some(first), Some(last)) => last.t - first.t,
            _ => 0,
        };
        let fill_fresh = fill_points
            .last()
            .map(|point| now - point.t <= 30 * 60_000)
            .unwrap_or(false);
        let use_fill = fill_points.len() >= 12 && fill_span >= 30 * 60_000 && fill_fresh;

        let book_qty = runtime.last_stock;
        let options = TrendOptions {
            now,
            target_price: Some(target_price),
            previous_state,
            book_qty,
            min_qty: Some(config.min_acceptable_qty),
            depth_source: use_fill,
            fill: runtime.last_fill.map(|fill| FillRef {
                price: fill.price,
                filled: fill.filled as f64,
                partial: fill.partial,
                approx: fill.approx,
            }),
        };
        let signal = {
            // Borrow the series instead of cloning up to 1440 points on every
            // rebuild: the model only ever reads it, so the copy was pure churn.
            let series: &[Point] = if use_fill {
                &runtime.fill_history
            } else {
                &runtime.history
            };
            build_trend(series, &options)
        };
        runtime.trend = Some(signal.clone());
        runtime.trend_json = serde_json::to_string(&signal)
            .ok()
            .and_then(|text| RawValue::from_string(text).ok())
            .map(Arc::from);
        runtime.trend_at = now;
        runtime.trend_target = target_price;
        Some(signal)
    }
}
