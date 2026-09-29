//! The provider abstraction, and the providers that need no Win32.
//!
//! A provider answers one question: *what could the user mean?* It returns
//! candidates, not results. Filtering, scoring, and ordering are
//! [`crate::rank`]'s job, and a provider that tries to rank is a provider whose
//! ordering the UI cannot explain or override.
//!
//! # The seam
//!
//! [`ResultProvider`] is what `orca-win` implements for installed applications
//! and open windows. Those need the registry and `EnumWindows`, so they cannot
//! live here. What *can* live here is everything that is pure Rust, and that is
//! most of the useful surface:
//!
//! | provider | source | needs |
//! |---|---|---|
//! | [`CommandProvider`] | [`Source::Command`] | nothing but the config |
//! | [`EnvVarProvider`] | [`Source::Command`] | an injected variable list |
//! | [`FileSearchProvider`] | [`Source::File`] / [`Source::Folder`] | an injected [`DirectoryLister`] |
//!
//! # Why the directory seam exists
//!
//! Layering rule 2 puts the filesystem outside this crate. So
//! [`FileSearchProvider`] — the traversal, the depth limit, the cycle guard, the
//! result cap — is a pure state machine over an injected [`DirectoryLister`],
//! and [`InMemoryDirectory`] is a complete implementation of it. The `std::fs`
//! adapter is a dozen lines and belongs to `orca`, which is the crate that owns
//! the composition root. What that buys is that the traversal is testable
//! against fixtures containing a junction loop, a permission denial, and a
//! 40-level-deep tree — none of which a real filesystem makes cheap to produce.
//!
//! # `collect` is blocking on purpose
//!
//! The trait is synchronous because the *caller* owns the concurrency: `orca`
//! runs it on a `BackgroundExecutor` via `cx.spawn`. A provider that spawned its
//! own thread would make cancellation impossible at exactly the moment the user
//! presses <kbd>Esc</kbd>.

use std::error::Error;
use std::fmt;

use crate::model::{LaunchTarget, ResultItem, Source};

/// One candidate, before ranking.
#[derive(Debug, Clone, PartialEq)]
pub struct RawResult {
    /// Stable, provider-scoped id. See [`ResultItem::id`].
    pub id: String,
    /// What the user reads.
    pub title: String,
    /// Optional second line. Blank strings are normalised to `None`.
    pub subtitle: Option<String>,
    /// Which kind of thing this is.
    pub source: Source,
    /// The provider's own prior, in `0.0 ..= 1.0`.
    ///
    /// A provider should only set this from something the *user* did — a file's
    /// mtime, a position in an MRU table. Anything that depends on the query
    /// belongs in the matcher, not here.
    pub score: f64,
    /// What activation does.
    pub target: LaunchTarget,
}

impl RawResult {
    /// Builds a candidate with no prior and no subtitle.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        source: Source,
        target: LaunchTarget,
    ) -> RawResult {
        RawResult {
            id: id.into(),
            title: title.into(),
            subtitle: None,
            source,
            score: 0.0,
            target,
        }
    }

    /// Builder-style setter for [`RawResult::subtitle`]; blank becomes `None`.
    #[must_use]
    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> RawResult {
        let subtitle = subtitle.into();
        self.subtitle = if subtitle.trim().is_empty() {
            None
        } else {
            Some(subtitle)
        };
        self
    }

    /// Builder-style setter for [`RawResult::score`], sanitised to `0.0 ..= 1.0`.
    #[must_use]
    pub fn with_score(mut self, score: f64) -> RawResult {
        self.score = crate::policy::clamp01(score);
        self
    }

    /// Converts to the domain type the ranker takes.
    ///
    /// `frecency` is passed in rather than read: the store lookup belongs to the
    /// caller, and reading it here would mean every provider needed a store.
    #[must_use]
    pub fn into_item(self, frecency: crate::frecency::Frecency) -> ResultItem {
        ResultItem {
            id: self.id,
            title: self.title,
            subtitle: self.subtitle,
            source: self.source,
            score: self.score,
            frecency,
            target: self.target,
        }
    }
}

/// A source of candidates.
///
/// Implementations live here (pure Rust) and in `orca-win` (installed apps,
/// window list). Neither knows about the other.
pub trait ResultProvider: Send + Sync {
    /// A stable name for logs and for de-duplicating against another provider.
    fn name(&self) -> &str;

    /// Collects candidates.
    ///
    /// Returns *everything* the provider can offer, not a filtered subset. The
    /// query is applied once, by the ranker, so two providers can never
    /// disagree about what "matches".
    fn collect(&self) -> Result<Vec<RawResult>, ProviderError>;

    /// An empty provider registered under `name`. Provided so callers can build
    /// a [`ProviderSet`] without every provider having a bespoke constructor.
    fn empty(name: &str) -> ProviderBox
    where
        Self: Sized,
    {
        ProviderBox::new(name, EmptyProvider)
    }
}

/// A provider that never has anything to say.
///
/// Exists so a disabled feature is a value rather than an `Option` threaded
/// through the whole UI, and so "this provider is off" needs no special case
/// anywhere downstream.
#[derive(Debug, Clone, Copy)]
pub struct EmptyProvider;

impl ResultProvider for EmptyProvider {
    fn name(&self) -> &str {
        "empty"
    }

    fn collect(&self) -> Result<Vec<RawResult>, ProviderError> {
        Ok(Vec::new())
    }
}

/// A named, type-erased provider.
///
/// The name is stored alongside so a set can report which providers are wired
/// up — including the [`EmptyProvider`] placeholders — without having to call
/// into them. The `+ Send + Sync` on the box is not decoration: a provider set
/// is built on the UI thread and collected on a background one.
pub struct ProviderBox {
    name: String,
    provider: Box<dyn ResultProvider + Send + Sync>,
}

impl ProviderBox {
    /// Wraps a provider under an explicit name.
    #[must_use]
    pub fn new(name: impl Into<String>, provider: impl ResultProvider + 'static) -> ProviderBox {
        ProviderBox {
            name: name.into(),
            provider: Box::new(provider),
        }
    }

    /// The name this provider was registered under.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Collects from the wrapped provider.
    pub fn collect(&self) -> Result<Vec<RawResult>, ProviderError> {
        self.provider.collect()
    }
}

impl fmt::Debug for ProviderBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderBox")
            .field("name", &self.name)
            .finish()
    }
}

/// A collection of providers that merges their output.
///
/// De-duplication is by [`RawResult::id`], **first provider wins**. That is
/// deliberate: a provider registered earlier is a more authoritative one, and
/// "later overrides earlier" would make the result depend on registration
/// order in a way nothing else in the crate does.
#[derive(Debug, Default)]
pub struct ProviderSet {
    providers: Vec<ProviderBox>,
}

impl ProviderSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> ProviderSet {
        ProviderSet::default()
    }

    /// Adds a provider, appending it after any already registered.
    #[must_use]
    pub fn with(
        mut self,
        name: impl Into<String>,
        provider: impl ResultProvider + 'static,
    ) -> ProviderSet {
        self.providers.push(ProviderBox::new(name, provider));
        self
    }

    /// The registered names, in registration order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.providers.iter().map(ProviderBox::name).collect()
    }

    /// Collects from every provider, in order, dropping duplicate ids.
    ///
    /// The first provider that fails stops the collection and the error names
    /// it. A partial list silently missing one source is worse than a list the
    /// caller can log and retry — a launcher that silently loses installed apps
    /// because the registry read failed is a launcher nobody can debug.
    pub fn collect_all(&self) -> Result<Vec<RawResult>, ProviderError> {
        let mut merged: Vec<RawResult> = Vec::new();
        for provider in &self.providers {
            let results = provider.collect().map_err(|source| ProviderError::Failed {
                provider: provider.name().to_owned(),
                source: Box::new(source),
            })?;
            for result in results {
                if !merged.iter().any(|existing| existing.id == result.id) {
                    merged.push(result);
                }
            }
        }
        Ok(merged)
    }

    /// Collects from every provider and converts to domain items, stamping
    /// `frecency` from `history`.
    ///
    /// The one call the composition root needs, so the frecency overlay happens
    /// in exactly one place.
    pub fn collect_ranked_items(
        &self,
        history: &dyn crate::store::UsageStore,
    ) -> Result<Vec<ResultItem>, ProviderError> {
        use crate::store::UsageStoreExt;
        let history = history
            .frecency_map()
            .map_err(|error| ProviderError::History(Box::new(error)))?;
        Ok(self
            .collect_all()?
            .into_iter()
            .map(|raw| {
                let frecency = history
                    .get(&raw.id)
                    .copied()
                    .unwrap_or(crate::Frecency::NEVER);
                raw.into_item(frecency)
            })
            .collect())
    }
}

/// Everything a provider can fail with.
#[derive(Debug)]
pub enum ProviderError {
    /// A directory could not be listed.
    Io {
        /// The path that failed.
        path: std::path::PathBuf,
        /// The OS error, rendered.
        detail: String,
    },
    /// A provider failed, and the set is reporting which one.
    Failed {
        /// The registered name of the provider that failed.
        provider: String,
        /// What it reported.
        source: Box<ProviderError>,
    },
    /// The launch history could not be read, so nothing could be ranked.
    History(Box<crate::store::StoreError>),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Io { path, detail } => {
                write!(f, "could not list {}: {detail}", path.display())
            }
            ProviderError::Failed { provider, source } => {
                write!(f, "provider {provider:?} failed: {source}")
            }
            ProviderError::History(source) => {
                write!(f, "could not read launch history: {source}")
            }
        }
    }
}

impl Error for ProviderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ProviderError::Failed { source, .. } => Some(source.as_ref()),
            ProviderError::History(source) => Some(source.as_ref()),
            ProviderError::Io { .. } => None,
        }
    }
}

pub mod commands;
pub mod env;
pub mod files;

pub use commands::{Alias, CommandProvider};
pub use env::{EnvVar, EnvVarProvider};
pub use files::{DirectoryLister, FileSearchProvider, InMemoryDirectory, WalkLimits};
