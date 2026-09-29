//! Launch history: the trait, the in-memory implementation, and the SQLite one.
//!
//! The store is the only state orca has about its user, so the interface is
//! deliberately tiny — record a launch, read a history, forget a thing. It is a
//! trait so the UI can be written and tested against
//! [`MemoryUsageStore`], which has no database, no file, and no schema, and the
//! same contract is run against [`sqlite::SqliteUsageStore`] so the two cannot
//! quietly diverge.
//!
//! The two halves of the split:
//!
//! * [`store`] owns the *contract* and the cheap implementation. The contract
//!   tests in [`store::contract_tests`] are a macro, not a function, so both
//!   implementations are held to identical assertions rather than to
//!   "roughly the same" ones.
//! * [`sqlite`] owns persistence. It opens a path the caller chose, keeps
//!   `bundled` SQLite so there is no system dependency, and migrates forward on
//!   open so an old database is never silently read with today's schema.
//!
//! Neither half reads a clock. Every record carries the timestamp it was
//! launched at, which is what makes the decay in [`crate::frecency`] testable.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::frecency::{Frecency, Timestamp};
use crate::model::Source;

pub mod sqlite;

/// One row of launch history, as read back from the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEntry {
    /// The provider-scoped id this history belongs to.
    pub item_id: String,
    /// Which kind of thing it was, as recorded at launch time.
    pub source: Source,
    /// The title as recorded at launch time.
    ///
    /// Kept so a history row can still say what it referred to after the
    /// provider stops producing that item — otherwise a "recently used" row
    /// with nothing to show next to it.
    pub title: String,
    /// Counts and timestamps.
    pub frecency: Frecency,
}

/// What to record when a result is activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchRecord<'a> {
    /// The id to credit.
    pub item_id: &'a str,
    /// Which kind of result it was.
    pub source: Source,
    /// What it was called, for the recents list.
    pub title: &'a str,
    /// When it was launched. Supplied, not read: the clock belongs to the
    /// caller.
    pub at: Timestamp,
}

/// Read and write launch history.
///
/// Every method is fallible rather than panicking, and none of them silently
/// swallow an error. `&mut self` on the writes is what lets a real
/// implementation own a connection without interior mutability.
pub trait UsageStore {
    /// Credits one launch, merging into any existing history for `item_id`.
    fn record_launch(&mut self, record: LaunchRecord<'_>) -> Result<(), StoreError>;

    /// The history for one id, or `None` if it has never been launched.
    fn frecency(&self, item_id: &str) -> Result<Option<Frecency>, StoreError>;

    /// Every recorded history, ordered by id for a deterministic result.
    fn entries(&self) -> Result<Vec<UsageEntry>, StoreError>;

    /// The most recently launched entries, newest first, at most `limit` of
    /// them. Ties break on id, so the list is deterministic.
    fn most_recent(&self, limit: usize) -> Result<Vec<UsageEntry>, StoreError>;

    /// Drops one id's history. `Ok(false)` means there was nothing to drop.
    fn forget(&mut self, item_id: &str) -> Result<bool, StoreError>;

    /// Drops every history. Intended for "reset my data" in settings.
    fn clear(&mut self) -> Result<(), StoreError>;

    /// How many ids are recorded.
    fn len(&self) -> Result<usize, StoreError>;

    /// Whether nothing is recorded.
    fn is_empty(&self) -> Result<bool, StoreError> {
        Ok(self.len()? == 0)
    }
}

/// Extension methods shared by every [`UsageStore`], so callers are not stuck
/// reading the same three lines.
pub trait UsageStoreExt: UsageStore {
    /// A `HashMap`-free id -> history view, ready to be stamped onto
    /// `ResultItem`s before ranking.
    ///
    /// Returns a `BTreeMap` rather than a `HashMap` so the result is ordered
    /// and therefore diffable in a test.
    fn frecency_map(&self) -> Result<BTreeMap<String, Frecency>, StoreError> {
        Ok(self
            .entries()?
            .into_iter()
            .map(|entry| (entry.item_id, entry.frecency))
            .collect())
    }
}

impl<T: UsageStore + ?Sized> UsageStoreExt for T {}

/// Everything a store can fail with.
///
/// Deliberately has no `NotImplemented` variant: a store either works or does
/// not, and a seam that pretends is worse than no seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The backing store could not be opened.
    Open {
        /// The path or other locator that failed, when there is one.
        path: Option<String>,
        /// What the backend said.
        detail: String,
    },
    /// A schema migration failed. The store is left unusable rather than
    /// half-migrated: the migrations run in one transaction.
    Migration {
        /// The migration's version, as a two-part label like `"0001_usage"`.
        version: String,
        /// What went wrong.
        detail: String,
    },
    /// A statement failed.
    Query {
        /// The statement, or a short name for it.
        context: &'static str,
        /// What the backend said.
        detail: String,
    },
    /// A row read back in a shape the store cannot represent — for example a
    /// `source` written by a newer build.
    Corrupt {
        /// The row's primary key.
        item_id: String,
        /// What was wrong with it.
        detail: String,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Open { path, detail } => match path {
                Some(path) => write!(f, "could not open the history store at {path}: {detail}"),
                None => write!(f, "could not open the history store: {detail}"),
            },
            StoreError::Migration { version, detail } => {
                write!(f, "history schema migration {version} failed: {detail}")
            }
            StoreError::Query { context, detail } => {
                write!(f, "history query ({context}) failed: {detail}")
            }
            StoreError::Corrupt { item_id, detail } => {
                write!(f, "history row for {item_id:?} is unusable: {detail}")
            }
        }
    }
}

impl Error for StoreError {}

/// A [`UsageStore`] held entirely in memory.
///
/// This is the implementation the UI is written against: no database file, no
/// schema, no locks, and cloning it is cheap. It is also a perfectly good
/// production store for a session-scoped recents list, so it is not a test-only
/// type that has to be kept working.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryUsageStore {
    entries: BTreeMap<String, UsageEntry>,
}

impl MemoryUsageStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> MemoryUsageStore {
        MemoryUsageStore::default()
    }

    /// A store preloaded with one launch per entry, for fixtures and demos.
    #[must_use]
    pub fn with_history(history: impl IntoIterator<Item = (String, Frecency)>) -> MemoryUsageStore {
        let mut store = MemoryUsageStore::new();
        for (item_id, frecency) in history {
            let entry = UsageEntry {
                source: Source::Unknown,
                title: item_id.clone(),
                frecency,
                item_id: item_id.clone(),
            };
            store.entries.insert(item_id, entry);
        }
        store
    }
}

impl UsageStore for MemoryUsageStore {
    fn record_launch(&mut self, record: LaunchRecord<'_>) -> Result<(), StoreError> {
        // `BTreeMap::entry` gives the merge in one lookup, and — unlike
        // get_mut-then-insert — cannot leave the map in a half-updated state if
        // a future edit adds a fallible step.
        let entry = self
            .entries
            .entry(record.item_id.to_owned())
            .or_insert_with(|| UsageEntry {
                item_id: record.item_id.to_owned(),
                source: record.source,
                title: record.title.to_owned(),
                frecency: Frecency::NEVER,
            });
        entry.frecency = entry.frecency.launched_at(record.at);
        // Later launches win on the descriptive fields too: a renamed app
        // should stop showing its old name in the recents list.
        entry.source = record.source;
        entry.title = record.title.to_owned();
        Ok(())
    }

    fn frecency(&self, item_id: &str) -> Result<Option<Frecency>, StoreError> {
        Ok(self.entries.get(item_id).map(|entry| entry.frecency))
    }

    fn entries(&self) -> Result<Vec<UsageEntry>, StoreError> {
        Ok(self.entries.values().cloned().collect())
    }

    fn most_recent(&self, limit: usize) -> Result<Vec<UsageEntry>, StoreError> {
        let mut recent: Vec<UsageEntry> = self
            .entries
            .values()
            .filter(|entry| entry.frecency.last_launch().is_some())
            .cloned()
            .collect();
        recent.sort_by(|a, b| {
            b.frecency
                .last_launch()
                .cmp(&a.frecency.last_launch())
                .then_with(|| a.item_id.cmp(&b.item_id))
        });
        recent.truncate(limit);
        Ok(recent)
    }

    fn forget(&mut self, item_id: &str) -> Result<bool, StoreError> {
        Ok(self.entries.remove(item_id).is_some())
    }

    fn clear(&mut self) -> Result<(), StoreError> {
        self.entries.clear();
        Ok(())
    }

    fn len(&self) -> Result<usize, StoreError> {
        Ok(self.entries.len())
    }
}

/// The assertions every [`UsageStore`] must satisfy.
///
/// A macro rather than a function because the contract has to be *run* against
/// each implementation: a trait test that only covers the in-memory store
/// documents a wish instead of a guarantee. Each backend's test module invokes
/// it with its own constructor.
#[cfg(test)]
macro_rules! usage_store_contract {
    ($name:ident, $make:expr) => {
        #[test]
        fn a_new_store_is_empty() {
            let store = $make;
            assert!(store.is_empty().expect("is_empty"));
            assert_eq!(store.len().expect("len"), 0);
            assert_eq!(store.entries().expect("entries").len(), 0);
            assert_eq!(
                store.frecency("nothing").expect("frecency"),
                None,
                "an unknown id must be None, not a zeroed Frecency"
            );
        }

        #[test]
        fn recording_a_launch_creates_then_accumulates() {
            let mut store = $make;
            let at = crate::frecency::Timestamp::from_unix_seconds(1_000);

            store
                .record_launch(crate::store::LaunchRecord {
                    item_id: "app:note",
                    source: crate::model::Source::Application,
                    title: "Notepad",
                    at,
                })
                .expect("first launch");
            assert_eq!(store.len().expect("len"), 1);
            assert_eq!(
                store.frecency("app:note").expect("frecency"),
                Some(crate::frecency::Frecency::new(1, Some(at)))
            );

            store
                .record_launch(crate::store::LaunchRecord {
                    item_id: "app:note",
                    source: crate::model::Source::Application,
                    title: "Notepad",
                    at: at.saturating_add_secs(60),
                })
                .expect("second launch");
            assert_eq!(store.len().expect("len"), 1, "must merge, not append");
            assert_eq!(
                store.frecency("app:note").expect("frecency"),
                Some(crate::frecency::Frecency::new(
                    2,
                    Some(at.saturating_add_secs(60))
                ))
            );
        }

        #[test]
        fn entries_carry_the_descriptive_fields() {
            let mut store = $make;
            let at = crate::frecency::Timestamp::from_unix_seconds(1_000);
            store
                .record_launch(crate::store::LaunchRecord {
                    item_id: "app:note",
                    source: crate::model::Source::Application,
                    title: "Notepad",
                    at,
                })
                .expect("launch");
            let entries = store.entries().expect("entries");
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].item_id, "app:note");
            assert_eq!(entries[0].source, crate::model::Source::Application);
            assert_eq!(entries[0].title, "Notepad");
        }

        #[test]
        fn a_renamed_item_takes_the_new_title_and_source() {
            let mut store = $make;
            let at = crate::frecency::Timestamp::from_unix_seconds(1_000);
            for (title, source) in [
                ("Old Name", crate::model::Source::Application),
                ("New Name", crate::model::Source::Command),
            ] {
                store
                    .record_launch(crate::store::LaunchRecord {
                        item_id: "app:x",
                        source,
                        title,
                        at: at.saturating_add_secs(1),
                    })
                    .expect("launch");
            }
            let entry = &store.entries().expect("entries")[0];
            assert_eq!(entry.title, "New Name");
            assert_eq!(entry.source, crate::model::Source::Command);
            assert_eq!(entry.frecency.launches(), 2, "the count must not reset");
        }

        #[test]
        fn most_recent_is_newest_first_and_respects_the_limit() {
            let mut store = $make;
            let base = crate::frecency::Timestamp::from_unix_seconds(1_000);
            for (index, id) in ["a", "b", "c", "d"].iter().enumerate() {
                store
                    .record_launch(crate::store::LaunchRecord {
                        item_id: id,
                        source: crate::model::Source::Application,
                        title: id,
                        at: base.saturating_add_secs(index as i64 * 100),
                    })
                    .expect("launch");
            }
            let recent = store.most_recent(2).expect("most_recent");
            assert_eq!(
                recent
                    .iter()
                    .map(|e| e.item_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["d", "c"]
            );
            assert_eq!(store.most_recent(0).expect("zero").len(), 0);
            assert_eq!(store.most_recent(99).expect("over-long").len(), 4);
        }

        #[test]
        fn most_recent_breaks_ties_on_id_for_determinism() {
            let mut store = $make;
            let at = crate::frecency::Timestamp::from_unix_seconds(1_000);
            for id in ["z", "m", "a"] {
                store
                    .record_launch(crate::store::LaunchRecord {
                        item_id: id,
                        source: crate::model::Source::Application,
                        title: id,
                        at,
                    })
                    .expect("launch");
            }
            let recent = store.most_recent(99).expect("most_recent");
            assert_eq!(
                recent
                    .iter()
                    .map(|e| e.item_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["a", "m", "z"],
                "identical timestamps must still produce one fixed order"
            );
        }

        #[test]
        fn forget_reports_whether_there_was_anything_to_forget() {
            let mut store = $make;
            assert!(!store.forget("ghost").expect("forget"));
            store
                .record_launch(crate::store::LaunchRecord {
                    item_id: "app:note",
                    source: crate::model::Source::Application,
                    title: "Notepad",
                    at: crate::frecency::Timestamp::from_unix_seconds(1),
                })
                .expect("launch");
            assert!(store.forget("app:note").expect("forget"));
            assert!(store.is_empty().expect("is_empty"));
            assert_eq!(store.frecency("app:note").expect("frecency"), None);
        }

        #[test]
        fn clear_empties_everything_and_is_idempotent() {
            let mut store = $make;
            for id in ["a", "b"] {
                store
                    .record_launch(crate::store::LaunchRecord {
                        item_id: id,
                        source: crate::model::Source::Application,
                        title: id,
                        at: crate::frecency::Timestamp::from_unix_seconds(1),
                    })
                    .expect("launch");
            }
            store.clear().expect("clear");
            assert!(store.is_empty().expect("is_empty"));
            store.clear().expect("clear again");
            assert!(store.is_empty().expect("is_empty"));
        }

        #[test]
        fn a_moved_store_keeps_its_history() {
            // Every store must be movable without losing data — `&mut self` on
            // the writes means a store is not behind a shared reference, and a
            // provider that owns one can hand it up the stack.
            let mut store = $make;
            let at = crate::frecency::Timestamp::from_unix_seconds(4_242);
            store
                .record_launch(crate::store::LaunchRecord {
                    item_id: "app:note",
                    source: crate::model::Source::Application,
                    title: "Notepad",
                    at,
                })
                .expect("launch");

            let moved = store;
            assert_eq!(
                moved.frecency("app:note").expect("frecency"),
                Some(crate::frecency::Frecency::new(1, Some(at)))
            );
        }

        #[test]
        fn a_fresh_store_starts_empty_even_when_one_already_existed() {
            // The counterpart to the above, and the reason persistence across
            // *processes* is a backend-specific test rather than part of the
            // contract: a contract the in-memory store cannot satisfy is not a
            // contract.
            let mut first = $make;
            first
                .record_launch(crate::store::LaunchRecord {
                    item_id: "app:note",
                    source: crate::model::Source::Application,
                    title: "Notepad",
                    at: crate::frecency::Timestamp::from_unix_seconds(1),
                })
                .expect("launch");
            assert_eq!(first.len().expect("len"), 1);

            let second = $make;
            assert!(second.is_empty().expect("is_empty"));
        }

        #[test]
        fn unicode_ids_and_titles_round_trip() {
            let mut store = $make;
            let at = crate::frecency::Timestamp::from_unix_seconds(1);
            for id in ["app:日本", "app:café", "app:🎉"] {
                store
                    .record_launch(crate::store::LaunchRecord {
                        item_id: id,
                        source: crate::model::Source::File,
                        title: "日本語のファイル 🎉",
                        at,
                    })
                    .expect("launch");
            }
            // Ids come back in the store's own order, which is byte order, not
            // insertion order. Asserting byte order is the point: a `HashMap`
            // here would make the order vary per process, and the result list
            // would reshuffle between launches.
            let entries = store.entries().expect("entries");
            let ids: Vec<&str> = entries.iter().map(|entry| entry.item_id.as_str()).collect();
            assert_eq!(ids, ["app:café", "app:日本", "app:🎉"]);
            assert!(entries
                .iter()
                .all(|entry| entry.title == "日本語のファイル 🎉"));
        }

        #[test]
        fn frecency_map_is_the_shape_the_ranker_wants() {
            let mut store = $make;
            let at = crate::frecency::Timestamp::from_unix_seconds(7);
            store
                .record_launch(crate::store::LaunchRecord {
                    item_id: "app:note",
                    source: crate::model::Source::Application,
                    title: "Notepad",
                    at,
                })
                .expect("launch");
            let map = crate::store::UsageStoreExt::frecency_map(&store).expect("map");
            assert_eq!(map.len(), 1);
            assert_eq!(
                map.get("app:note"),
                Some(&crate::frecency::Frecency::new(1, Some(at)))
            );
        }
    };
}

#[cfg(test)]
pub(crate) use usage_store_contract;

#[cfg(test)]
mod tests;
