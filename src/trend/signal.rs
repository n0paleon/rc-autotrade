use super::math::{clamp, round};
use super::model::compute_trend_model;
use super::types::{Point, TrendOptions, TrendSignal};

/// Widens the entry band with the model's uncertainty, inside hard limits.
pub fn classification_band_pct(error_band_pct: Option<f64>) -> f64 {
    let band = error_band_pct
        .filter(|value| value.is_finite())
        .unwrap_or(2.0);
    clamp(band, 2.0, 10.0)
}

/// Positive proximity = target above the trend; negative = waiting for a dip.
pub fn classify_state(
    proximity_pct: f64,
    previous_state: Option<&str>,
    band_pct: f64,
) -> &'static str {
    let band = if band_pct.is_finite() && band_pct > 0.0 {
        band_pct
    } else {
        2.0
    };
    let held = matches!(previous_state, Some("NEAR") | Some("UNSURE"));
    let exit = if held { (band * 1.5).max(3.0) } else { band };

    if proximity_pct > exit {
        return "ABOVE";
    }
    if proximity_pct < -exit {
        return if proximity_pct <= -20.0 {
            "FAR_BELOW"
        } else {
            "BELOW"
        };
    }
    if held {
        return match previous_state {
            Some("NEAR") => "NEAR",
            Some("UNSURE") => "UNSURE",
            _ => "NEAR",
        };
    }
    if band >= 5.0 { "UNSURE" } else { "NEAR" }
}

pub fn recommend_action(
    state: &str,
    reliable: bool,
    reliable_reason: Option<&'static str>,
) -> (&'static str, &'static str) {
    match state {
        "STALE" => ("wait", "stale"),
        "NO_DATA" => ("wait", "no-data"),
        "NO_TARGET" => ("set-target", "no-target"),
        "UNSURE" => ("wait", "unresolved"),
        "ABOVE" if !reliable => ("wait", reliable_reason.unwrap_or("low-confidence")),
        "ABOVE" => ("lower", "above-trend"),
        "FAR_BELOW" => ("review", "far-below"),
        "NEAR" => ("hold", "at-trend"),
        _ => ("hold", "below-trend"),
    }
}

fn with_action(mut signal: TrendSignal) -> TrendSignal {
    let (action, reason) = recommend_action(signal.state, signal.reliable, signal.reliable_reason);
    signal.action = action;
    signal.action_reason = reason;
    signal
}

/// Adds the target-relative reading on top of the market model.
pub fn build_trend(history: &[Point], options: &TrendOptions) -> TrendSignal {
    let model = compute_trend_model(history, options);
    let target = options
        .target_price
        .filter(|value| value.is_finite() && *value > 0.0);

    if model.status != "ok" {
        let state = if model.status == "stale" {
            "STALE"
        } else {
            "NO_DATA"
        };
        let reliable_reason = if model.status == "stale" {
            Some("stale")
        } else {
            Some("no-data")
        };
        return with_action(TrendSignal {
            model,
            target,
            proximity_pct: None,
            gap_pct: None,
            band_pct: None,
            uncertain: false,
            state,
            reliable: false,
            reliable_reason,
            action: "wait",
            action_reason: "wait",
        });
    }

    if target.is_none() {
        let band_pct = round(classification_band_pct(model.error_band_pct), 2);
        return with_action(TrendSignal {
            model,
            target: None,
            proximity_pct: None,
            gap_pct: None,
            band_pct,
            uncertain: false,
            state: "NO_TARGET",
            reliable: false,
            reliable_reason: Some("no-target"),
            action: "set-target",
            action_reason: "no-target",
        });
    }

    let target = target.expect("checked above");
    let trend_level = model.trend.unwrap_or(f64::NAN);
    let proximity_pct = (target / trend_level - 1.0) * 100.0;
    let band_pct = classification_band_pct(model.error_band_pct);
    let enough_evidence = model.confidence >= 50;
    let trustworthy_band = model.error_band_pct.unwrap_or(20.0) <= 10.0;
    let state = classify_state(proximity_pct, options.previous_state.as_deref(), band_pct);
    let reliable = enough_evidence && trustworthy_band;
    let reliable_reason = if reliable {
        None
    } else if enough_evidence {
        Some("wide-band")
    } else {
        Some("low-confidence")
    };

    with_action(TrendSignal {
        model,
        target: Some(target),
        proximity_pct: round(proximity_pct, 3),
        gap_pct: round(-proximity_pct, 3),
        band_pct: round(band_pct, 2),
        uncertain: state == "UNSURE",
        state,
        reliable,
        reliable_reason,
        action: "hold",
        action_reason: "hold",
    })
}
