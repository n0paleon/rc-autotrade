//! Session refresh used by the scheduled tick, the feed and the HTTP handler.

use std::sync::Arc;

use crate::clock::now_ms;
use crate::lock::lock;
use crate::server::AppState;
use crate::session;

/// One refresh attempt. Returns whether the access token changed.
pub async fn refresh_session(state: &Arc<AppState>) -> bool {
    let Ok(_guard) = state.refresh_lock.try_lock() else {
        // Another refresh is already in flight.
        return false;
    };

    let session_snapshot = {
        let store = lock(&state.store);
        store.state.session.clone()
    };
    let result = session::refresh_once(&state.api, &session_snapshot).await;

    if !result.ok {
        let message = result
            .error
            .clone()
            .unwrap_or_else(|| "refresh failed".into());
        let status = {
            let mut store = lock(&state.store);
            store.mark_session_error(&message);
            store.session_status()
        };
        state
            .logger
            .warn("session", format!("refresh failed: {message}"));
        // The deck used to learn about a failed refresh from the session
        // manager's `updated` event; without this push the Link/HUD panels
        // kept showing the last successful status until a manual refresh.
        state.broadcast("session", status);
        state.wake_keepalive("session refresh failed");
        return false;
    }

    let (changed, status) = {
        let mut store = lock(&state.store);
        let changed = session::apply_refresh(&mut store, &result);
        (changed, store.session_status())
    };
    state.logger.info("session", "access token renewed");
    state.broadcast("session", status);
    state.wake_keepalive("session refreshed");
    if changed {
        // A refresh that happens while the feed is down must reopen it with the
        // new token; a live feed keeps its socket.
        crate::server::reconnect_gateway(state, "session refreshed", false).await;
    }
    changed
}

pub(super) async fn tick_session(state: &Arc<AppState>) {
    let due = {
        let store = lock(&state.store);
        session::needs_refresh(&store.state.session, &store.state.settings, now_ms())
    };
    if due {
        refresh_session(state).await;
    }
}
