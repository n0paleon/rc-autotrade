use rusqlite::params;
use serde_json::Value;

use crate::clock::now_ms;
use crate::validation::{self, SessionPatch};

use super::Store;
use super::tokens::preview_token;

impl Store {
    // ── session ─────────────────────────────────────────────────────────────

    pub fn apply_session_patch(&mut self, patch: SessionPatch) -> Result<(), String> {
        let session = &mut self.state.session;
        if patch.clear_access_token {
            session.access_token.clear();
        }
        if patch.clear_refresh_token {
            session.refresh_token.clear();
        }
        if patch.clear_cookie {
            session.cookie.clear();
        }
        if let Some(token) = patch.access_token {
            session.access_token = token;
        }
        if let Some(token) = patch.refresh_token {
            session.refresh_token = token;
        }
        if let Some(cookie) = patch.cookie {
            session.cookie = cookie;
        }
        if let Some(source) = patch.source {
            session.source = Some(source);
        }
        session.updated_at = Some(now_ms());
        session.last_error = None;
        self.persist_session()
    }

    pub fn mark_refreshed(&mut self, access_token: &str, refresh_token: Option<&str>) {
        let now = now_ms();
        self.state.session.access_token = access_token.to_string();
        if let Some(token) = refresh_token.filter(|t| !t.is_empty()) {
            self.state.session.refresh_token = token.to_string();
        }
        self.state.session.last_refresh_at = Some(now);
        self.state.session.refresh_count += 1;
        self.state.session.last_error = None;
        if let Err(error) = self.persist_session() {
            self.report_write_error("could not persist the refreshed session", &error);
        }
    }

    pub fn mark_session_error(&mut self, message: &str) {
        self.state.session.last_error = Some(crate::redact::redact_secrets(message));
        if let Err(error) = self.persist_session() {
            self.report_write_error("could not persist the session error", &error);
        }
    }

    pub fn session_status(&self) -> Value {
        let session = &self.state.session;
        serde_json::json!({
            "hasAccessToken": !session.access_token.is_empty(),
            "hasRefreshToken": !session.refresh_token.is_empty(),
            "hasCookie": !session.cookie.is_empty(),
            "accessPreview": preview_token(&session.access_token),
            "refreshPreview": preview_token(&session.refresh_token),
            "updatedAt": session.updated_at,
            "source": session.source,
            "lastRefreshAt": session.last_refresh_at,
            "refreshCount": session.refresh_count,
            "lastError": session.last_error,
            "expiresAt": validation::jwt_expiry(&session.access_token),
        })
    }

    fn persist_session(&self) -> Result<(), String> {
        let session = &self.state.session;
        self.db
            .execute(
                "UPDATE session SET access_token = ?1, refresh_token = ?2, cookie = ?3,
                        source = ?4, updated_at = ?5, last_refresh_at = ?6,
                        refresh_count = ?7, last_error = ?8 WHERE id = 1",
                params![
                    session.access_token,
                    session.refresh_token,
                    session.cookie,
                    session.source,
                    session.updated_at,
                    session.last_refresh_at,
                    session.refresh_count,
                    session.last_error
                ],
            )
            .map(|_| ())
            .map_err(|e| format!("could not persist session: {e}"))
    }
}
