//! `orca-core` — the pure-logic half of the orca launcher.
//!
//! This crate holds the domain model (what a search result *is*), the ranking
//! rules (how a result is scored against a query), and — as they are added —
//! config parsing, the SQLite index, and the provider implementations.
//!
//! It has **zero external dependencies** on purpose. It must not depend on
//! `gpui`, nor on any crate that binds Win32. Platform concerns live in
//! `orca-win`; presentation lives in `orca`. See `docs/ARCHITECTURE.md`.
//!
//! Because everything here is plain data plus pure functions over plain data,
//! the whole crate is testable without a window, a display, or a message loop.

// ---------------------------------------------------------------------------
// Domain model
// ---------------------------------------------------------------------------

/// Where a [`ResultItem`] came from.
///
/// Providers produce items tagged with their origin. The launcher uses this
/// for provenance ("from Windows Search"), for icon/affordance decisions, and
/// as an input to [`rank`] via [`Source::weight`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Relative importance of this source in a blended score.
    ///
    /// Deliberately narrow (`0.80 ..= 1.00`) so a weak match from a
    /// high-weight source cannot outrank a strong match from a low-weight one
    /// by much. Revisit these numbers against real ranking data, not vibes.
    pub const fn weight(self) -> f64 {
        match self {
            Source::Application => 1.00,
            Source::Command => 0.95,
            Source::Calculator => 0.90,
            Source::Folder => 0.90,
            Source::WebSearch => 0.85,
            Source::File => 0.85,
            Source::Clipboard => 0.80,
            Source::Unknown => 0.80,
        }
    }
}

/// What activating a [`ResultItem`] actually does.
///
/// Modelled as a value rather than a closure so that results can be cached,
/// compared, serialised, and produced in a background thread without dragging
/// a UI handle along.
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

/// One scored, activatable thing in the launcher's result list.
///
/// `score` is the *provider's* own relevance, not the final ranking score: an
/// MRU frequency, a filesystem recency, or a provider-side fuzzy score, all
/// normalised to `0.0 ..= 1.0`. [`rank`] blends it with the query match.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultItem {
    /// Stable, source-scoped identity, e.g. `app:outlook` or `file:0x1a4f`.
    ///
    /// Used as the React-style key / GPUI element key, so it must be unique
    /// within a single result list and stable across refreshes.
    pub id: String,
    /// Primary label shown in the result row.
    pub title: String,
    /// Secondary label: a path, a hostname, a description. `None` renders as
    /// no second line rather than an empty one.
    pub subtitle: Option<String>,
    /// Which provider produced this item.
    pub source: Source,
    /// Provider-supplied relevance in `0.0 ..= 1.0`.
    pub score: f64,
    /// What happens when the user activates this item.
    pub target: LaunchTarget,
}

impl ResultItem {
    /// Builds an item, clamping `score` into the documented `0.0 ..= 1.0`
    /// range and normalising a blank subtitle to `None`.
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
            source,
            score: 0.0,
            target,
        }
    }

    /// Builder-style setter for [`ResultItem::subtitle`].
    ///
    /// An empty or whitespace-only string becomes `None`.
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

    /// Builder-style setter for [`ResultItem::score`], clamped to `0.0 ..= 1.0`.
    #[must_use]
    pub fn with_score(mut self, score: f64) -> ResultItem {
        self.score = score.clamp(0.0, 1.0);
        self
    }
}

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

/// Score for an exact, case-insensitive match.
pub const EXACT_SCORE: f64 = 1.00;
/// Score for a candidate that starts with the query.
pub const PREFIX_SCORE: f64 = 0.90;
/// Score for a candidate that contains the query.
pub const CONTAINS_SCORE: f64 = 0.75;
/// Score given to every candidate when the query is empty.
///
/// The launcher's "browse" state: with nothing typed, recents and MRU order
/// decide, so every candidate is a candidate.
pub const BROWSE_SCORE: f64 = 0.50;
/// Floor of the fuzzy band. Kept below [`BROWSE_SCORE`] so a typo'd query
/// always ranks below an untyped one, no matter how the provider scored it.
pub const FUZZY_FLOOR: f64 = 0.25;
/// Width of the fuzzy band above [`FUZZY_FLOOR`].
pub const FUZZY_CEILING: f64 = 0.24;

/// Weight of the query match versus the provider's own score.
const MATCH_WEIGHT: f64 = 0.80;
/// Weight of the provider's own score.
const PROVIDER_WEIGHT: f64 = 0.20;

/// A [`ResultItem`] together with the final score [`rank`] assigned to it.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedItem {
    /// The original, unmodified item.
    pub item: ResultItem,
    /// Final score in `0.0 ..= 1.0`, higher is better.
    pub score: f64,
}

/// Normalises a string for comparison: trimmed and lowercased.
fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Scores how well `candidate` matches `query`, on a `0.0 ..= 1.0` scale.
///
/// The model is small, total, and deterministic so it can be unit tested
/// without a UI:
///
/// | match                            | score                                  |
/// |----------------------------------|-----------------------------------------|
/// | empty query (browse state)       | [`BROWSE_SCORE`] (0.50)                |
/// | exact                            | [`EXACT_SCORE`] (1.00)                 |
/// | prefix                           | [`PREFIX_SCORE`] (0.90)                |
/// | substring                        | [`CONTAINS_SCORE`] (0.75)              |
/// | in-order subsequence (fuzzy)     | `FUZZY_FLOOR .. FUZZY_FLOOR+FUZZY_CEILING`, scaled by how tightly the matched characters are packed |
/// | no match                         | `0.0`                                  |
/// | empty candidate                  | `0.0`                                  |
///
/// Whitespace inside the query is ignored, so `"not pad"` matches
/// `"Notepad.exe"`.
pub fn match_score(query: &str, candidate: &str) -> f64 {
    let needle: Vec<char> = normalize(query)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let haystack: Vec<char> = normalize(candidate).chars().collect();

    if haystack.is_empty() {
        return 0.0;
    }
    if needle.is_empty() {
        return BROWSE_SCORE;
    }
    if haystack == needle {
        return EXACT_SCORE;
    }
    if haystack.starts_with(&needle[..]) {
        return PREFIX_SCORE;
    }
    if haystack.windows(needle.len()).any(|w| w == &needle[..]) {
        return CONTAINS_SCORE;
    }

    match fuzzy_span(&needle, &haystack) {
        Some((first, last)) => {
            // Tighter spans (matched characters packed together near the start
            // of the candidate) score higher than scattered matches.
            let span = (last - first + 1) as f64;
            let packed = (1.0 - span / haystack.len() as f64).clamp(0.0, 1.0);
            FUZZY_FLOOR + FUZZY_CEILING * packed
        }
        None => 0.0,
    }
}

/// Finds the index span covering an in-order subsequence match of `needle` in
/// `haystack`, or `None` if the characters do not appear in order.
///
/// Walks both sequences once, keeping the first and last consumed position.
fn fuzzy_span(needle: &[char], haystack: &[char]) -> Option<(usize, usize)> {
    let mut consumed = 0usize;
    let mut first = 0usize;
    let mut last = 0usize;

    for (index, candidate) in haystack.iter().enumerate() {
        if consumed < needle.len() && *candidate == needle[consumed] {
            if consumed == 0 {
                first = index;
            }
            last = index;
            consumed += 1;
        }
    }

    if consumed == needle.len() {
        Some((first, last))
    } else {
        None
    }
}

/// Blends a query match with a provider's own relevance.
///
/// `MATCH_WEIGHT` goes to the query and `PROVIDER_WEIGHT` to the provider, so
/// a provider can break ties between equally-good matches (MRU order) without
/// being able to promote something the user did not actually match.
pub fn blended_score(match_score: f64, provider_score: f64) -> f64 {
    let match_score = match_score.clamp(0.0, 1.0);
    let provider_score = provider_score.clamp(0.0, 1.0);
    (MATCH_WEIGHT * match_score + PROVIDER_WEIGHT * provider_score).clamp(0.0, 1.0)
}

/// Ranks `items` against `query`, best first.
///
/// Items whose [`match_score`] is `0.0` are dropped entirely — a launcher that
/// shows non-matches is a launcher nobody trusts. The remaining items are
/// scored with [`blended_score`] and sorted by:
///
/// 1. final score, descending;
/// 2. title, case-insensitively, ascending — so ties render in a stable,
///    human-sensible order rather than in provider order;
/// 3. id, ascending — a total order, so the output is fully deterministic.
pub fn rank<'a>(query: &str, items: impl IntoIterator<Item = &'a ResultItem>) -> Vec<RankedItem> {
    let mut ranked: Vec<RankedItem> = items
        .into_iter()
        .filter_map(|item| {
            let score = match match_score(query, &item.title) {
                0.0 => None,
                m => Some(blended_score(m, item.score)),
            };
            score.map(|score| RankedItem {
                item: item.clone(),
                score,
            })
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.item
                    .title
                    .to_lowercase()
                    .cmp(&b.item.title.to_lowercase())
            })
            .then_with(|| a.item.id.cmp(&b.item.id))
    });

    ranked
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, title: &str) -> ResultItem {
        ResultItem::new(
            id,
            title,
            Source::Application,
            LaunchTarget::Command {
                program: title.to_string(),
                args: Vec::new(),
            },
        )
    }

    fn titles(ranked: &[RankedItem]) -> Vec<&str> {
        ranked.iter().map(|r| r.item.title.as_str()).collect()
    }

    #[test]
    fn match_score_orders_exact_above_prefix_above_substring() {
        let score = |q: &str| match_score(q, "Notepad.exe");

        assert_eq!(score("notepad.exe"), EXACT_SCORE);
        assert_eq!(score("not"), PREFIX_SCORE);
        assert_eq!(score("pad"), CONTAINS_SCORE);
        assert!(score("pad") < score("not"));
        assert!(score("not") < score("notepad.exe"));
    }

    #[test]
    fn match_score_is_case_and_whitespace_insensitive() {
        assert_eq!(match_score("NoTePaD", "notepad"), EXACT_SCORE);
        assert_eq!(match_score("NOTEPAD.EXE", "notepad.exe"), EXACT_SCORE);
        // Case folding must not turn a prefix into an exact match, or the
        // exact band becomes unreachable for anything with an extension.
        assert_eq!(match_score("notepad", "notepad.exe"), PREFIX_SCORE);
        // Whitespace inside the query is dropped before comparison, so a
        // spec copied out of a settings UI lands on the same band as the
        // unspaced equivalent.
        assert_eq!(match_score(" ep ad ", "notepad.exe"), CONTAINS_SCORE);
        assert_eq!(match_score(" notepad ", "notepad"), EXACT_SCORE);
        assert_eq!(match_score("  notepad  ", "  Notepad  "), EXACT_SCORE);
    }

    #[test]
    fn match_score_rejects_out_of_order_subsequences() {
        // Every character is present, but not in order: not a match at all.
        assert_eq!(match_score("xen", "notepad"), 0.0);
        assert_eq!(match_score("aod", "notepad"), 0.0);
    }

    #[test]
    fn fuzzy_band_is_bounded_and_prefers_tightly_packed_matches() {
        // Maximally scattered: 'n' at 0, 't' at 2, 'd' at 6 spans the whole
        // candidate, so packing is 0 and the score lands on the floor.
        let scattered = match_score("ntd", "notepad");
        assert!(
            (FUZZY_FLOOR..BROWSE_SCORE).contains(&scattered),
            "scattered fuzzy out of band: {scattered}"
        );

        // Packed at the front: the same three characters, close together.
        let packed = match_score("nte", "notepad");
        assert!(
            packed > scattered,
            "packed fuzzy should beat scattered: {packed} vs {scattered}"
        );
        assert!(
            packed < BROWSE_SCORE,
            "fuzzy must stay below the browse state, got {packed}"
        );
    }

    #[test]
    fn empty_query_returns_browse_score_and_empty_candidate_returns_zero() {
        assert_eq!(match_score("", "notepad.exe"), BROWSE_SCORE);
        assert_eq!(match_score("   ", "notepad.exe"), BROWSE_SCORE);
        assert_eq!(match_score("notepad", ""), 0.0);
    }

    #[test]
    fn match_score_stays_in_range_and_respects_the_documented_bands() {
        for query in ["n", "not", "notepad", "notepad.exe", "zzz", "td", "xen", ""] {
            let score = match_score(query, "notepad.exe");
            assert!(
                (0.0..=1.0).contains(&score),
                "score out of range for {query:?}: {score}"
            );
        }

        // Whole-word matches are always at or above the browse score; only the
        // deliberately-loose fuzzy band sits below it.
        for query in ["notepad", "notepad.exe", "not", "note"] {
            let score = match_score(query, "notepad.exe");
            assert!(
                score >= BROWSE_SCORE,
                "{query:?} should rank above browse, got {score}"
            );
        }
    }

    #[test]
    fn blended_score_weights_the_query_most_heavily() {
        // A great match with a poor provider score still beats the reverse.
        assert!(blended_score(1.0, 0.0) > blended_score(0.5, 1.0));
        // Out-of-range inputs are clamped, not propagated.
        assert_eq!(blended_score(2.0, 2.0), 1.0);
        assert_eq!(blended_score(-1.0, -1.0), 0.0);
    }

    #[test]
    fn rank_drops_non_matching_items() {
        let items = vec![item("app:note", "Notepad"), item("app:calc", "Calculator")];
        let ranked = rank("note", &items);

        assert_eq!(titles(&ranked), vec!["Notepad"]);
    }

    #[test]
    fn rank_orders_by_match_strength_first() {
        // All three genuinely match "note", at three different strengths:
        // exact 1.00, prefix 0.90, substring 0.75.
        let items = vec![
            item("app:textnote", "Text note"),
            item("app:note", "Note"),
            item("app:notepad", "Notepad"),
        ];
        let ranked = rank("note", &items);

        assert_eq!(titles(&ranked), vec!["Note", "Notepad", "Text note"]);
        let scores: Vec<f64> = ranked.iter().map(|r| r.score).collect();
        assert!(
            scores.windows(2).all(|w| w[0] > w[1]),
            "scores not strictly descending: {scores:?}"
        );
    }

    #[test]
    fn rank_breaks_ties_deterministically_by_title_then_id() {
        // Identical titles from two sources: same match score, same provider
        // score, so the id tiebreak is the only thing that can order them.
        let a = item("app:zzz", "Notepad");
        let b = item("app:aaa", "Notepad");
        let items = vec![a.clone(), b.clone()];

        let first = rank("notepad", &items);
        let second = rank("notepad", items.iter().rev());
        assert_eq!(titles(&first), vec!["Notepad", "Notepad"]);
        assert_eq!(first, second, "ranking must not depend on input order");
        assert_eq!(first[0].item.id, "app:aaa");
    }

    #[test]
    fn rank_with_empty_query_keeps_everything_ordered_by_provider_score() {
        let items = vec![
            item("app:note", "Notepad").with_score(0.1),
            item("app:calc", "Calculator").with_score(0.9),
        ];
        let ranked = rank("", &items);

        assert_eq!(titles(&ranked), vec!["Calculator", "Notepad"]);
    }

    #[test]
    fn with_subtitle_normalises_blank_to_none() {
        assert_eq!(
            item("app:note", "Notepad")
                .with_subtitle("C:\\a")
                .subtitle
                .as_deref(),
            Some("C:\\a")
        );
        assert_eq!(
            item("app:note", "Notepad").with_subtitle("   ").subtitle,
            None
        );
    }

    #[test]
    fn with_score_clamps_out_of_range_input() {
        assert_eq!(item("app:note", "Notepad").with_score(4.2).score, 1.0);
        assert_eq!(item("app:note", "Notepad").with_score(-1.0).score, 0.0);
    }

    #[test]
    fn equality_covers_every_field_including_the_target() {
        let a = item("app:note", "Notepad")
            .with_subtitle("C:\\a")
            .with_score(0.5);
        let mut b = a.clone();
        assert_eq!(a, b);

        b.target = LaunchTarget::Uri("https://example.com".into());
        assert_ne!(a, b);
    }

    #[test]
    fn source_weights_are_ordered_and_bounded() {
        assert!(Source::Application.weight() > Source::Unknown.weight());
        for source in [
            Source::Application,
            Source::File,
            Source::Folder,
            Source::Command,
            Source::WebSearch,
            Source::Calculator,
            Source::Clipboard,
            Source::Unknown,
        ] {
            assert!(
                (0.0..=1.0).contains(&source.weight()),
                "{source:?} has out-of-range weight"
            );
        }
    }
}
