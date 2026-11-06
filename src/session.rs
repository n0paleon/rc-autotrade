//! Session freshness helpers, ported from `src/worker/session.js`.
//!
//! The scheduler (interval, coalescing, event emission) lives in the wiring so
//! this module stays pure and testable: given a stored session and `now`,
//! decide whether a refresh is due, and apply a refresh result to the store.
//!
//! Timing rules, copied 1:1:
//! * refresh when the JWT expires within 2 minutes, but never more often than
//!   once per 30 seconds (Rollercoin issues ~10 minute tokens, so "expires
//!   within 10 minutes" would otherwise refresh on every tick);
//! * for opaque tokens fall back to a 25 minute cadence anchored at
//!   `lastRefreshAt`, then `updatedAt`.

use crate::api::{ApiClient, Credentials, RefreshResult};
use crate::model::{Session, Settings};
use crate::store::Store;
use crate::validation::jwt_expiry;

pub const REFRESH_AHEAD_MS: i64 = 2 * 60 * 1000;
pub const MIN_REFRESH_INTERVAL_MS: i64 = 30 * 1000;
pub const FALLBACK_REFRESH_MS: i64 = 25 * 60 * 1000;
pub const FAILURE_COOLDOWN_MS: i64 = 60 * 1000;
pub const TICK_MS: i64 = 30 * 1000;

/// True when the stored session should be refreshed now.
pub fn needs_refresh(session: &Session, settings: &Settings, now: i64) -> bool {
    if !settings.auto_refresh {
        return false;
    }
    if session.access_token.is_empty() || session.refresh_token.is_empty() {
        return false;
    }

    if let Some(expires_at) = jwt_expiry(&session.access_token) {
        if expires_at - now > REFRESH_AHEAD_MS {
            return false;
        }
        let anchor = session.last_refresh_at.unwrap_or(0);
        return now - anchor >= MIN_REFRESH_INTERVAL_MS;
    }

    let anchor = session.last_refresh_at.or(session.updated_at).unwrap_or(0);
    now - anchor >= FALLBACK_REFRESH_MS
}

/// Credentials snapshot for one API call.
pub fn credentials(session: &Session) -> Credentials {
    Credentials {
        access_token: session.access_token.clone(),
        refresh_token: session.refresh_token.clone(),
        cookie: session.cookie.clone(),
    }
}

/// Apply a successful refresh to the store. Returns whether anything changed.
pub fn apply_refresh(store: &mut Store, result: &RefreshResult) -> bool {
    if !result.ok {
        return false;
    }
    let Some(access_token) = result.access_token.as_deref() else {
        return false;
    };
    let changed = store.state.session.access_token != access_token;
    store.mark_refreshed(access_token, result.refresh_token.as_deref());
    changed
}

/// One refresh attempt against the upstream. Coalescing is the caller's job.
pub async fn refresh_once(client: &ApiClient, session: &Session) -> RefreshResult {
    let credentials = credentials(session);
    client.refresh_session(&credentials).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger::Logger;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    const NOW: i64 = 1_777_982_400_000;

    fn session(access: &str, refresh: &str) -> Session {
        Session {
            access_token: access.into(),
            refresh_token: refresh.into(),
            ..Default::default()
        }
    }

    fn settings() -> Settings {
        Settings::default()
    }

    /// Builds a JWT whose `exp` is `now + seconds` (signature ignored upstream).
    fn jwt_expiring_in(seconds: i64) -> String {
        use base64::Engine as _;
        let payload = serde_json::json!({ "exp": (NOW + seconds * 1000) / 1000 });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("x.{encoded}.y")
    }

    #[test]
    fn a_fresh_token_is_not_refreshed() {
        let mut s = session(&jwt_expiring_in(10 * 60), "refresh");
        s.last_refresh_at = Some(NOW - 5 * 60 * 1000);
        assert!(!needs_refresh(&s, &settings(), NOW));
    }

    #[test]
    fn a_token_near_expiry_is_refreshed_but_rate_limited() {
        let mut s = session(&jwt_expiring_in(60), "refresh");
        s.last_refresh_at = Some(NOW - 10_000);
        assert!(
            !needs_refresh(&s, &settings(), NOW),
            "inside the 30 s guard"
        );

        s.last_refresh_at = Some(NOW - 45_000);
        assert!(needs_refresh(&s, &settings(), NOW));
    }

    #[test]
    fn opaque_tokens_fall_back_to_the_25_minute_cadence() {
        let mut s = session("opaque-access", "refresh");
        s.updated_at = Some(NOW - 10 * 60 * 1000);
        assert!(!needs_refresh(&s, &settings(), NOW));

        s.updated_at = Some(NOW - 26 * 60 * 1000);
        assert!(needs_refresh(&s, &settings(), NOW));

        // `lastRefreshAt` wins over `updatedAt` as the anchor.
        s.last_refresh_at = Some(NOW - 60_000);
        assert!(!needs_refresh(&s, &settings(), NOW));
    }

    #[test]
    fn missing_credentials_or_disabled_auto_refresh_never_refresh() {
        assert!(!needs_refresh(&session("", "refresh"), &settings(), NOW));
        assert!(!needs_refresh(&session("access", ""), &settings(), NOW));

        let mut disabled = settings();
        disabled.auto_refresh = false;
        let s = session(&jwt_expiring_in(10), "refresh");
        assert!(!needs_refresh(&s, &disabled, NOW));
    }

    #[test]
    fn applying_a_refresh_updates_the_store_and_counts_it() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rc-rust-session-{nanos}"));
        let mut store = Store::open(dir.clone(), Arc::new(Logger::new("error"))).unwrap();
        let patch = crate::validation::SessionPatch {
            access_token: Some("old-access".into()),
            refresh_token: Some("old-refresh".into()),
            ..Default::default()
        };
        store.apply_session_patch(patch).unwrap();
        let before = store.state.session.refresh_count;

        let changed = apply_refresh(
            &mut store,
            &RefreshResult {
                ok: true,
                status: 200,
                access_token: Some("new-access".into()),
                refresh_token: Some("new-refresh".into()),
                error: None,
            },
        );
        assert!(changed);
        assert_eq!(store.state.session.access_token, "new-access");
        assert_eq!(store.state.session.refresh_token, "new-refresh");
        assert_eq!(store.state.session.refresh_count, before + 1);
        assert!(store.state.session.last_refresh_at.is_some());

        // A failed refresh must leave the session untouched.
        let unchanged = apply_refresh(
            &mut store,
            &RefreshResult {
                ok: false,
                status: 401,
                error: Some("nope".into()),
                ..Default::default()
            },
        );
        assert!(!unchanged);
        assert_eq!(store.state.session.access_token, "new-access");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Bridge merge rules from `server.js #mergePushedSession`.
///
/// A userscript pushes the browser's token; accept it when the worker has none,
/// when the current token is expired, or when the incoming one expires clearly
/// later (30 s of headroom). Same token only backfills a missing refresh token.
pub fn merge_pushed(
    store: &mut Store,
    incoming_access: &str,
    incoming_refresh: &str,
    now: i64,
) -> bool {
    if incoming_access.is_empty() {
        return false;
    }
    let session = store.state.session.clone();

    if session.access_token.is_empty() {
        let mut patch = crate::validation::SessionPatch::default();
        patch.access_token = Some(incoming_access.to_string());
        if !incoming_refresh.is_empty() {
            patch.refresh_token = Some(incoming_refresh.to_string());
        }
        patch.source = Some("bridge".into());
        let _ = store.apply_session_patch(patch);
        return true;
    }

    if session.access_token == incoming_access {
        if session.refresh_token.is_empty() && !incoming_refresh.is_empty() {
            let patch = crate::validation::SessionPatch {
                refresh_token: Some(incoming_refresh.to_string()),
                source: Some("bridge".into()),
                ..Default::default()
            };
            let _ = store.apply_session_patch(patch);
        }
        return false;
    }

    let current_exp = jwt_expiry(&session.access_token);
    let incoming_exp = jwt_expiry(incoming_access);
    let accept = match (current_exp, incoming_exp) {
        (Some(current), _) if current <= now => true,
        (Some(current), Some(incoming)) => incoming > current + 30_000,
        (None, Some(_)) => true,
        (Some(_), None) => false,
        (None, None) => now - session.updated_at.unwrap_or(0) > 20 * 60 * 1_000,
    };
    if !accept {
        return false;
    }

    let mut patch = crate::validation::SessionPatch::default();
    patch.access_token = Some(incoming_access.to_string());
    if !incoming_refresh.is_empty() {
        patch.refresh_token = Some(incoming_refresh.to_string());
    }
    patch.source = Some("bridge".into());
    let _ = store.apply_session_patch(patch);
    true
}

#[cfg(test)]
mod merge_tests {
    use super::*;
    use crate::logger::Logger;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    const NOW: i64 = 1_777_982_400_000;

    fn store(tag: &str) -> (Store, std::path::PathBuf) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rc-rust-merge-{tag}-{nanos}"));
        let store = Store::open(dir.clone(), Arc::new(Logger::new("error"))).unwrap();
        (store, dir)
    }

    fn jwt(seconds_from_now: i64) -> String {
        use base64::Engine as _;
        let payload = serde_json::json!({ "exp": (NOW + seconds_from_now * 1000) / 1000 });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("x.{encoded}.y")
    }

    fn seed(store: &mut Store, access: &str, refresh: &str, updated_at: i64) {
        let patch = crate::validation::SessionPatch {
            access_token: Some(access.to_string()),
            refresh_token: Some(refresh.to_string()),
            ..Default::default()
        };
        store.apply_session_patch(patch).unwrap();
        store.state.session.updated_at = Some(updated_at);
    }

    #[test]
    fn accepts_when_the_worker_has_no_session() {
        let (mut store, dir) = store("empty");
        assert!(merge_pushed(
            &mut store,
            "incoming-token-123",
            "refresh-123",
            NOW
        ));
        assert_eq!(store.state.session.access_token, "incoming-token-123");
        assert_eq!(store.state.session.source.as_deref(), Some("bridge"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn same_token_only_backfills_a_missing_refresh() {
        let (mut store, dir) = store("same");
        let token = jwt(600);
        seed(&mut store, &token, "", NOW);
        assert!(!merge_pushed(&mut store, &token, "new-refresh-123", NOW));
        assert_eq!(store.state.session.refresh_token, "new-refresh-123");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn replaces_an_expired_token() {
        let (mut store, dir) = store("expired");
        seed(&mut store, &jwt(-60), "refresh", NOW);
        assert!(merge_pushed(&mut store, &jwt(600), "refresh", NOW));
        assert!(store.state.session.access_token.starts_with("x."));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ignores_a_token_that_expires_earlier() {
        let (mut store, dir) = store("earlier");
        seed(&mut store, &jwt(3600), "refresh", NOW);
        let before = store.state.session.access_token.clone();
        assert!(!merge_pushed(&mut store, &jwt(600), "refresh", NOW));
        assert_eq!(store.state.session.access_token, before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn opaque_tokens_use_the_twenty_minute_rule() {
        let (mut store, dir) = store("opaque");
        seed(
            &mut store,
            "opaque-current",
            "refresh",
            NOW - 25 * 60 * 1_000,
        );
        assert!(merge_pushed(&mut store, "opaque-incoming", "refresh", NOW));
        assert_eq!(store.state.session.access_token, "opaque-incoming");
        let _ = std::fs::remove_dir_all(dir);
    }
}
