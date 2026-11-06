//! Item label/rarity resolver, ported from `src/worker/item-info.js`.
//!
//! Calls the marketplace `item-info` endpoint, composes a `Rarity Name` label
//! and writes it back to the target. Miner payloads carry no `rarityGroup`, so
//! rarity is derived from `data.level` the same way the JS worker does.
//!
//! Difference from the JS version: in-flight de-duplication is not implemented
//! (the wire path awaits one enrichment at a time and the HTTP endpoint is
//! user-triggered), so two concurrent requests can both hit upstream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::api::{ApiClient, Credentials, ItemInfoResult};
use crate::logger::Logger;
use crate::store::Store;

pub const DEFAULT_TTL_MS: i64 = 10 * 60 * 1000;
pub const FAILURE_COOLDOWN_MS: i64 = 5 * 60 * 1000;
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
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

    if raw_rarity.is_empty() && item_type == "miner" {
        if let Some(level) = data.get("level").and_then(Value::as_f64) {
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

#[derive(Debug, Clone)]
struct Cached {
    at: i64,
    value: ItemInfo,
}

pub struct ItemInfoService {
    api: Arc<ApiClient>,
    logger: Arc<Logger>,
    ttl_ms: i64,
    cache: Mutex<HashMap<String, Cached>>,
    failed_at: Mutex<HashMap<String, i64>>,
}

impl ItemInfoService {
    pub fn new(api: Arc<ApiClient>, logger: Arc<Logger>) -> Self {
        Self {
            api,
            logger,
            ttl_ms: DEFAULT_TTL_MS,
            cache: Mutex::new(HashMap::new()),
            failed_at: Mutex::new(HashMap::new()),
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
        if !fresh {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = cache.get(&key) {
                if now_ms() - cached.at < self.ttl_ms {
                    return Ok(cached.value.clone());
                }
            }
        }

        let mut candidates = vec![item_type.to_string()];
        for candidate in ITEM_TYPE_CANDIDATES {
            if candidate != item_type {
                candidates.push(candidate.to_string());
            }
        }

        let mut last_error = String::from("item-info returned no data");
        let mut last_status = 0u16;
        for candidate in &candidates {
            let result = self
                .api
                .fetch_item_info(credentials, item_id, candidate, "RLT")
                .await;
            if result.ok {
                if let Some(data) = result.data {
                    let value = normalize_item_info(&data, item_id, candidate);
                    let at = now_ms();
                    let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                    cache.insert(
                        key.clone(),
                        Cached {
                            at,
                            value: value.clone(),
                        },
                    );
                    cache.insert(
                        Self::cache_key(item_id, candidate),
                        Cached {
                            at,
                            value: value.clone(),
                        },
                    );
                    drop(cache);
                    self.failed_at
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&key);
                    return Ok(value);
                }
            }
            let type_miss = is_type_miss(&result);
            last_error = result
                .error
                .clone()
                .unwrap_or_else(|| "item-info returned no data".to_string());
            last_status = result.status;
            if !type_miss {
                break;
            }
        }

        let last = {
            let failed = self.failed_at.lock().unwrap_or_else(|e| e.into_inner());
            failed.get(&key).copied().unwrap_or(0)
        };
        if now_ms() - last > FAILURE_COOLDOWN_MS {
            self.failed_at
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, now_ms());
            self.logger.warn(
                "item-info",
                format!("could not resolve {item_id}: {last_error} (HTTP {last_status})"),
            );
        }
        Err(last_error)
    }

    /// Resolves an item without touching the target list (used by the UI when
    /// previewing an id that has not been added yet).
    pub async fn preview(
        &self,
        credentials: &Credentials,
        item_id: &str,
        item_type: &str,
    ) -> Result<ItemInfo, String> {
        self.load(credentials, item_id, item_type, false).await
    }

    /// Resolves an item and writes `label`/`itemType`/`rarityColor` to the store.
    pub async fn enrich(
        &self,
        store: &mut Store,
        credentials: &Credentials,
        item_id: &str,
        item_type: &str,
        fresh: bool,
    ) -> Result<ItemInfo, String> {
        let value = self.load(credentials, item_id, item_type, fresh).await?;
        let changed = store.apply_item_info(
            item_id,
            &value.label,
            &value.item_type,
            value.rarity_color.as_deref(),
        );
        if changed {
            self.logger
                .info("item-info", format!("{item_id} => {}", value.label));
        }
        Ok(value)
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
    fn type_miss_detection_matches_the_javascript_rules() {
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
