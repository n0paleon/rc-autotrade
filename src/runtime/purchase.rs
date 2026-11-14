//! Purchase execution and its DNS/socket telemetry.

use std::sync::Arc;

use crate::api::{PurchaseRequest, PurchaseResult};
use crate::clock::now_ms;
use crate::dns_probe::{self, SocketMode};
use crate::engine::{OrderIntent, PurchaseOutcome};
use crate::lock::lock;
use crate::mock::MockHandle;
use crate::server::AppState;
use crate::session;
use crate::timings::Timings;

use super::frames::dispatch;
use super::session::refresh_session;

pub(super) fn credentials(state: &Arc<AppState>) -> crate::api::Credentials {
    let store = lock(&state.store);
    session::credentials(&store.state.session)
}

pub(super) async fn run_purchase(state: Arc<AppState>, mut intent: OrderIntent) {
    let request = PurchaseRequest {
        item_id: intent.item_id.clone(),
        item_type: intent.item_type.clone(),
        qty: intent.qty,
        unit_price_raw: intent.unit_price_raw,
        total_price_markup_pct: intent.total_price_markup_pct,
    };
    let request_payload = request.payload();

    // MOCK=1 routes purchases to the simulator; the HTTP client is untouched.
    let mock: Option<MockHandle> = lock(&state.mock).clone();
    let real = mock.is_none();

    let mut result = match mock {
        Some(mock) => mock.purchase(request.clone()).await,
        None => state.api.purchase(&credentials(&state), &request).await,
    };
    // The first dispatch stays on the timeline even across a 401 retry, so the
    // stage still shows the refresh round-trip; the retry contributes the final
    // headers and TTFB.
    let send_at = result.send_at;
    let mut response_at = result.response_at;
    let mut ttfb_ms = result.ttfb_ms;
    let mut complete_at = now_ms();
    let mut attempts: u8 = 1;
    let mut refreshed = false;

    // 401 → refresh → retry once.
    if result.unauthorized {
        state.logger.warn(
            "order",
            "session rejected the purchase (401); refreshing and retrying",
        );
        if refresh_session(&state).await {
            result = state.api.purchase(&credentials(&state), &request).await;
            response_at = result.response_at;
            ttfb_ms = result.ttfb_ms;
            complete_at = now_ms();
            attempts = 2;
            refreshed = true;
        }
    }

    intent.timings.attempts = Some(attempts);
    if refreshed {
        intent.timings.unauthorized = Some(true);
    }

    if real {
        let host = dns_probe::host_of(state.api.base_url());
        apply_dns_telemetry(&mut intent.timings, host.as_deref(), &result);
    } else {
        // The simulator performs no HTTP exchange, so there is no header mark.
        // Keep the timeline readable by falling back to the completion time.
        response_at = response_at.or(Some(complete_at));
    }

    let outcome = PurchaseOutcome {
        ok: result.ok,
        order_id: result.order_id,
        error: result.error,
        send_at,
        response_at,
        complete_at: Some(complete_at),
        ttfb_ms,
        accepted_at: None,
        request_payload: Some(request_payload),
    };

    let events = {
        let mut store = lock(&state.store);
        let mut engine = lock(&state.engine);
        engine.on_purchase_result(&mut store, &intent, outcome)
    };
    dispatch(&state, events);
}

/// Writes the connection and resolver facts onto the order timings. A fresh
/// connection keeps the real lookup; a pooled keep-alive socket reports
/// `reused` with no DNS cost; IP and local upstreams are `skipped`.
pub(super) fn apply_dns_telemetry(
    timings: &mut Timings,
    host: Option<&str>,
    result: &PurchaseResult,
) {
    timings.socket_mode = Some(result.socket.as_str().to_string());

    let Some(host) = host else {
        return;
    };
    timings.dns_host = Some(host.chars().take(128).collect());
    if dns_probe::is_local(host) {
        timings.dns_mode = Some("skipped".to_string());
        return;
    }

    match &result.dns {
        // hyper only resolves when it opens a connection, so this is the lookup
        // this purchase actually paid for.
        Some(lookup) => {
            timings.dns_mode = Some("lookup".to_string());
            timings.dns_ms = Some(lookup.ms);
            timings.dns_address = lookup.address.clone();
            timings.dns_family = lookup.family;
            timings.dns_error = lookup.error.clone();
        }
        None if result.socket == SocketMode::Reused => {
            timings.dns_mode = Some("reused".to_string());
        }
        // A connection was opened but the lookup did not complete (cancelled
        // by a timeout, for example), or the upstream is not observable.
        None => {
            timings.dns_mode = Some("unknown".to_string());
        }
    }
}
