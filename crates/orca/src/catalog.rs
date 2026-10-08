//! Collecting candidates and answering queries, off the UI thread.
//!
//! # Why the collection is a separate type from the widget
//!
//! `orca-core`'s [`ProviderSet`] cannot own a `FileSearchProvider`, because
//! that provider borrows its [`DirectoryLister`] and `ProviderSet::with`
//! requires `'static`. So this type holds the provider set and the `[files]`
//! config, and does the merge itself. That is a small amount of duplication of
//! `ProviderSet::collect_all` and it buys something worth more: the entire
//! path from "raw candidates" to "ranked rows" is reachable from a unit test,
//! because the lister and the history store are both parameters.
//!
//! # Why nothing here runs on the UI thread
//!
//! Collecting means enumerating the Start Menu, opening a COM apartment, reading
//! registry keys, walking directory trees, and reading a SQLite file. Ranking a
//! catalogue is milliseconds. Both happen on a `BackgroundExecutor`; the widget
//! only ever receives finished [`RankedItem`] vectors and paints them.
//!
//! # The cache
//!
//! Collection is the expensive half by a wide margin and the answer barely
//! changes between keystrokes, so it is collected **once per process** and
//! reused. `docs/ARCHITECTURE.md` is explicit that calling `collect_all` per
//! keystroke is the mistake to avoid; the [`Engine`] is where that is
//! structurally prevented rather than merely noted.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use orca_core::config::Files as FilesConfig;
use orca_core::providers::{
    DirectoryLister, FileSearchProvider, ProviderError, ProviderSet, RawResult,
};
use orca_core::store::{UsageStore, UsageStoreExt};
use orca_core::{rank_with, Frecency, RankedItem, RankingPolicy, ResultItem, Timestamp};

/// The candidate set plus the file-walk configuration.
///
/// No row cap here: truncation is a display decision and belongs to
/// [`Engine`], which is the thing that knows what the list will show. Keeping
/// it in one place means the cap cannot be applied twice or not at all.
pub struct Catalog {
    providers: ProviderSet,
    files: FilesConfig,
}

impl Catalog {
    /// Builds a catalogue.
    #[must_use]
    pub fn new(providers: ProviderSet, files: FilesConfig) -> Catalog {
        Catalog { providers, files }
    }

    /// The registered provider names, for the status line.
    #[must_use]
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.names()
    }

    /// Collects everything, de-duplicated, with launch history stamped on.
    ///
    /// Provider order decides de-duplication, first provider wins, exactly as
    /// `ProviderSet::collect_all` documents — so the file walk is collected
    /// *last* and can never displace a real application.
    pub fn collect(
        &self,
        lister: &dyn DirectoryLister,
        history: &dyn UsageStore,
    ) -> Result<Vec<ResultItem>, ProviderError> {
        let frecency = history
            .frecency_map()
            .map_err(|error| ProviderError::History(Box::new(error)))?;

        let mut merged: Vec<RawResult> = Vec::new();
        for candidate in self.providers.collect_all()? {
            if !merged.iter().any(|existing| existing.id == candidate.id) {
                merged.push(candidate);
            }
        }

        if self.files.enabled {
            for item in FileSearchProvider::from_config(lister, &self.files).walk()? {
                let raw = to_raw(item);
                if !merged.iter().any(|existing| existing.id == raw.id) {
                    merged.push(raw);
                }
            }
        }

        Ok(merged
            .into_iter()
            .map(|raw| {
                let seen = frecency.get(&raw.id).copied().unwrap_or(Frecency::NEVER);
                raw.into_item(seen)
            })
            .collect())
    }
}

/// Turns a `ResultItem` back into a `RawResult`.
///
/// `ResultItem` and `RawResult` have the same fields apart from `frecency`, and
/// `frecency` is stamped at the end of the collection rather than per provider,
/// so the file walk's items have to pass through the same merge as everyone
/// else's. Written out by hand rather than derived because `frecency` has to be
/// dropped, and a derive would either keep it or need a newtype.
#[must_use]
fn to_raw(item: ResultItem) -> RawResult {
    RawResult {
        id: item.id,
        title: item.title,
        subtitle: item.subtitle,
        keywords: item.keywords,
        source: item.source,
        score: item.score,
        target: item.target,
    }
}

/// The current wall clock, as a `Timestamp`.
///
/// `orca-core` takes time as an argument so it can decay frecency without a
/// clock; this is the one place the real clock is read. Returns
/// [`Timestamp::EPOCH`] rather than panicking if the system clock is before the
/// Unix epoch, which degrades ranking rather than killing the launcher.
#[must_use]
pub fn now() -> Timestamp {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    i64::try_from(seconds).map_or(Timestamp::EPOCH, Timestamp::from_unix_seconds)
}

/// Ranks a catalogue against a query, truncated to `max_results` rows.
///
/// Thin on purpose: every ordering rule lives in `orca-core` and is tested
/// there. The truncation is here because it is a UI concern and putting it in
/// the ranker would make the ranker untestable without a screen.
///
/// Returns the total match count as well as the rows, because "there are 400
/// matches and you are seeing 50" and "there are 50 matches" are different
/// situations and the status line is the only place that can say which.
#[must_use]
pub fn rank(
    policy: &RankingPolicy,
    query: &str,
    at: Timestamp,
    items: &[ResultItem],
    max_results: usize,
) -> RankedRows {
    let ranked = rank_with(policy, query, at, items.iter());
    let matches = ranked.len();
    let rows = ranked
        .into_iter()
        .take(max_results.max(1))
        .collect::<Vec<_>>();
    RankedRows { rows, matches }
}

/// A ranked answer: the rows to draw, and how many there would have been.
#[derive(Debug, Clone, Default)]
pub struct RankedRows {
    /// The rows to draw.
    pub rows: Vec<RankedItem>,
    /// The full match count, before truncation.
    pub matches: usize,
}

/// A monotonically increasing token identifying one search request.
///
/// Every keystroke bumps the generation. A background search that finishes
/// after the user has typed another character is discarded rather than rendered,
/// which is the difference between a launcher that keeps up and one that
/// flickers back through results the user already rejected.
///
/// `fetch_add` rather than a load/store pair: two searches finishing at the same
/// moment on different threads must not be handed the same number.
#[derive(Debug, Default)]
pub struct Generation(AtomicU64);

impl Generation {
    /// The current value, without changing it.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    /// Claims the next value.
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::AcqRel) + 1
    }
}

/// The collected catalogue, shared between the UI thread and the search thread.
pub type Shared = Arc<Vec<ResultItem>>;

/// What one background search produced.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The generation this answer belongs to. A caller that has moved on must
    /// drop it.
    pub generation: u64,
    /// The ranked rows, already truncated to the configured cap.
    pub rows: Vec<RankedItem>,
    /// How many candidates the query matched, before truncation.
    ///
    /// Distinct from `rows.len()` on purpose: "there are 400 matches and you
    /// are seeing 50" and "there are 50 matches" are different situations, and
    /// the status line is the only place that can say which.
    pub matches: usize,
    /// Set when collection itself failed. `rows` is then empty.
    pub error: Option<String>,
}

/// Collects once, then answers queries from the cache.
///
/// `Engine` is `Send + Sync` and is the only thing the background search needs,
/// so a search is one cheap call that cannot touch the filesystem again by
/// accident.
pub struct Engine {
    catalog: Catalog,
    lister: Arc<dyn DirectoryLister + Send + Sync>,
    history: Arc<Mutex<Box<dyn UsageStore + Send>>>,
    policy: RankingPolicy,
    max_results: usize,
    cached: Mutex<Option<Shared>>,
}

impl Engine {
    /// Builds an engine over a catalogue, a lister, and a launch-history store.
    #[must_use]
    pub fn new(
        catalog: Catalog,
        lister: Arc<dyn DirectoryLister + Send + Sync>,
        history: Arc<Mutex<Box<dyn UsageStore + Send>>>,
        policy: RankingPolicy,
        max_results: usize,
    ) -> Arc<Engine> {
        Arc::new(Engine {
            catalog,
            lister,
            history,
            policy,
            max_results,
            cached: Mutex::new(None),
        })
    }

    /// The cached catalogue, if it has been collected.
    #[must_use]
    pub fn cached(&self) -> Option<Shared> {
        self.cached
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
    }

    /// Ranks `query` against the cached catalogue.
    ///
    /// If nothing has been collected yet this collects first. That is the one
    /// path that blocks, and it is always reached from the background executor.
    pub fn search(&self, query: &str, generation: u64) -> Outcome {
        let (items, error) = match self.cached() {
            Some(items) => (items, None),
            None => match self.collect_and_cache() {
                Ok(items) => (items, None),
                Err(error) => (Arc::new(Vec::new()), Some(error)),
            },
        };

        let RankedRows { rows, matches } =
            rank(&self.policy, query, now(), &items, self.max_results);
        Outcome {
            generation,
            rows,
            matches,
            error,
        }
    }
    /// Collects, caches, and returns the catalogue.
    ///
    /// A poisoned lock is reported rather than panicked on: the lock is only
    /// ever held across a pure pointer clone, so poisoning means something else
    /// went wrong and the useful recovery is to collect again.
    pub fn collect_and_cache(&self) -> Result<Shared, String> {
        let history = self
            .history
            .lock()
            .map_err(|_| "launch history lock was poisoned".to_owned())?;
        let collected = self
            .catalog
            .collect(self.lister.as_ref(), history.as_ref())
            // `user_message`, not `to_string`: this string becomes the status
            // line inside the popup, and no path is ever rendered there. The full
            // `Display`, path included, is what the log gets.
            .map_err(|error| error.user_message())?;
        let shared: Shared = Arc::new(collected);
        drop(history);

        match self.cached.lock() {
            Ok(mut guard) => *guard = Some(Arc::clone(&shared)),
            Err(_) => return Err("catalogue cache lock was poisoned".to_owned()),
        }
        Ok(shared)
    }

    /// Credits a launch to `item`.
    ///
    /// Called on the UI thread because it is the activation path and a
    /// background hop would delay the program the user asked for. It is a
    /// single-row SQLite write, which is the one piece of I/O on this path that
    /// is small enough to be worth the trade.
    pub fn record_launch(&self, item: &ResultItem) {
        let Ok(mut store) = self.history.lock() else {
            return;
        };
        let record = orca_core::store::LaunchRecord {
            item_id: &item.id,
            source: item.source,
            title: &item.title,
            at: now(),
        };
        let _ = store.record_launch(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::Files;
    use orca_core::frecency::Frecency;
    use orca_core::providers::{InMemoryDirectory, ResultProvider};
    use orca_core::store::MemoryUsageStore;
    use orca_core::{LaunchTarget, Source};
    use std::path::PathBuf;

    const NOW: Timestamp = Timestamp::from_unix_seconds(1_700_000_000);

    fn exe(name: &str) -> LaunchTarget {
        LaunchTarget::Command {
            program: name.to_owned(),
            args: Vec::new(),
        }
    }

    fn fixed_provider(entries: &[(&str, &str, Source)]) -> impl ResultProvider + 'static {
        struct Fixed {
            entries: Vec<RawResult>,
        }
        impl ResultProvider for Fixed {
            fn name(&self) -> &str {
                "fixed"
            }
            fn collect(&self) -> Result<Vec<RawResult>, ProviderError> {
                Ok(self.entries.clone())
            }
        }
        Fixed {
            entries: entries
                .iter()
                .map(|(id, title, source)| RawResult::new(*id, *title, *source, exe(title)))
                .collect(),
        }
    }

    fn empty_files() -> Files {
        Files::default()
    }

    #[test]
    fn a_collection_carries_every_providers_candidates() {
        let providers = ProviderSet::new()
            .with(
                "a",
                fixed_provider(&[("app:note", "Notepad", Source::Application)]),
            )
            .with("b", fixed_provider(&[("cmd:gh", "gh", Source::Command)]));
        let catalog = Catalog::new(providers, empty_files());
        let items = catalog
            .collect(&InMemoryDirectory::new(), &MemoryUsageStore::new())
            .expect("collection should succeed");

        let mut titles: Vec<&str> = items.iter().map(|item| item.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(titles, vec!["Notepad", "gh"]);
    }

    #[test]
    fn the_first_provider_wins_a_duplicate_id() {
        // The same shape as `ProviderSet::collect_all`, asserted here because
        // the file walk is merged by this crate and could easily disagree.
        let providers = ProviderSet::new()
            .with(
                "authoritative",
                fixed_provider(&[("dup", "First", Source::Application)]),
            )
            .with(
                "later",
                fixed_provider(&[("dup", "Second", Source::Application)]),
            );
        let catalog = Catalog::new(providers, empty_files());
        let items = catalog
            .collect(&InMemoryDirectory::new(), &MemoryUsageStore::new())
            .expect("collection should succeed");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "First");
    }

    #[test]
    fn the_file_walk_contributes_only_when_it_is_enabled() {
        let directory = InMemoryDirectory::new().with_file("/root/notes.md");
        let files = Files {
            enabled: true,
            roots: vec![PathBuf::from("/root")],
            max_depth: 1,
            max_results: 100,
            follow_links: false,
        };
        let providers = ProviderSet::new().with("apps", fixed_provider(&[]));
        let catalog = Catalog::new(providers, files.clone());
        let items = catalog
            .collect(&directory, &MemoryUsageStore::new())
            .expect("collection should succeed");
        assert!(
            items.iter().any(|item| item.title == "notes.md"),
            "enabled file walk contributed nothing: {items:?}"
        );

        let disabled = Catalog::new(
            ProviderSet::new().with("apps", fixed_provider(&[])),
            Files {
                enabled: false,
                ..files
            },
        );
        let items = disabled
            .collect(&directory, &MemoryUsageStore::new())
            .expect("collection should succeed");
        assert!(items.is_empty(), "disabled file walk contributed something");
    }

    #[test]
    fn the_round_trip_through_a_raw_result_preserves_every_field() {
        let original = ResultItem::new("file:/a/b.txt", "b.txt", Source::File, exe("b.txt"))
            .with_subtitle("/a")
            .with_score(0.75)
            .with_frecency(Frecency::new(4, Some(NOW)));
        let raw = to_raw(original.clone());
        assert_eq!(raw.id, original.id);
        assert_eq!(raw.title, original.title);
        assert_eq!(raw.subtitle, original.subtitle);
        assert_eq!(raw.source, original.source);
        assert_eq!(raw.score, original.score);
        assert_eq!(raw.target, original.target);
    }

    #[test]
    fn launch_history_is_stamped_onto_the_items_it_names() {
        let providers = ProviderSet::new().with(
            "apps",
            fixed_provider(&[
                ("app:note", "Notepad", Source::Application),
                ("app:calc", "Calculator", Source::Application),
            ]),
        );
        let store =
            MemoryUsageStore::with_history([("app:note".to_owned(), Frecency::new(9, Some(NOW)))]);
        let catalog = Catalog::new(providers, empty_files());
        let items = catalog
            .collect(&InMemoryDirectory::new(), &store)
            .expect("collection should succeed");

        let note = items.iter().find(|item| item.id == "app:note").unwrap();
        let calc = items.iter().find(|item| item.id == "app:calc").unwrap();
        assert_eq!(note.frecency.launches(), 9);
        assert!(
            calc.frecency.is_never(),
            "an unlaunched item gained history"
        );
    }

    #[test]
    fn ranking_a_query_drops_what_does_not_match_and_keeps_what_does() {
        let items = vec![
            ResultItem::new("app:note", "Notepad", Source::Application, exe("notepad")),
            ResultItem::new("app:calc", "Calculator", Source::Application, exe("calc")),
        ];
        let ranked = rank(&RankingPolicy::DEFAULT, "note", NOW, &items, 50);
        assert_eq!(ranked.matches, 1);
        assert_eq!(ranked.rows.len(), 1);
        assert_eq!(ranked.rows[0].item.title, "Notepad");
    }

    #[test]
    fn the_row_cap_truncates_without_hiding_that_there_were_more() {
        let items: Vec<ResultItem> = (0..20)
            .map(|index| {
                ResultItem::new(
                    format!("app:{index}"),
                    format!("App {index}"),
                    Source::Application,
                    exe("app"),
                )
            })
            .collect();
        let capped = rank(&RankingPolicy::DEFAULT, "", NOW, &items, 5);
        assert_eq!(capped.rows.len(), 5);
        assert_eq!(
            capped.matches, 20,
            "the cap must not be reported as 'these were all the matches'"
        );
        // A config of `max_results = 0` would otherwise render a list that can
        // never be moved in, so zero is treated as one rather than as nothing.
        assert_eq!(
            rank(&RankingPolicy::DEFAULT, "", NOW, &items, 0).rows.len(),
            1
        );
    }

    #[test]
    fn generations_never_repeat() {
        let generation = Generation::default();
        let first = generation.next();
        let second = generation.next();
        assert_ne!(first, second);
        assert_eq!(generation.get(), second);
    }

    #[test]
    fn an_engine_collects_once_and_then_answers_from_the_cache() {
        // The regression this guards: a per-keystroke `collect_all`. A second
        // `search` must not re-enumerate, which is only observable if the
        // catalogue handle is the *same* allocation.
        let providers = ProviderSet::new().with(
            "apps",
            fixed_provider(&[("app:note", "Notepad", Source::Application)]),
        );
        let catalog = Catalog::new(providers, empty_files());
        let engine = Engine::new(
            catalog,
            Arc::new(InMemoryDirectory::new()),
            Arc::new(Mutex::new(
                Box::new(MemoryUsageStore::new()) as Box<dyn UsageStore + Send>
            )),
            RankingPolicy::DEFAULT,
            50,
        );

        let first = engine.search("note", 1);
        assert_eq!(first.error, None);
        assert_eq!(first.rows.len(), 1);
        let after_first = engine
            .cached()
            .expect("the first search populates the cache");
        assert!(Arc::ptr_eq(&after_first, &engine.cached().unwrap()));

        let second = engine.search("note", 2);
        assert_eq!(second.rows.len(), 1);
        assert!(Arc::ptr_eq(&engine.cached().unwrap(), &after_first));
    }

    #[test]
    fn a_failed_collection_reports_the_error_rather_than_an_empty_list() {
        struct Failing;
        impl ResultProvider for Failing {
            fn name(&self) -> &str {
                "failing"
            }
            fn collect(&self) -> Result<Vec<RawResult>, ProviderError> {
                Err(ProviderError::Io {
                    path: PathBuf::from("/nowhere"),
                    detail: "no such directory".to_owned(),
                })
            }
        }
        let catalog = Catalog::new(ProviderSet::new().with("failing", Failing), empty_files());
        let engine = Engine::new(
            catalog,
            Arc::new(InMemoryDirectory::new()),
            Arc::new(Mutex::new(
                Box::new(MemoryUsageStore::new()) as Box<dyn UsageStore + Send>
            )),
            RankingPolicy::DEFAULT,
            50,
        );

        let outcome = engine.search("note", 7);
        let error = outcome.error.expect("a failed collection must be reported");
        assert!(
            error.contains("failing"),
            "{error} does not name the provider"
        );
        assert!(outcome.rows.is_empty());
    }

    #[test]
    fn the_outcome_carries_the_generation_it_was_asked_for() {
        let catalog = Catalog::new(
            ProviderSet::new().with("apps", fixed_provider(&[])),
            empty_files(),
        );
        let engine = Engine::new(
            catalog,
            Arc::new(InMemoryDirectory::new()),
            Arc::new(Mutex::new(
                Box::new(MemoryUsageStore::new()) as Box<dyn UsageStore + Send>
            )),
            RankingPolicy::DEFAULT,
            50,
        );
        assert_eq!(engine.search("", 42).generation, 42);
    }

    /// An engine over an in-memory store, for tests that only care about the
    /// engine and not about the catalogue.
    fn engine_over(store: MemoryUsageStore) -> Arc<Engine> {
        let catalog = Catalog::new(
            ProviderSet::new().with("apps", fixed_provider(&[])),
            empty_files(),
        );
        Engine::new(
            catalog,
            Arc::new(InMemoryDirectory::new()),
            Arc::new(Mutex::new(Box::new(store) as Box<dyn UsageStore + Send>)),
            RankingPolicy::DEFAULT,
            50,
        )
    }

    #[test]
    fn recording_a_launch_credits_the_named_item() {
        let engine = engine_over(MemoryUsageStore::new());
        let item = ResultItem::new("app:note", "Notepad", Source::Application, exe("notepad"));
        engine.record_launch(&item);

        let history = engine.history.lock().expect("the lock is not poisoned");
        assert_eq!(
            history
                .frecency(&item.id)
                .expect("history readable")
                .expect("the launch was recorded")
                .launches(),
            1
        );
    }

    #[test]
    fn recording_a_launch_survives_a_poisoned_history_lock() {
        let engine = engine_over(MemoryUsageStore::new());
        let item = ResultItem::new("app:note", "Notepad", Source::Application, exe("notepad"));

        // Poison the mutex from inside a panic. `record_launch` must then be a
        // no-op rather than a second panic: propagating a poisoned lock would
        // turn one bad frame anywhere in the process into a crash every time
        // the user presses Enter.
        let clone = Arc::clone(&engine.history);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = clone.lock().expect("lock to poison");
            panic!("poison");
        }));
        assert!(
            engine.history.lock().is_err(),
            "the lock should be poisoned"
        );

        engine.record_launch(&item);
    }
}
