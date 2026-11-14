use std::sync::Arc;

use crate::api::Credentials;
use crate::item_info::ItemInfoService;
use crate::logger::Logger;
use crate::store::{Store, now_ms};

/// How long one monitor response may spend resolving names upstream.
pub(super) const RESOLVE_BUDGET_MS: u64 = 2_500;
/// Names resolved per response. A cold catalogue fills over a few refreshes
/// instead of turning the first render into one request per row.
pub(super) const RESOLVE_BATCH: usize = 8;

/// One id the response still has to name.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingName {
    pub item_id: String,
    pub item_type: String,
}

/// Which rows on this page still need an `item-info` lookup.
///
/// Pure and synchronous so it runs under the store lock: the caller then does
/// the slow part ([`resolve_pending`]) with no lock held and applies the results
/// in one more short critical section.
///
/// Rows are ordered by fewest attempts first. Without that ordering a cold
/// catalogue with more unnamed rows than `RESOLVE_BATCH` would retry the same
/// first eight on every refresh forever — exactly what a batch of unnameable ids
/// (a delisted product, or the simulator's random ids) would cause while they
/// kept a transport error from being cached.
pub fn pending_names(store: &Store, rows: &[crate::model::ItemActivityRow]) -> Vec<PendingName> {
    let mut pending: Vec<(i64, i64, PendingName)> = Vec::new();
    for row in rows {
        if !row.label.is_empty() {
            continue;
        }
        let key = row.item_id.to_ascii_lowercase();
        let entry = store.catalog_entry(&key);
        if let Some(entry) = entry
            && !entry.needs_resolution()
        {
            // Already asked for, and upstream did not know it (or it has no
            // name). Keep the fallback label instead of retrying forever.
            continue;
        }
        let item_type = if row.item_type.is_empty() {
            "mutation_component".to_string()
        } else {
            row.item_type.clone()
        };
        // Fewest attempts first, then the busiest row; the tiebreak keeps the
        // order stable between refreshes.
        let attempts = entry.map(|entry| entry.attempts).unwrap_or(0);
        let events = row.events;
        pending.push((
            attempts,
            -events,
            PendingName {
                item_id: key,
                item_type,
            },
        ));
    }
    pending.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    pending
        .into_iter()
        .take(RESOLVE_BATCH)
        .map(|(_, _, entry)| entry)
        .collect()
}

/// One lookup outcome, ready to be applied to the store.
#[derive(Debug, Clone)]
pub struct ResolvedName {
    pub item_id: String,
    pub resolved_at: i64,
    /// `Ok` carries the resolved name/rarity; `Err` carries the upstream
    /// message and whether it was a definitive miss (delisted / unknown type)
    /// rather than a transport failure.
    pub outcome: Result<Resolved, String>,
    pub definitive_miss: bool,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub label: String,
    pub item_type: String,
    pub rarity: Option<String>,
    pub rarity_color: Option<String>,
}

/// Names the pending ids, best-effort and with a strict budget.
///
/// Never holds a lock: the caller passes the ids in and applies the returned
/// outcomes afterwards. A timeout, a rejected session or an item the marketplace
/// no longer lists is reported, not raised — the monitor keeps rendering the id
/// and the next refresh retries a transport failure.
pub async fn resolve_pending(
    item_info: &Arc<ItemInfoService>,
    credentials: &Credentials,
    logger: &Arc<Logger>,
    pending: Vec<PendingName>,
) -> Vec<ResolvedName> {
    let mut resolved = Vec::with_capacity(pending.len());
    let started = std::time::Instant::now();
    for (index, entry) in pending.into_iter().enumerate() {
        if started.elapsed().as_millis() as u64 > RESOLVE_BUDGET_MS {
            logger.debug(
                "item-activity",
                format!(
                    "name resolution stopped after {index} item(s); the rest waits for the next refresh"
                ),
            );
            break;
        }
        let outcome = item_info
            .preview(credentials, &entry.item_id, &entry.item_type)
            .await;
        let resolved_at = now_ms();
        match outcome {
            Ok(item) => resolved.push(ResolvedName {
                item_id: entry.item_id,
                resolved_at,
                outcome: Ok(Resolved {
                    label: item.label,
                    item_type: item.item_type,
                    rarity: item.rarity,
                    rarity_color: item.rarity_color,
                }),
                definitive_miss: false,
            }),
            Err(error) => {
                let message = error.to_ascii_lowercase();
                let definitive_miss = message.contains("not found") || message.contains("invalid");
                if definitive_miss {
                    logger.debug(
                        "item-activity",
                        format!("item-info has no name for {}: {error}", entry.item_id),
                    );
                } else {
                    logger.warn(
                        "item-activity",
                        format!("could not name {}: {error}", entry.item_id),
                    );
                }
                resolved.push(ResolvedName {
                    item_id: entry.item_id,
                    resolved_at,
                    outcome: Err(error),
                    definitive_miss,
                });
            }
        }
    }
    resolved
}

/// Writes resolved names to the catalogue and back onto the page.
///
/// Returns how many rows are still nameless, so the response can say so without
/// a second scan of the catalogue.
pub fn apply_resolved(
    store: &mut Store,
    rows: &mut [crate::model::ItemActivityRow],
    resolved: Vec<ResolvedName>,
) -> usize {
    for entry in resolved {
        match entry.outcome {
            Ok(item) => {
                store.apply_catalog_item(
                    &entry.item_id,
                    &item.label,
                    &item.item_type,
                    item.rarity.as_deref(),
                    item.rarity_color.as_deref(),
                    entry.resolved_at,
                );
                for row in rows.iter_mut() {
                    if row.item_id.eq_ignore_ascii_case(&entry.item_id) {
                        row.label = item.label.clone();
                        row.item_type = item.item_type.clone();
                        row.rarity = item.rarity.clone();
                        row.rarity_color = item.rarity_color.clone();
                    }
                }
            }
            Err(_) if entry.definitive_miss => {
                // A miss is remembered so a delisted id is not retried on every
                // refresh; a transport failure is not, so a flaky network
                // recovers on its own.
                store.mark_catalog_resolved(&entry.item_id, entry.resolved_at);
            }
            Err(_) => {
                // Not final, but it still gets an attempt so the next batch
                // rotates to ids that have not been tried yet.
                store.note_catalog_attempts(&entry.item_id, entry.resolved_at);
            }
        }
    }
    rows.iter().filter(|row| row.label.is_empty()).count()
}
