//! The in-memory store's tests.
//!
//! Deliberately the only place the shared contract is invoked for
//! [`MemoryUsageStore`]; the SQLite backend invokes the same macro in its own
//! test module, which is the point of it being a macro.

use super::{MemoryUsageStore, UsageStore};

usage_store_contract!(memory_store_contract, MemoryUsageStore::new());

#[cfg(test)]
mod extra {
    use super::*;
    use crate::frecency::{Frecency, Timestamp};
    use crate::store::{LaunchRecord, UsageStoreExt};
    use crate::Source;

    #[test]
    fn with_history_builds_a_fixture_without_a_launch() {
        let store = MemoryUsageStore::with_history([(
            "app:note".to_owned(),
            Frecency::new(3, Some(Timestamp::from_unix_seconds(10))),
        )]);
        assert_eq!(store.len().expect("len"), 1);
        assert_eq!(
            store.frecency("app:note").expect("frecency"),
            Some(Frecency::new(3, Some(Timestamp::from_unix_seconds(10))))
        );
    }

    #[test]
    fn a_launch_merges_into_preloaded_history() {
        let mut store = MemoryUsageStore::with_history([(
            "app:note".to_owned(),
            Frecency::new(2, Some(Timestamp::from_unix_seconds(10))),
        )]);
        store
            .record_launch(LaunchRecord {
                item_id: "app:note",
                source: Source::Application,
                title: "Notepad",
                at: Timestamp::from_unix_seconds(20),
            })
            .expect("launch");
        assert_eq!(
            store.frecency("app:note").expect("frecency"),
            Some(Frecency::new(3, Some(Timestamp::from_unix_seconds(20))))
        );
    }

    #[test]
    fn an_empty_id_is_stored_rather_than_rejected() {
        // The trait does not police ids; the provider contract does. The store's
        // job is not to be the second place that rule lives, and a silent
        // drop here would be indistinguishable from a lost launch.
        let mut store = MemoryUsageStore::new();
        store
            .record_launch(LaunchRecord {
                item_id: "",
                source: Source::Unknown,
                title: "",
                at: Timestamp::EPOCH,
            })
            .expect("launch");
        assert_eq!(store.len().expect("len"), 1);
    }

    #[test]
    fn frecency_map_is_ordered_and_overlays_onto_items() {
        let store = MemoryUsageStore::with_history([
            (
                "app:z".to_owned(),
                Frecency::new(1, Some(Timestamp::from_unix_seconds(1))),
            ),
            (
                "app:a".to_owned(),
                Frecency::new(2, Some(Timestamp::from_unix_seconds(2))),
            ),
        ]);
        let map = store.frecency_map().expect("map");
        assert_eq!(
            map.keys().cloned().collect::<Vec<_>>(),
            vec!["app:a".to_owned(), "app:z".to_owned()],
            "a BTreeMap keeps the overlay diffable"
        );

        let now = Timestamp::from_unix_seconds(2);
        let item = crate::ResultItem::new(
            "app:a",
            "Alpha",
            Source::Application,
            crate::LaunchTarget::Uri("x".into()),
        )
        .with_frecency(map["app:a"]);
        let ranked = crate::rank_at(
            crate::RankingPolicy::DEFAULT,
            "alpha",
            now,
            std::iter::once(&item),
        );
        assert_eq!(ranked.len(), 1);
    }
}
