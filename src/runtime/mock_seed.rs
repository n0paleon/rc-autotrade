//! Demo-mode seeding for the simulator.

use std::sync::Arc;

use crate::lock::lock;
use crate::logger::Logger;
use crate::mock::MockItem;
use crate::model::Target;
use crate::store::Store;

use super::AppState;

/// Seeds the demo targets and a synthetic session when `MOCK=1` and the store
/// is empty (the simulator must be usable with zero setup).
pub fn seed_mock_state(store: &mut Store, logger: &Arc<Logger>) -> Result<(), String> {
    if store.state.configs.is_empty() {
        let now = crate::store::now_ms();
        let targets = vec![
            Target {
                item_id: "6571aa39ca8bfef34adcd9e9".into(),
                label: "NEON SHARD".into(),
                item_type: "mutation_component".into(),
                rarity_color: None,
                target_price: 0.1,
                desired_qty: 10,
                high_stock_threshold: 20,
                min_acceptable_qty: 1,
                allow_partial_order: true,
                active: true,
                created_at: Some(now),
                updated_at: Some(now),
            },
            Target {
                item_id: "66b0a1c2d3e4f5061728394a".into(),
                label: "PIXEL CORE".into(),
                item_type: "mutation_component".into(),
                rarity_color: None,
                target_price: 0.05,
                desired_qty: 25,
                high_stock_threshold: 60,
                min_acceptable_qty: 5,
                allow_partial_order: true,
                active: true,
                created_at: Some(now),
                updated_at: Some(now),
            },
        ];
        store.set_configs(targets)?;
        logger.info("mock", "seeded demo targets");
    }
    if store.state.session.access_token.is_empty() {
        let patch = crate::validation::SessionPatch {
            access_token: Some(format!(
                "mock.{}.{}",
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4()
            )),
            refresh_token: Some("mock-refresh".into()),
            source: Some("mock".into()),
            ..Default::default()
        };
        store.apply_session_patch(patch)?;
        logger.info("mock", "seeded mock session");
    }
    seed_mock_catalog(store, logger);
    Ok(())
}

/// Names the simulator's ambient items up front.
///
/// The real worker resolves names from `item-info`; `MOCK=1` has no session to
/// call it with, so without this the item monitor would show six 24-character
/// ids and no product names. Rarity is derived from the level the same way the
/// miner resolver does it.
fn seed_mock_catalog(store: &mut Store, logger: &Arc<Logger>) {
    let now = crate::store::now_ms();
    let mut seeded = 0usize;
    for (item_id, name, item_type, level) in crate::mock::AMBIENT_ITEMS {
        if store.catalog_entry(item_id).is_some() {
            continue;
        }
        let rarity = match level {
            1 => Some(("Uncommon", "2bd600")),
            2 => Some(("Rare", "2196f3")),
            3 => Some(("Epic", "a52bff")),
            4 => Some(("Legendary", "ffb800")),
            5 => Some(("Unreal", "ff3b6b")),
            _ => None,
        };
        let label = match rarity {
            Some((rarity, _)) => format!("{rarity} {name}"),
            None => name.to_string(),
        };
        store.apply_catalog_item(
            item_id,
            &label,
            item_type,
            rarity.map(|(rarity, _)| rarity),
            rarity.map(|(_, color)| color),
            now,
        );
        seeded += 1;
    }
    for target in store.state.configs.clone() {
        if store.catalog_entry(&target.item_id).is_some() {
            continue;
        }
        store.apply_catalog_item(
            &target.item_id,
            &target.label,
            &target.item_type,
            None,
            target.rarity_color.as_deref(),
            now,
        );
        seeded += 1;
    }
    if seeded > 0 {
        logger.info("mock", format!("seeded {seeded} catalogue item(s)"));
    }
}

pub(super) fn mock_items(state: &Arc<AppState>) -> Vec<MockItem> {
    let store = lock(&state.store);
    store
        .state
        .configs
        .iter()
        .map(|target| MockItem {
            item_id: target.item_id.clone(),
            label: target.label.clone(),
            item_type: target.item_type.clone(),
            target_price: target.target_price,
            desired_qty: target.desired_qty,
            high_stock_threshold: target.high_stock_threshold,
            min_acceptable_qty: target.min_acceptable_qty,
            active: target.active,
        })
        .collect()
}
