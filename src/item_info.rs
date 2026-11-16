//! Item label/rarity resolver.
//!
//! Calls the marketplace `item-info` endpoint, composes a `Rarity Name` label
//! and writes it back to the target. Miner payloads carry no `rarityGroup`, so
//! rarity is derived from `data.level`.
//!
//! In-flight de-duplication is handled by moka's `try_get_with`: concurrent
//! requests for the same key coalesce into one upstream resolution.

use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use serde_json::Value;

use crate::api::{ApiClient, Credentials, ItemInfoResult};
use crate::logger::Logger;

pub const DEFAULT_TTL_MS: i64 = 10 * 60 * 1000;
pub const FAILURE_COOLDOWN_MS: i64 = 5 * 60 * 1000;
/// Upper bound for the resolution caches. A normal fleet is far below this, so
/// the cap only ever bites after a long session of previewing arbitrary ids.
const CACHE_MAX_ENTRIES: usize = 512;
pub const ITEM_TYPE_CANDIDATES: [&str; 5] = [
    "mutation_component",
    "miner",
    "rack",
    "battery",
    "utility_item",
];
const MINER_LEVEL_RARITIES: [&str; 6] = ["", "Uncommon", "Rare", "Epic", "Legendary", "Unreal"];

fn miner_color(rarity: &str) -> &'static str {
    match rarity {
        "Uncommon" => "2bd600",
        "Rare" => "2196f3",
        "Epic" => "a52bff",
        "Legendary" => "ffb800",
        "Unreal" => "ff3b6b",
        _ => "",
    }
}

fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn short_id(item_id: &str) -> String {
    let chars: Vec<char> = item_id.chars().collect();
    let tail: String = chars.iter().skip(chars.len().saturating_sub(6)).collect();
    tail.to_uppercase()
}

fn hex_color(value: &str) -> Option<String> {
    let digits = value.trim().trim_start_matches('#');
    if digits.len() == 6 && digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(digits.to_ascii_lowercase())
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemInfo {
    pub item_id: String,
    pub item_name: Option<String>,
    pub rarity: Option<String>,
    pub rarity_color: Option<String>,
    pub item_type: String,
    pub label: String,
}

/// Pure shape mapping, so it can be tested without HTTP.
pub fn normalize_item_info(data: &Value, item_id: &str, fallback_type: &str) -> ItemInfo {
    let raw_name = data
        .pointer("/name/en")
        .and_then(Value::as_str)
        .or_else(|| data.get("name").and_then(Value::as_str))
        .unwrap_or_default();

    let item_type = data
        .get("itemType")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_type)
        .to_ascii_lowercase();

    let mut raw_rarity = data
        .pointer("/rarityGroup/title/en")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut raw_color = data
        .pointer("/rarityGroup/baseHexColor")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_start_matches('#')
        .to_string();

    if raw_rarity.is_empty()
        && item_type == "miner"
        && let Some(level) = data.get("level").and_then(Value::as_f64)
    {
        let index = level as i64;
        if level.fract() == 0.0 && (0..MINER_LEVEL_RARITIES.len() as i64).contains(&index) {
            let derived = MINER_LEVEL_RARITIES[index as usize];
            if !derived.is_empty() {
                raw_rarity = derived.to_string();
                if hex_color(&raw_color).is_none() {
                    raw_color = miner_color(derived).to_string();
                }
            }
        }
    }

    let name = clean(raw_name);
    let rarity = clean(&raw_rarity);
    let composed = format!("{rarity} {name}")
        .trim()
        .chars()
        .take(48)
        .collect::<String>();
    let label = if composed.is_empty() {
        format!("ITEM {}", short_id(item_id))
    } else {
        composed
    };

    ItemInfo {
        item_id: item_id.to_string(),
        item_name: if name.is_empty() { None } else { Some(name) },
        rarity: if rarity.is_empty() {
            None
        } else {
            Some(rarity)
        },
        rarity_color: hex_color(&raw_color),
        item_type,
        label,
    }
}

fn is_type_miss(result: &ItemInfoResult) -> bool {
    let message = result
        .error
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    (result.status == 400 || result.status == 404)
        && (message.contains("item not found") || message.contains("item type is invalid"))
}

pub struct ItemInfoService {
    api: Arc<ApiClient>,
    logger: Arc<Logger>,
    cache: Cache<String, ItemInfo>,
    failed_at: Cache<String, ()>,
}

impl ItemInfoService {
    pub fn new(api: Arc<ApiClient>, logger: Arc<Logger>) -> Self {
        Self {
            api,
            logger,
            cache: Cache::builder()
                .max_capacity(CACHE_MAX_ENTRIES as u64)
                .time_to_live(Duration::from_millis(DEFAULT_TTL_MS as u64))
                .build(),
            failed_at: Cache::builder()
                .max_capacity(CACHE_MAX_ENTRIES as u64)
                .time_to_live(Duration::from_millis(FAILURE_COOLDOWN_MS as u64))
                .build(),
        }
    }

    fn cache_key(item_id: &str, item_type: &str) -> String {
        format!("{}:{item_type}", item_id.to_ascii_lowercase())
    }

    async fn load(
        &self,
        credentials: &Credentials,
        item_id: &str,
        item_type: &str,
        fresh: bool,
    ) -> Result<ItemInfo, String> {
        let key = Self::cache_key(item_id, item_type);
        if fresh {
            self.cache.invalidate(&key).await;
        } else if let Some(cached) = self.cache.get(&key).await {
            self.logger.debug_lazy("item-info", || {
                format!("cache hit {key} ({})", cached.label)
            });
            return Ok(cached);
        }

        self.logger.debug_lazy("item-info", || {
            format!("resolving {item_id} as {item_type} (fresh cache={fresh})")
        });
        let mut candidates = vec![item_type.to_string()];
        for candidate in ITEM_TYPE_CANDIDATES {
            if candidate != item_type {
                candidates.push(candidate.to_string());
            }
        }

        // Concurrent lookups of the same key share one upstream resolution.
        // `Err` is never cached, so a failed id can be retried after cooldown.
        let mut resolved_type: Option<String> = None;
        let slot = &mut resolved_type;
        let resolved = self
            .cache
            .try_get_with(key.clone(), async {
                let mut last_error = String::from("item-info returned no data");
                let mut last_status = 0u16;
                for candidate in &candidates {
                    self.logger
                        .debug_lazy("item-info", || format!("querying {item_id} as {candidate}"));
                    let result = self
                        .api
                        .fetch_item_info(credentials, item_id, candidate, "RLT")
                        .await;
                    if result.ok
                        && let Some(data) = result.data
                    {
                        let value = normalize_item_info(&data, item_id, candidate);
                        self.logger.debug_lazy("item-info", || {
                            format!(
                                "resolved {item_id} as {candidate} -> {} ({})",
                                value.label, value.item_type
                            )
                        });
                        *slot = Some(candidate.clone());
                        return Ok(value);
                    }
                    let type_miss = is_type_miss(&result);
                    last_error = result
                        .error
                        .clone()
                        .unwrap_or_else(|| "item-info returned no data".to_string());
                    last_status = result.status;
                    if type_miss {
                        self.logger.debug_lazy("item-info", || {
                            format!(
                                "type {candidate} missed for {item_id} (HTTP {}); trying the next candidate",
                                result.status
                            )
                        });
                    } else {
                        break;
                    }
                }
                Err((last_error, last_status))
            })
            .await;

        match resolved {
            Ok(value) => {
                if let Some(candidate) = resolved_type
                    && candidate != item_type
                {
                    self.cache
                        .insert(Self::cache_key(item_id, &candidate), value.clone())
                        .await;
                }
                self.failed_at.remove(&key).await;
                Ok(value)
            }
            Err(error) => {
                let (last_error, last_status) = error.as_ref().clone();
                if self.failed_at.get(&key).await.is_none() {
                    self.failed_at.insert(key.clone(), ()).await;
                    self.logger.warn(
                        "item-info",
                        format!("could not resolve {item_id}: {last_error} (HTTP {last_status})"),
                    );
                }
                Err(last_error)
            }
        }
    }

    /// Resolves an item without touching the target list (used by the UI when
    /// previewing an id that has not been added yet).
    /// Resolves an item without writing it to the store.
    pub async fn preview(
        &self,
        credentials: &Credentials,
        item_id: &str,
        item_type: &str,
    ) -> Result<ItemInfo, String> {
        self.load(credentials, item_id, item_type, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn composes_rarity_and_name() {
        let item = normalize_item_info(
            &json!({
                "name": { "en": "Uncommon Hashboard" },
                "itemType": "MUTATION_COMPONENT",
                "rarityGroup": { "title": { "en": "Rare" }, "baseHexColor": "#A52BFF" }
            }),
            "6319f7baa8ce530569ed16b9",
            "mutation_component",
        );
        assert_eq!(item.rarity.as_deref(), Some("Rare"));
        assert_eq!(item.item_name.as_deref(), Some("Uncommon Hashboard"));
        assert_eq!(item.label, "Rare Uncommon Hashboard");
        assert_eq!(item.rarity_color.as_deref(), Some("a52bff"));
        assert_eq!(item.item_type, "mutation_component");
    }

    #[test]
    fn derives_miner_rarity_from_level() {
        let item = normalize_item_info(
            &json!({ "name": { "en": "Hashboard" }, "itemType": "miner", "level": 3 }),
            "abc123",
            "miner",
        );
        assert_eq!(item.rarity.as_deref(), Some("Epic"));
        assert_eq!(item.rarity_color.as_deref(), Some("a52bff"));
        assert_eq!(item.label, "Epic Hashboard");
    }

    #[test]
    fn falls_back_to_short_item_id() {
        let item = normalize_item_info(&json!({}), "6571aa39ca8bfef34adcd9e9", "rack");
        assert_eq!(item.label, "ITEM DCD9E9");
        assert_eq!(item.item_name, None);
        assert_eq!(item.rarity_color, None);
    }

    #[test]
    fn normalises_whitespace_and_hex_prefix() {
        let item = normalize_item_info(
            &json!({
                "name": "  Uncommon   Hashboard ",
                "rarityGroup": { "title": { "en": "Uncommon" }, "baseHexColor": "2BD600" }
            }),
            "x1",
            "miner",
        );
        assert_eq!(item.item_name.as_deref(), Some("Uncommon Hashboard"));
        assert_eq!(item.rarity_color.as_deref(), Some("2bd600"));
    }

    #[test]
    fn type_miss_detection() {
        assert!(is_type_miss(&ItemInfoResult {
            ok: false,
            status: 404,
            error: Some("Item not found".into()),
            ..Default::default()
        }));
        assert!(is_type_miss(&ItemInfoResult {
            ok: false,
            status: 400,
            error: Some("item type is invalid".into()),
            ..Default::default()
        }));
        assert!(!is_type_miss(&ItemInfoResult {
            ok: false,
            status: 500,
            error: Some("item not found".into()),
            ..Default::default()
        }));
    }
}
