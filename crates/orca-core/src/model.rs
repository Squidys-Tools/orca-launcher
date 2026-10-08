//! The domain types: what a search result *is*.
//!
//! These are the only types the UI, the platform layer, and the providers all
//! have to agree on. They are plain data with no handles in them, so a
//! `ResultItem` can be built on a background thread, cached, compared, and
//! serialised without dragging a window along.

use crate::frecency::Frecency;

/// Where a [`ResultItem`] came from.
///
/// The source is three things at once: provenance for the UI, a hint for
/// affordance decisions, and one input to [`crate::policy::RankingPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// An installed application (Start-menu entry, `.lnk`, App Paths key).
    Application,
    /// A file, as reported by a filesystem index.
    File,
    /// A folder, as reported by a filesystem index.
    Folder,
    /// A named, user-configured command or alias.
    Command,
    /// A configured web-search shortcut.
    WebSearch,
    /// A computed answer, e.g. an arithmetic or unit expression.
    Calculator,
    /// A recently copied clipboard entry.
    Clipboard,
    /// A `Source` whose backing provider is not wired up yet.
    Unknown,
}

impl Source {
    /// Every variant, in declaration order.
    ///
    /// The order is load-bearing: it is the index space of
    /// [`crate::policy::WeightTable`], and it is what lets a weight table be a
    /// fixed-size array instead of a map.
    pub const ALL: [Source; 8] = [
        Source::Application,
        Source::File,
        Source::Folder,
        Source::Command,
        Source::WebSearch,
        Source::Calculator,
        Source::Clipboard,
        Source::Unknown,
    ];

    /// Number of variants. Kept next to [`Source::ALL`] so the two cannot
    /// drift apart silently.
    pub const COUNT: usize = 8;

    /// Dense index into the weight table, in `0..Self::COUNT`.
    pub const fn index(self) -> usize {
        match self {
            Source::Application => 0,
            Source::File => 1,
            Source::Folder => 2,
            Source::Command => 3,
            Source::WebSearch => 4,
            Source::Calculator => 5,
            Source::Clipboard => 6,
            Source::Unknown => 7,
        }
    }

    /// Rebuilds a `Source` from [`Source::index`].
    ///
    /// # Panics
    ///
    /// Panics if `index >= Source::COUNT`. Callers outside this crate get an
    /// `Option` from [`Source::from_index`]; this one exists for the weight
    /// table, where the index is a private array offset and therefore already
    /// in range.
    const fn from_index_unchecked(index: usize) -> Source {
        Source::ALL[index]
    }

    /// The checked inverse of [`Source::index`], for decoding a persisted
    /// discriminator that an older or newer build may have written.
    pub const fn from_index(index: usize) -> Option<Source> {
        if index < Source::COUNT {
            Some(Source::from_index_unchecked(index))
        } else {
            None
        }
    }

    /// Default trust weight. See [`crate::policy::WeightTable::DEFAULT`].
    pub const fn weight(self) -> f64 {
        crate::policy::WeightTable::DEFAULT.get(self)
    }
}

/// What activating a [`ResultItem`] actually does.
///
/// Modelled as a value rather than a closure so that results can be cached,
/// compared, serialised, and produced on a background thread without dragging a
/// UI handle along.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LaunchTarget {
    /// Run a program, optionally with fixed arguments.
    Executable {
        /// Absolute path to the program.
        path: std::path::PathBuf,
        /// Arguments passed on every activation.
        args: Vec<String>,
    },
    /// Open a URI in the user's default handler.
    Uri(String),
    /// Run a named command resolved through `PATH` rather than by path.
    Command {
        /// Executable name as typed.
        program: String,
        /// Arguments passed on every activation.
        args: Vec<String>,
    },
}

/// One activatable thing in the launcher's result list.
///
/// Two independent relevance signals hang off it, and keeping them separate is
/// the point:
///
/// * [`ResultItem::score`] is the **provider's** own prior, produced by
///   whatever produced this item — a filesystem mtime, an MRU table, a
///   provider-side heuristic. It knows nothing about the query.
/// * [`ResultItem::frecency`] is **orca's own** launch history, read from the
///   store in [`crate::store`]. It also knows nothing about the query.
///
/// [`crate::rank`] is the only thing that sees the query, and it is the only
/// thing allowed to combine these with it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultItem {
    /// Stable, source-scoped identity, e.g. `app:outlook` or `file:0x1a4f`.
    ///
    /// Used as the element key in the UI, so it must be unique within a single
    /// result list and stable across refreshes. It is also the primary key of
    /// the frecency table, which is why providers are responsible for making
    /// it stable across runs.
    pub id: String,
    /// Primary label shown in the result row, and the text the query is
    /// matched against.
    pub title: String,
    /// Secondary label: a hostname, a folder name, a description. `None` renders
    /// as no second line rather than an empty one. Also matched, at a discount.
    ///
    /// **This is what the user reads, so it must never be a filesystem path.**
    /// See [`ResultItem::keywords`] for the text that is searchable precisely
    /// because it is never displayed, and [`crate::looks_like_path`] for the
    /// rule this field is held to.
    pub subtitle: Option<String>,
    /// Extra text the query is matched against and that is **never rendered**.
    ///
    /// This exists because [`ResultItem::subtitle`] cannot be both the thing on
    /// screen and the thing we search. A file's full path is worth matching —
    /// typing `downloads` should find a file in Downloads — and is not worth
    /// reading, because a row of `C:\Users\chris\Downloads\2024\report-final.docx`
    /// is noise on every single result. Splitting the two roles means neither
    /// has to compromise.
    pub keywords: Option<String>,
    /// Which kind of thing this is.
    pub source: Source,
    /// Provider-supplied prior relevance in `0.0 ..= 1.0`.
    pub score: f64,
    /// Launch history for this id, or [`Frecency::NEVER`] if it has never been
    /// launched.
    pub frecency: Frecency,
    /// What happens when the user activates this item.
    pub target: LaunchTarget,
}

impl ResultItem {
    /// Builds an item with no prior score and no history.
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        source: Source,
        target: LaunchTarget,
    ) -> ResultItem {
        ResultItem {
            id: id.into(),
            title: title.into(),
            subtitle: None,
            keywords: None,
            source,
            score: 0.0,
            frecency: Frecency::NEVER,
            target,
        }
    }

    /// Builder-style setter for [`ResultItem::subtitle`].
    ///
    /// An empty or whitespace-only string becomes `None`.
    ///
    /// The value is stored as given rather than filtered. Refusing a
    /// path-shaped string here would make the rule true by construction, but it
    /// would also silently discard it, and a caller who cannot tell the
    /// difference between "rejected" and "stored" will eventually depend on the
    /// wrong one. The rule is held by `no_provider_renders_a_path` instead,
    /// which fails loudly and points here.
    #[must_use]
    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> ResultItem {
        let subtitle = subtitle.into();
        self.subtitle = if subtitle.trim().is_empty() {
            None
        } else {
            Some(subtitle)
        };
        self
    }

    /// Builder-style setter for [`ResultItem::keywords`].
    ///
    /// Blank strings become `None`, exactly as in [`ResultItem::with_subtitle`].
    #[must_use]
    pub fn with_keywords(mut self, keywords: impl Into<String>) -> ResultItem {
        let keywords = keywords.into();
        self.keywords = if keywords.trim().is_empty() {
            None
        } else {
            Some(keywords)
        };
        self
    }

    /// Builder-style setter for [`ResultItem::score`].
    ///
    /// Out-of-range and non-finite values are sanitised rather than clamped:
    /// `f64::clamp` propagates `NaN`, and a single `NaN` in a catalog would
    /// silently poison the sort comparator. `NaN` becomes `0.0`.
    #[must_use]
    pub fn with_score(mut self, score: f64) -> ResultItem {
        self.score = crate::policy::clamp01(score);
        self
    }

    /// Builder-style setter for [`ResultItem::frecency`].
    #[must_use]
    pub fn with_frecency(mut self, frecency: Frecency) -> ResultItem {
        self.frecency = frecency;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frecency::Timestamp;

    fn exe(program: &str) -> LaunchTarget {
        LaunchTarget::Command {
            program: program.to_owned(),
            args: Vec::new(),
        }
    }

    #[test]
    fn with_subtitle_normalises_blank_to_none() {
        assert_eq!(
            ResultItem::new("app:note", "Notepad", Source::Application, exe("notepad"))
                .with_subtitle("C:\\a")
                .subtitle
                .as_deref(),
            Some("C:\\a")
        );
        assert_eq!(
            ResultItem::new("app:note", "Notepad", Source::Application, exe("notepad"))
                .with_subtitle("   ")
                .subtitle,
            None
        );
    }

    #[test]
    fn with_score_sanitises_rather_than_propagating_nan() {
        let item = ResultItem::new("app:note", "Notepad", Source::Application, exe("n"));
        assert_eq!(item.clone().with_score(4.2).score, 1.0);
        assert_eq!(item.clone().with_score(-1.0).score, 0.0);
        // The one that matters: `f64::clamp` returns NaN for NaN, which would
        // make every comparison involving this item fall through to the
        // tiebreakers and hide a real ordering bug.
        assert_eq!(item.with_score(f64::NAN).score, 0.0);
    }

    #[test]
    fn with_frecency_defaults_to_never() {
        let item = ResultItem::new("app:note", "Notepad", Source::Application, exe("n"));
        assert_eq!(item.frecency, Frecency::NEVER);
        let used = item.with_frecency(Frecency::new(3, Some(Timestamp::from_unix_seconds(1))));
        assert_eq!(used.frecency.launches(), 3);
    }

    #[test]
    fn equality_covers_every_field_including_the_target() {
        let a = ResultItem::new("app:note", "Notepad", Source::Application, exe("n"))
            .with_subtitle("C:\\a")
            .with_score(0.5);
        let mut b = a.clone();
        assert_eq!(a, b);

        b.target = LaunchTarget::Uri("https://example.com".into());
        assert_ne!(a, b);
    }

    #[test]
    fn source_indices_round_trip_and_are_dense() {
        let mut seen = [false; Source::COUNT];
        for (expected, source) in Source::ALL.iter().copied().enumerate() {
            assert_eq!(source.index(), expected, "{source:?} has the wrong index");
            assert!(!seen[expected], "index {expected} used twice");
            seen[expected] = true;
            assert_eq!(Source::from_index(expected), Some(source));
        }
        assert_eq!(Source::from_index(Source::COUNT), None);
        assert!(seen.iter().all(|hit| *hit), "index space has a hole");
    }

    #[test]
    fn default_weights_are_ordered_and_bounded() {
        // The ordering is a product decision, not an accident. An application is
        // what the launcher is for; an unknown provider should never win a tie.
        assert!(Source::Application.weight() > Source::Command.weight());
        assert!(Source::Command.weight() > Source::Folder.weight());
        assert!(Source::Folder.weight() > Source::File.weight());
        assert!(Source::File.weight() >= Source::WebSearch.weight());
        assert!(Source::File.weight() > Source::Clipboard.weight());
        assert!(Source::Clipboard.weight() > Source::Unknown.weight());

        for source in Source::ALL {
            let weight = source.weight();
            assert!((0.0..=1.0).contains(&weight), "{source:?} weight {weight}");
        }
    }
}
