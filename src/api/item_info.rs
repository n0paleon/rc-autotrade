use std::time::Duration;

use serde_json::Value;

use super::ITEM_INFO_TIMEOUT_MS;
use super::client::ApiClient;
use super::parse::{message_from, read_json};
use super::types::{Credentials, ItemInfoResult};

impl ApiClient {
    /// Resolve an item's label/rarity for the deck.
    pub async fn fetch_item_info(
        &self,
        credentials: &Credentials,
        item_id: &str,
        item_type: &str,
        currency: &str,
    ) -> ItemInfoResult {
        self.touch();
        let started = std::time::Instant::now();
        let item_type = if item_type.is_empty() {
            "mutation_component"
        } else {
            item_type
        };
        let mut url = url::Url::parse(&format!("{}/api/marketplace/item-info", self.base_url))
            .expect("item-info url");
        url.query_pairs_mut()
            .append_pair("itemId", item_id)
            .append_pair("itemType", item_type)
            .append_pair("currency", currency);
        let result = self
            .http
            .get(url)
            .headers(self.headers(credentials, false))
            .timeout(Duration::from_millis(ITEM_INFO_TIMEOUT_MS))
            .send()
            .await;

        match result {
            Ok(response) => {
                let status = response.status().as_u16();
                let (body, text) = read_json(response).await;
                self.record_http(started, true, None);
                if !matches!(status, 200..=299) {
                    return ItemInfoResult {
                        ok: false,
                        status,
                        error: Some(message_from(
                            body.as_ref(),
                            &text,
                            "item-info request failed.",
                        )),
                        ..Default::default()
                    };
                }
                let body = body.unwrap_or(Value::Null);
                if body.get("success").and_then(Value::as_bool) != Some(true) {
                    return ItemInfoResult {
                        ok: false,
                        status,
                        error: Some(
                            body.get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("item-info response did not include data.")
                                .to_string(),
                        ),
                        ..Default::default()
                    };
                }
                match body.get("data") {
                    Some(data) => ItemInfoResult {
                        ok: true,
                        status,
                        data: Some(data.clone()),
                        error: None,
                    },
                    None => ItemInfoResult {
                        ok: false,
                        status,
                        error: Some("item-info response did not include data.".into()),
                        ..Default::default()
                    },
                }
            }
            Err(error) => {
                let message = if error.is_timeout() {
                    "item-info request timed out.".to_string()
                } else {
                    crate::redact::redact_secrets(&error.to_string())
                };
                self.record_http(started, false, Some(&message));
                ItemInfoResult {
                    ok: false,
                    status: 0,
                    error: Some(message),
                    ..Default::default()
                }
            }
        }
    }
}
