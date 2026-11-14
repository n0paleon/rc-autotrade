use std::sync::Arc;

use crate::logger::Logger;
use crate::model::Mode;
use crate::store::Store;
use crate::validation;

use super::{store, target, temp_dir};

#[test]
fn generates_and_verifies_a_worker_token() {
    let mut store = store("token");
    let token = store.ensure_worker_token("").unwrap().expect("generated");
    assert!(store.verify_worker_token(&token));
    assert!(!store.verify_worker_token("wrong-token"));
    assert!(store.ensure_worker_token("").unwrap().is_none());
}

#[test]
fn pins_the_token_from_the_environment() {
    let mut store = store("env-token");
    let pinned = "super-secret-worker-key";
    assert!(store.ensure_worker_token(pinned).unwrap().is_none());
    assert!(store.verify_worker_token(pinned));
}

#[test]
fn persists_targets_and_settings_across_reopen() {
    let dir = temp_dir("persist");
    let logger = Arc::new(Logger::new("error"));
    {
        let mut store = Store::open(dir.clone(), logger.clone()).unwrap();
        store.upsert_config(target("6319f7baa8ce530569ed16b9"), None);
        let patch = validation::SettingsPatch {
            mode: Some(Mode::Off),
            total_price_markup_pct: Some(7.5),
            ..Default::default()
        };
        store.patch_settings(patch).unwrap();
        store.close();
    }
    let store = Store::open(dir.clone(), logger).unwrap();
    assert_eq!(store.state.configs.len(), 1);
    assert_eq!(store.state.configs[0].item_id, "6319f7baa8ce530569ed16b9");
    assert_eq!(store.state.settings.mode, Mode::Off);
    assert_eq!(store.state.settings.total_price_markup_pct, 7.5);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The legacy columns are mirrors, not state: whatever mode is written must
/// leave a pair of flags that agrees with it on disk.
#[test]
fn the_config_index_tracks_every_mutation() {
    let mut store = store("config-index");
    store.upsert_config(target("AAA111"), None);
    store.upsert_config(target("bbb222"), None);

    // Mixed case in the caller still resolves, without an allocation.
    assert_eq!(store.find_config("aaa111").unwrap().item_id, "AAA111");
    assert_eq!(store.find_config("BBB222").unwrap().item_id, "bbb222");
    assert!(store.find_config("missing").is_none());

    // An edit can change the stored case; the index must follow.
    store.upsert_config(target("aaa111"), Some("AAA111"));
    assert_eq!(store.find_config("aaa111").unwrap().item_id, "aaa111");

    // Deleting shifts every later position.
    assert!(store.remove_config("aaa111"));
    assert!(store.find_config("aaa111").is_none());
    assert!(store.find_config("bbb222").is_some());

    // A bulk replace rewrites the whole set.
    store.set_configs(vec![target("ccc333")]).unwrap();
    assert!(store.find_config("bbb222").is_none());
    assert!(store.find_config("ccc333").is_some());
}

#[test]
fn mode_and_the_legacy_mirror_columns_never_disagree() {
    let dir = temp_dir("mirror");
    let logger = Arc::new(Logger::new("error"));
    let mut store = Store::open(dir.clone(), logger.clone()).unwrap();
    for (mode, dry_run, paused) in [(Mode::Off, 1, 1), (Mode::Shadow, 1, 0), (Mode::Live, 0, 0)] {
        store
            .patch_settings(validation::SettingsPatch {
                mode: Some(mode),
                ..Default::default()
            })
            .unwrap();
        let row: (String, i64, i64) = store
            .db
            .query_row("SELECT mode, dry_run, paused FROM settings", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(row, (mode.as_str().to_string(), dry_run, paused));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
