//! The file-search provider: a bounded directory walk over an injected seam.
//!
//! Layering rule 2 keeps the filesystem out of this crate, so the walk itself
//! is a trait ([`DirectoryLister`]) and everything above it — depth limiting,
//! cycle detection, result capping, id derivation, path-to-subtitle — is a pure
//! state machine over that trait. [`InMemoryDirectory`] is a complete
//! implementation, so every behaviour below is tested against fixtures rather
//! than against whatever happens to be on the machine.
//!
//! The `std::fs` adapter is a dozen lines and belongs to `orca`:
//!
//! ```ignore
//! pub struct StdFsDirectory;
//!
//! impl DirectoryLister for StdFsDirectory {
//!     fn list(&self, path: &Path) -> Result<Vec<PathBuf>, ProviderError> {
//!         std::fs::read_dir(path)
//!             .map_err(|error| ProviderError::Io { path: path.into(), detail: error.to_string() })?
//!             .filter_map(|entry| entry.ok().map(|entry| entry.path()))
//!             .collect();
//!         Ok(collected)
//!     }
//!
//!     fn metadata(&self, path: &Path) -> Option<EntryKind> { /* stat the path */ }
//! }
//! ```
//!
//! # Why a cycle guard is not optional
//!
//! On Windows a junction or a symlink can point at an ancestor, and
//! `std::fs::read_dir` cannot tell. Without a visited set a walk over
//! `C:\Users` that crosses one junction never terminates. `follow_links` exists
//! as an option, and the guard is on in both modes — the option decides whether
//! the walk *descends* through a link, not whether it *notices* one.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::model::{LaunchTarget, ResultItem, Source};

use super::{ProviderError, RawResult, ResultProvider};

/// What a [`DirectoryLister`] knows about a path without listing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// Something else: a reparse point that is neither, a device, a socket.
    Other,
}

/// Reads directory entries.
///
/// The seam between orca-core and the filesystem. It is deliberately as small
/// as it can be: list a directory, classify a path, and read a modification
/// time. Everything else is pure.
pub trait DirectoryLister: Send + Sync {
    /// Every entry directly inside `path`.
    fn list(&self, path: &Path) -> Result<Vec<PathBuf>, ProviderError>;

    /// What `path` is. `None` when it does not exist or cannot be stat'ed.
    fn kind(&self, path: &Path) -> Option<EntryKind>;

    /// Modification time as seconds since the Unix epoch, or `None` if
    /// unavailable.
    ///
    /// Optional because not every file system has one, and a file without a
    /// timestamp is still a perfectly good result.
    fn modified_at(&self, path: &Path) -> Option<i64>;
}

/// Bounds on one collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkLimits {
    /// How many levels below each root to descend. `1` means the root's own
    /// entries and nothing deeper.
    pub max_depth: usize,
    /// Hard cap on entries returned, across all roots.
    pub max_results: usize,
    /// Whether to descend through a link back into an already-visited
    /// directory.
    pub follow_links: bool,
}

impl Default for WalkLimits {
    fn default() -> WalkLimits {
        WalkLimits {
            max_depth: 6,
            max_results: 2_000,
            follow_links: false,
        }
    }
}

impl WalkLimits {
    /// Builds limits from the config file's `[files]` section.
    #[must_use]
    pub fn from_config(config: &crate::config::Files) -> WalkLimits {
        WalkLimits {
            max_depth: config.max_depth,
            max_results: config.max_results,
            follow_links: config.follow_links,
        }
    }
}

/// Serves files and folders from a set of roots.
#[derive(Clone)]
pub struct FileSearchProvider<'a> {
    roots: Vec<PathBuf>,
    limits: WalkLimits,
    lister: Option<&'a dyn DirectoryLister>,
}

impl std::fmt::Debug for FileSearchProvider<'_> {
    /// Hand-written rather than derived: a `dyn DirectoryLister` is not
    /// `Debug`, and printing a filesystem adapter's internals is not useful
    /// anyway. The roots and the limits are the interesting part.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSearchProvider")
            .field("roots", &self.roots)
            .field("limits", &self.limits)
            .field("has_lister", &self.lister.is_some())
            .finish()
    }
}

impl<'a> FileSearchProvider<'a> {
    /// Builds a provider over `lister`.
    #[must_use]
    pub fn new(
        lister: &'a dyn DirectoryLister,
        roots: impl IntoIterator<Item = PathBuf>,
        limits: WalkLimits,
    ) -> FileSearchProvider<'a> {
        FileSearchProvider {
            roots: roots.into_iter().collect(),
            limits,
            lister: Some(lister),
        }
    }

    /// Builds a disabled provider: no roots, collects nothing.
    ///
    /// This is what a `files.enabled = false` config produces, and it is a
    /// value rather than an `Option` so the UI never has to ask.
    #[must_use]
    pub fn disabled() -> FileSearchProvider<'static> {
        FileSearchProvider {
            roots: Vec::new(),
            limits: WalkLimits::default(),
            lister: None,
        }
    }

    /// Builds a provider from the config file, enabled or not.
    #[must_use]
    pub fn from_config(
        lister: &'a dyn DirectoryLister,
        config: &crate::config::Files,
    ) -> FileSearchProvider<'a> {
        if config.enabled {
            FileSearchProvider::new(
                lister,
                config.roots.clone(),
                WalkLimits::from_config(config),
            )
        } else {
            FileSearchProvider::disabled()
        }
    }

    /// The configured roots.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The walk in isolation, as `ResultItem`s with no history stamped.
    ///
    /// Separate from [`ResultProvider::collect`] so the walk can be tested
    /// without going through a provider set, and so a caller can stamp history
    /// itself.
    ///
    /// # Depth
    ///
    /// A root sits at depth 0. `max_depth = 1` means *only the root's own
    /// entries* — nothing below them — so the check is applied to a child's
    /// depth, before the child is emitted or queued. Checking the depth of a
    /// directory when it is dequeued would let a child at `max_depth + 1` into
    /// the results before its parent's entry was refused, which is the
    /// off-by-one that makes a "depth 1" search return a two-level tree.
    ///
    /// Iterative, not recursive, so a deep tree is bounded by `max_depth` and
    /// not by the call stack.
    pub fn walk(&self) -> Result<Vec<ResultItem>, ProviderError> {
        let Some(lister) = self.lister else {
            return Ok(Vec::new());
        };
        if self.roots.is_empty() {
            return Ok(Vec::new());
        }

        // BTreeSet, not HashSet: a provider whose output order is reproducible
        // is one the tests can assert on exactly.
        let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
        let mut results: Vec<ResultItem> = Vec::new();
        let mut queue: VecDeque<(PathBuf, usize)> = self
            .roots
            .iter()
            .map(|root| (root.clone(), 0usize))
            .collect();

        while let Some((directory, depth)) = queue.pop_front() {
            if results.len() >= self.limits.max_results {
                break;
            }
            // Normalised before dedup so `C:\a\b` and `C:\a\.\b` are the same
            // directory, which is one half of the cycle guard. The other half is
            // that a *link* into an already-visited directory is refused here.
            if !visited.insert(normalise(&directory)) {
                continue;
            }

            let entries = lister.list(&directory)?;
            let child_depth = depth + 1;
            if child_depth > self.limits.max_depth {
                // Nothing inside this directory may be emitted, and nothing
                // inside it is reachable without going through it, so the whole
                // subtree is done. `break` rather than `continue` for the same
                // reason: there is nothing left in *this* directory to consider.
                continue;
            }

            for path in entries {
                let path = normalise(&path);
                if visited.contains(&path) {
                    continue;
                }
                match lister.kind(&path) {
                    Some(EntryKind::Directory) => {
                        results.push(entry_to_item(
                            &path,
                            EntryKind::Directory,
                            lister.modified_at(&path),
                        ));
                        queue.push_back((path, child_depth));
                    }
                    Some(EntryKind::File) => {
                        results.push(entry_to_item(
                            &path,
                            EntryKind::File,
                            lister.modified_at(&path),
                        ));
                    }
                    Some(EntryKind::Other) | None => {}
                }
                if results.len() >= self.limits.max_results {
                    break;
                }
            }
        }

        Ok(results)
    }
}

impl ResultProvider for FileSearchProvider<'_> {
    fn name(&self) -> &str {
        "files"
    }

    fn collect(&self) -> Result<Vec<RawResult>, ProviderError> {
        Ok(self
            .walk()?
            .into_iter()
            .map(|item| RawResult {
                id: item.id,
                title: item.title,
                subtitle: item.subtitle,
                keywords: item.keywords,
                source: item.source,
                score: item.score,
                target: item.target,
            })
            .collect())
    }
}

/// The containing folder's *name*, with no separators and no drive.
///
/// This is the only part of a path worth putting on screen. It answers the one
/// question a file name cannot — `notes.txt` in Documents versus `notes.txt` on
/// the Desktop — and it reads as a word rather than as a location.
///
/// `None` when there is no parent name to report: a filesystem root, or a
/// relative path with no directory component. The row then shows a title and
/// nothing else, which is correct rather than a gap.
fn parent_folder_name(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    let name = parent.file_name()?.to_string_lossy();
    (!name.is_empty()).then(|| name.into_owned())
}

/// Turns one walked path into a candidate.
///
/// The id is the normalised path, so the same file keys the same frecency row
/// across runs — which is the whole reason the frecency store has a primary key
/// at all. It is a path, not a hash, because a hash would collide eventually and
/// a collision means two different files share a launch history.
fn entry_to_item(path: &Path, kind: EntryKind, modified_at: Option<i64>) -> ResultItem {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let source = match kind {
        EntryKind::Directory => Source::Folder,
        _ => Source::File,
    };
    // The full path is searchable and is never rendered; the subtitle is the
    // containing folder's *name*, which is the one piece of a path that
    // disambiguates two files called `notes.txt` without being a path at all.
    //
    // See `ResultItem::keywords` for why these are two fields. The short version
    // is that a row reading `C:\Users\chris\Documents\2026\notes.txt` is
    // unusable as a list: it is mostly the same text on every line, and the one
    // word that varies — the file name — is what the title already says.
    let mut item = ResultItem::new(
        format!("{}:{}", id_prefix(kind), path.to_string_lossy()),
        name,
        source,
        LaunchTarget::Executable {
            path: path.to_path_buf(),
            args: Vec::new(),
        },
    )
    .with_keywords(path.to_string_lossy());
    if let Some(folder) = parent_folder_name(path) {
        item = item.with_subtitle(folder);
    }

    // Modification time becomes a prior in `0.0 ..= 1.0`, with a one-year
    // half-life over the Unix epoch — a coarse, deliberately forgiving scale,
    // because a file's mtime is a much weaker signal than a launch count and
    // should not be able to dominate one.
    match modified_at {
        Some(at) if at > 0 => {
            let years = at as f64 / (365.25 * 86_400.0);
            item.with_score(0.5f64.powf(years).clamp(0.0, 1.0))
        }
        _ => item,
    }
}

fn id_prefix(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Directory => "dir",
        _ => "file",
    }
}

/// Collapses `.` components and redundant separators so two spellings of one
/// path dedup against each other.
///
/// Deliberately not `canonicalize`: that is a filesystem call, and it also
/// resolves junctions, which is precisely the thing the cycle guard needs to see
/// as distinct.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// A [`DirectoryLister`] backed by a literal map, for tests and fixtures.
///
/// A complete implementation, not a stub: it is what makes the walk above
/// testable against a fixture that contains a cycle, a permission denial, and a
/// tree far deeper than any real test should create on disk.
#[derive(Debug, Clone, Default)]
pub struct InMemoryDirectory {
    entries: std::collections::BTreeMap<PathBuf, Vec<PathBuf>>,
    kinds: std::collections::BTreeMap<PathBuf, EntryKind>,
    modified: std::collections::BTreeMap<PathBuf, i64>,
    unreadable: BTreeSet<PathBuf>,
}

impl InMemoryDirectory {
    /// An empty filesystem.
    #[must_use]
    pub fn new() -> InMemoryDirectory {
        InMemoryDirectory::default()
    }

    /// Adds a regular file, creating its parent directory.
    pub fn with_file(mut self, path: impl AsRef<Path>) -> InMemoryDirectory {
        self.add(path.as_ref(), EntryKind::File);
        self
    }

    /// Adds a directory.
    pub fn with_dir(mut self, path: impl AsRef<Path>) -> InMemoryDirectory {
        self.add(path.as_ref(), EntryKind::Directory);
        self
    }

    /// Marks a directory as failing to list, with the message it should give.
    pub fn with_unreadable(mut self, path: impl AsRef<Path>) -> InMemoryDirectory {
        self.unreadable.insert(normalise(path.as_ref()));
        self
    }

    /// Adds `target` to `from`'s entries as if it were a directory link.
    ///
    /// This is how a Windows junction or a symlink is modelled: `from` lists
    /// `target`, and `target` looks like a directory. Pointing it at an ancestor
    /// produces exactly the cycle that makes a naive walk run forever, which is
    /// the only way to test the cycle guard without creating a junction.
    #[must_use]
    pub fn with_link(
        mut self,
        from: impl AsRef<Path>,
        target: impl AsRef<Path>,
    ) -> InMemoryDirectory {
        let from = normalise(from.as_ref());
        let target = normalise(target.as_ref());
        self.kinds
            .entry(target.clone())
            .or_insert(EntryKind::Directory);
        self.entries.entry(from).or_default().push(target);
        self
    }

    /// Sets a modification time, in Unix seconds.
    #[must_use]
    pub fn with_modified(mut self, path: impl AsRef<Path>, at: i64) -> InMemoryDirectory {
        self.modified.insert(normalise(path.as_ref()), at);
        self
    }

    fn add(&mut self, path: &Path, kind: EntryKind) {
        let path = normalise(path);
        if let Some(parent) = path.parent() {
            self.kinds
                .entry(parent.to_path_buf())
                .or_insert(EntryKind::Directory);
        }
        self.kinds.insert(path.clone(), kind);
        if let Some(parent) = path.parent() {
            self.entries
                .entry(parent.to_path_buf())
                .or_default()
                .push(path);
        }
    }
}

impl DirectoryLister for InMemoryDirectory {
    fn list(&self, path: &Path) -> Result<Vec<PathBuf>, ProviderError> {
        let path = normalise(path);
        if self.unreadable.contains(&path) {
            return Err(ProviderError::Io {
                path,
                detail: "permission denied (fixture)".to_owned(),
            });
        }
        Ok(self.entries.get(&path).cloned().unwrap_or_default())
    }

    fn kind(&self, path: &Path) -> Option<EntryKind> {
        self.kinds.get(&normalise(path)).copied()
    }

    fn modified_at(&self, path: &Path) -> Option<i64> {
        self.modified.get(&normalise(path)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{rank_at, Frecency, RankingPolicy, Timestamp};

    const SECONDS_PER_YEAR: i64 = 31_557_600;

    fn fixture() -> InMemoryDirectory {
        InMemoryDirectory::new()
            .with_dir(r"C:\root")
            .with_file(r"C:\root\notes.txt")
            .with_file(r"C:\root\Notepad.exe")
            .with_dir(r"C:\root\projects")
            .with_dir(r"C:\root\projects\orca")
            .with_file(r"C:\root\projects\orca\Cargo.toml")
            .with_dir(r"C:\root\empty")
    }

    fn walk(directory: &InMemoryDirectory, roots: &[&str], limits: WalkLimits) -> Vec<String> {
        let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
        FileSearchProvider::new(directory, roots, limits)
            .walk()
            .expect("walk")
            .into_iter()
            .map(|item| item.title)
            .collect()
    }

    /// Titles of a walked set, for assertion messages.
    fn titles_of(items: &[ResultItem]) -> Vec<&str> {
        items.iter().map(|item| item.title.as_str()).collect()
    }

    #[test]
    fn a_disabled_provider_walks_nothing() {
        let directory = fixture();
        let provider = FileSearchProvider::disabled();
        assert!(provider.walk().expect("walk").is_empty());
        assert!(provider.collect().expect("collect").is_empty());
        assert!(provider.roots().is_empty());

        // And the fixture really does have content, so the empty result above is
        // the provider's doing and not an empty fixture.
        assert!(!FileSearchProvider::new(
            &directory,
            [PathBuf::from(r"C:\root")],
            WalkLimits::default()
        )
        .walk()
        .expect("walk")
        .is_empty());
    }

    #[test]
    fn walks_files_and_folders_and_labels_each_correctly() {
        let directory = fixture();
        let items = FileSearchProvider::new(
            &directory,
            [PathBuf::from(r"C:\root")],
            WalkLimits::default(),
        )
        .walk()
        .expect("walk");

        let by_title: std::collections::BTreeMap<_, _> = items
            .iter()
            .map(|item| (item.title.as_str(), item.source))
            .collect();
        assert_eq!(by_title["notes.txt"], Source::File);
        assert_eq!(by_title["projects"], Source::Folder);
        assert_eq!(by_title["Cargo.toml"], Source::File);
        // An empty directory is still a result; the user may want to open it.
        assert_eq!(by_title["empty"], Source::Folder);
    }

    #[test]
    fn the_depth_limit_bounds_the_walk() {
        let directory = fixture();
        let shallow = walk(
            &directory,
            &[r"C:\root"],
            WalkLimits {
                max_depth: 1,
                ..WalkLimits::default()
            },
        );
        // `max_depth = 1` is the root's own entries and nothing below them:
        // `projects` is listed, `orca` (one level further) is not.
        assert!(shallow.contains(&"projects".to_owned()), "{shallow:?}");
        assert!(!shallow.contains(&"orca".to_owned()), "{shallow:?}");

        let medium = walk(
            &directory,
            &[r"C:\root"],
            WalkLimits {
                max_depth: 2,
                ..WalkLimits::default()
            },
        );
        assert!(medium.contains(&"orca".to_owned()), "{medium:?}");

        let deep = walk(
            &directory,
            &[r"C:\root"],
            WalkLimits {
                max_depth: 5,
                ..WalkLimits::default()
            },
        );
        assert!(deep.contains(&"Cargo.toml".to_owned()), "{deep:?}");

        // A depth of 0 is a legitimate "no results", and the config layer is
        // what forbids it for a real search.
        assert!(walk(
            &directory,
            &[r"C:\root"],
            WalkLimits {
                max_depth: 0,
                ..WalkLimits::default()
            }
        )
        .is_empty());
    }

    #[test]
    fn the_result_cap_is_hard() {
        let directory = fixture();
        for cap in [1usize, 2, 3] {
            let titles = walk(
                &directory,
                &[r"C:\root"],
                WalkLimits {
                    max_results: cap,
                    ..WalkLimits::default()
                },
            );
            assert_eq!(titles.len(), cap, "cap {cap} not honoured");
        }
        assert!(walk(&directory, &[r"C:\root"], WalkLimits::default()).len() > 3);
    }

    #[test]
    fn a_cycle_terminates_instead_of_hanging() {
        // The whole reason `visited` exists. `C:\root\loop` is a junction back
        // to `C:\root`, and `C:\root\up` is a second spelling of the same
        // directory, which `.`-components would otherwise defeat.
        let directory = InMemoryDirectory::new()
            .with_dir(r"C:\root")
            .with_file(r"C:\root\a.txt")
            .with_dir(r"C:\root\loop")
            .with_file(r"C:\root\loop\b.txt")
            .with_link(r"C:\root\loop", r"C:\root")
            .with_file(r"C:\root\.\c.txt");

        let titles = walk(
            &directory,
            &[r"C:\root"],
            WalkLimits {
                max_depth: 32,
                follow_links: true,
                ..WalkLimits::default()
            },
        );

        // Everything is found once...
        for expected in ["a.txt", "b.txt", "c.txt", "loop"] {
            assert!(
                titles.contains(&expected.to_owned()),
                "{expected:?} missing: {titles:?}"
            );
        }
        // ...and the cycle did not multiply anything, which is what "the walk
        // would not have terminated" looks like if the guard ever regressed.
        let mut deduped = titles.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), titles.len(), "duplicates: {titles:?}");
        assert_eq!(titles.len(), 4, "{titles:?}");
    }

    #[test]
    fn a_self_referential_root_terminates() {
        // A root that is its own ancestor: the pathological case.
        let directory = InMemoryDirectory::new()
            .with_dir(r"C:\root")
            .with_file(r"C:\root\only.txt")
            .with_link(r"C:\root", r"C:\root");
        let titles = walk(
            &directory,
            &[r"C:\root"],
            WalkLimits {
                max_depth: 64,
                follow_links: true,
                ..WalkLimits::default()
            },
        );
        assert_eq!(titles, ["only.txt"]);
    }

    #[test]
    fn multiple_roots_are_merged_and_the_cap_spans_them() {
        let directory = InMemoryDirectory::new()
            .with_file(r"C:\a\one.txt")
            .with_file(r"C:\b\two.txt")
            .with_file(r"C:\b\three.txt");
        let all = walk(&directory, &[r"C:\a", r"C:\b"], WalkLimits::default());
        assert_eq!(all.len(), 3);
        let capped = walk(
            &directory,
            &[r"C:\a", r"C:\b"],
            WalkLimits {
                max_results: 2,
                ..WalkLimits::default()
            },
        );
        assert_eq!(capped.len(), 2, "the cap is global, not per root");
    }

    #[test]
    fn an_unreadable_directory_is_reported_not_swallowed() {
        let directory = fixture().with_unreadable(r"C:\root\projects");
        let error = FileSearchProvider::new(
            &directory,
            [PathBuf::from(r"C:\root")],
            WalkLimits::default(),
        )
        .walk()
        .expect_err("must be reported");
        assert!(matches!(error, ProviderError::Io { .. }), "{error}");
        assert!(error.to_string().contains("projects"), "{error}");
    }

    #[test]
    fn a_missing_root_yields_nothing_rather_than_an_error() {
        let directory = fixture();
        let titles = walk(&directory, &[r"C:\nope"], WalkLimits::default());
        assert!(titles.is_empty());
    }

    #[test]
    fn no_roots_yields_nothing() {
        let directory = fixture();
        assert!(walk(&directory, &[], WalkLimits::default()).is_empty());
    }

    #[test]
    fn an_id_is_the_normalised_path_so_history_is_stable() {
        let directory = fixture();
        let items = FileSearchProvider::new(
            &directory,
            [PathBuf::from(r"C:\root")],
            WalkLimits::default(),
        )
        .walk()
        .expect("walk");
        let file = items
            .iter()
            .find(|item| item.title == "notes.txt")
            .expect("found");

        assert_eq!(file.id, r"file:C:\root\notes.txt");
        assert_eq!(file.frecency, Frecency::NEVER);
        assert_eq!(file.score, 0.0, "no mtime means no prior");
        // The subtitle is the folder name only. The full path is searchable, and
        // is carried in `keywords` precisely so it never has to be rendered.
        assert_eq!(file.subtitle.as_deref(), Some("root"));
        assert_eq!(file.keywords.as_deref(), Some(r"C:\root\notes.txt"));
        assert_eq!(
            file.target,
            LaunchTarget::Executable {
                path: PathBuf::from(r"C:\root\notes.txt"),
                args: Vec::new(),
            }
        );

        let folder = items
            .iter()
            .find(|item| item.title == "projects")
            .expect("found");
        assert!(
            folder.id.starts_with("dir:"),
            "a folder must not share a namespace with a file: {}",
            folder.id
        );
    }

    #[test]
    fn a_modification_time_becomes_a_decaying_prior() {
        let now = SECONDS_PER_YEAR;
        let directory = fixture()
            .with_file(r"C:\root\fresh.txt")
            .with_modified(r"C:\root\fresh.txt", now)
            .with_file(r"C:\root\old.txt")
            .with_modified(r"C:\root\old.txt", 0);
        let items = FileSearchProvider::new(
            &directory,
            [PathBuf::from(r"C:\root")],
            WalkLimits::default(),
        )
        .walk()
        .expect("walk");

        let score_of = |name: &str| {
            items
                .iter()
                .find(|item| item.title == name)
                .unwrap_or_else(|| panic!("{name} not in {:?}", titles_of(&items)))
                .score
        };
        let fresh = score_of("fresh.txt");
        let old = score_of("old.txt");
        assert!(
            fresh > old,
            "a fresh file should out-prioritise an old one: {fresh} vs {old}"
        );
        assert!((0.0..=1.0).contains(&fresh));
        // A timestamp of exactly 0 is "no useful mtime", not "the epoch".
        assert_eq!(old, 0.0);
    }

    #[test]
    fn results_reach_the_ranker_and_ranked_order_is_deterministic() {
        let directory = fixture();
        let items = FileSearchProvider::new(
            &directory,
            [PathBuf::from(r"C:\root")],
            WalkLimits::default(),
        )
        .walk()
        .expect("walk");

        let ranked = rank_at(
            RankingPolicy::DEFAULT,
            "note",
            Timestamp::from_unix_seconds(1),
            &items,
        );
        let titles: Vec<&str> = ranked.iter().map(|r| r.item.title.as_str()).collect();
        // `notes.txt` (fuzzy) and `Notepad.exe` (substring) both match; `orca`
        // does not.
        assert!(titles.contains(&"notes.txt"), "{titles:?}");
        assert!(titles.contains(&"Notepad.exe"), "{titles:?}");
        assert!(!titles.contains(&"Cargo.toml"), "{titles:?}");

        let again = rank_at(
            RankingPolicy::DEFAULT,
            "note",
            Timestamp::from_unix_seconds(1),
            &items,
        );
        assert_eq!(
            titles,
            again
                .iter()
                .map(|r| r.item.title.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_walk_order_is_reproducible() {
        let directory = fixture();
        let first = walk(&directory, &[r"C:\root"], WalkLimits::default());
        for _ in 0..5 {
            assert_eq!(
                walk(&directory, &[r"C:\root"], WalkLimits::default()),
                first
            );
        }
    }

    #[test]
    fn built_from_config_it_is_disabled_by_default() {
        let directory = fixture();
        let config = crate::config::Config::default();
        let provider = FileSearchProvider::from_config(&directory, &config.files);
        assert!(provider.roots().is_empty());
        assert!(provider.walk().expect("walk").is_empty());

        let config = crate::config::Config::load_from_str(
            "[files]\nenabled = true\nroots = [\"C:\\\\root\"]\nmax_depth = 1\n",
            "test.toml",
        )
        .expect("config");
        let provider = FileSearchProvider::from_config(&directory, &config.files);
        assert_eq!(provider.roots(), [PathBuf::from(r"C:\root")]);
        assert!(provider.walk().expect("walk").len() < fixture().list_all_len());
    }

    impl InMemoryDirectory {
        /// How many entries the fixture holds, so the assertion above is not a
        /// tautology.
        fn list_all_len(self) -> usize {
            self.kinds.len()
        }
    }

    #[test]
    fn a_deep_tree_does_not_blow_the_stack() {
        // The walk is iterative, so depth is bounded by `max_depth` and not by
        // the call stack. 200 levels with a generous limit is a real check.
        //
        // The counts are `files(200) + directories(199)`, not 200: level 1's
        // parent is the root, which already exists, so it adds a file and no new
        // directory. Every intermediate directory has to be registered or its
        // children are unreachable — asserted separately below.
        let mut directory = InMemoryDirectory::new().with_dir(r"C:\root");
        for level in 1..=200u32 {
            let parent = if level == 1 {
                r"C:\root".to_owned()
            } else {
                format!(r"C:\root\{}", "d".repeat(level as usize - 1))
            };
            directory = directory
                .with_dir(&parent)
                .with_file(format!(r"{parent}\f{level}.txt"));
        }
        let titles = walk(
            &directory,
            &[r"C:\root"],
            WalkLimits {
                max_depth: 250,
                max_results: 10_000,
                follow_links: false,
            },
        );
        let files = titles.iter().filter(|t| t.starts_with('f')).count();
        let directories = titles.len() - files;
        assert_eq!(files, 200, "every level's file");
        assert_eq!(directories, 199, "the root's children plus 198 deeper dirs");
        assert!(titles.contains(&"f1.txt".to_owned()), "the shallowest file");
        assert!(titles.contains(&"f200.txt".to_owned()), "the deepest file");
    }

    #[test]
    fn a_file_whose_parent_is_not_registered_is_unreachable() {
        // Documents the fixture's own limit, so a future test that "finds
        // nothing" knows whether it found nothing or built an empty tree.
        let directory = InMemoryDirectory::new()
            .with_dir(r"C:\root")
            .with_file(r"C:\root\orphan\child.txt");
        assert!(walk(&directory, &[r"C:\root"], WalkLimits::default()).is_empty());
    }

    #[test]
    fn unicode_and_spaces_in_paths_survive() {
        let directory = InMemoryDirectory::new()
            .with_dir(r"C:\root")
            .with_file(r"C:\root\Ünïcödé fïle.txt")
            .with_file(r"C:\root\日本語 ファイル.txt");
        let items = FileSearchProvider::new(
            &directory,
            [PathBuf::from(r"C:\root")],
            WalkLimits::default(),
        )
        .walk()
        .expect("walk");
        let titles: Vec<&str> = items.iter().map(|item| item.title.as_str()).collect();
        assert!(titles.contains(&"Ünïcödé fïle.txt"), "{titles:?}");
        assert!(titles.contains(&"日本語 ファイル.txt"), "{titles:?}");
        assert!(items.iter().all(|item| item.id.contains("file:")));
    }
}
