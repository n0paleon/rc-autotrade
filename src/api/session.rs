use serde_json::{Value, json};

use super::client::ApiClient;
use super::parse::message_from;
use super::types::{Credentials, RefreshResult};

impl ApiClient {
    /// Renew the session pair. The access token is deliberately not sent.
    pub async fn refresh_session(&self, credentials: &Credentials) -> RefreshResult {
        if credentials.refresh_token.is_empty() {
            return RefreshResult {
                ok: false,
                status: 0,
                error: Some("No refresh token available.".into()),
                ..Default::default()
            };
        }
        let payload = json!({ "refresh_token": credentials.refresh_token });
        let (status, body, text, transport_error, _marks) = self
            .post_json(
                "/api/auth/refresh",
                &payload,
                credentials,
                false,
                self.timeout,
            )
            .await;

        if let Some(error) = transport_error {
            return RefreshResult {
                ok: false,
                status: 0,
                error: Some(error),
                ..Default::default()
            };
        }
        if !matches!(status, 200..=299) {
            let error = message_from(body.as_ref(), &text, "Refresh was rejected.");
            return RefreshResult {
                ok: false,
                status,
                error: Some(error),
                ..Default::default()
            };
        }

        let body = body.unwrap_or(Value::Null);
        let access_token = body
            .pointer("/data/access_token")
            .or_else(|| body.get("access_token"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let next_refresh = body
            .pointer("/data/refresh_token")
            .or_else(|| body.get("refresh_token"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| Some(credentials.refresh_token.clone()));

        match access_token {
            Some(access_token) => RefreshResult {
                ok: true,
                status,
                access_token: Some(access_token),
                refresh_token: next_refresh,
                error: None,
            },
            None => RefreshResult {
                ok: false,
                status,
                error: Some("Refresh response did not include an access token.".into()),
                ..Default::default()
            },
        }
    }
}
