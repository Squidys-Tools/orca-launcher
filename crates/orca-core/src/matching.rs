//! Match quality: how well does this candidate answer this query?
//!
//! The tiers, best to worst, are exact, prefix, word-boundary, substring,
//! browse, and fuzzy. Each is a fixed score, and the order between them is a
//! product decision with a rationale, not a guess:
//!
//! * **Exact** — the user typed the whole name. There is nothing better.
//! * **Prefix** — the strongest signal short of exact. Typing `note` and
//!   meaning `Notepad` is the normal case, and a prefix is also the only place
//!   acronym-style matching (`np` -> `Notepad`) should score highly.
//! * **Word boundary** — the query matches a whole word but does not start the
//!   name. `downloads` finding `My Downloads` is a real answer that a plain
//!   substring match would bury alongside `downloader.exe` and
//!   `undownloads.sh`.
//! * **Substring** — present, but only as a fragment. Correct often enough to
//!   keep, weak enough to sit below the tiers above.
//! * **Browse** — nothing was typed. Every candidate is a candidate, so this is
//!   a constant and cannot affect the ordering at all.
//! * **Fuzzy** — the characters appear, in order, scattered. A typo tolerance.
//!   Deliberately capped *below* [`BROWSE_SCORE`] so a misspelling can never
//!   outrank the untyped recents list.
//!
//! Everything is a pure function of two `&str`. No clock, no I/O, no
//! allocation beyond the two `Vec<char>` needed to make case folding
//! length-safe.

use crate::text::{fold, fold_query, is_word_char};

/// The candidate equals the query.
pub const EXACT_SCORE: f64 = 1.00;
/// The candidate starts with the query.
pub const PREFIX_SCORE: f64 = 0.92;
/// The query matches a whole word inside the candidate, but not at the front.
pub const WORD_BOUNDARY_SCORE: f64 = 0.85;
/// The query appears inside the candidate as a fragment.
pub const CONTAINS_SCORE: f64 = 0.75;

/// Alias for [`CONTAINS_SCORE`], for callers that would rather name the tier
/// than the verb.
pub const SUBSTRING_SCORE: f64 = CONTAINS_SCORE;
/// Score given to every candidate when the query is empty.
///
/// The launcher's "browse" state: with nothing typed, recents and MRU order
/// decide, so every candidate is a candidate.
pub const BROWSE_SCORE: f64 = 0.50;
/// Floor of the fuzzy band. Kept below [`BROWSE_SCORE`] so a typo'd query
/// always ranks below an untyped one, no matter how the history looks.
pub const FUZZY_FLOOR: f64 = 0.20;
/// Width of the fuzzy band above [`FUZZY_FLOOR`].
///
/// `FUZZY_FLOOR + FUZZY_CEILING == 0.48 < BROWSE_SCORE`, which is the invariant
/// that keeps the whole fuzzy tier under the browse state. Both halves are
/// asserted in the tests rather than left to a reader to verify.
pub const FUZZY_CEILING: f64 = 0.28;

/// Multiplier applied to a match found only in the subtitle.
///
/// Searching `downloads` should find a file whose *path* contains it. But a
/// title match is a stronger signal than a path match, so a subtitle hit is
/// discounted rather than promoted to parity.
pub const SUBTITLE_DISCOUNT: f64 = 0.90;

/// The tier a candidate landed in. Derived `Ord` is best to worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchKind {
    /// The folded candidate equals the folded query.
    Exact,
    /// The folded candidate starts with the query.
    Prefix,
    /// The query matches at the start of a word that is not the first one.
    WordBoundary,
    /// The query appears as a fragment.
    Substring,
    /// The query was empty: every candidate qualifies equally.
    Browse,
    /// The query's characters appear in order, scattered.
    Fuzzy,
    /// The query's characters do not appear in order at all.
    None,
}

impl MatchKind {
    /// The fixed score of every tier except [`MatchKind::Fuzzy`].
    #[must_use]
    pub const fn fixed_score(self) -> f64 {
        match self {
            MatchKind::Exact => EXACT_SCORE,
            MatchKind::Prefix => PREFIX_SCORE,
            MatchKind::WordBoundary => WORD_BOUNDARY_SCORE,
            MatchKind::Substring => CONTAINS_SCORE,
            MatchKind::Browse => BROWSE_SCORE,
            MatchKind::None => 0.0,
            // Scaled by span in `analyze`, which is the only place that knows
            // how tightly the characters packed.
            MatchKind::Fuzzy => FUZZY_FLOOR,
        }
    }

    /// Whether this tier is a real hit rather than a guess.
    ///
    /// A caller deciding "do I show this at all?" wants this, not
    /// `score > 0.0`, because a fuzzy hit is a guess and [`MatchKind::None`]
    /// is not a guess at all. [`MatchKind::Browse`] counts as real: an empty
    /// query really does match everything.
    #[must_use]
    pub const fn is_real(self) -> bool {
        !matches!(self, MatchKind::Fuzzy | MatchKind::None)
    }
}

/// A match, with both the tier and the number, so callers never re-derive one
/// from the other — and in particular never re-derive the fuzzy score without
/// the span it was computed from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchQuality {
    /// Which tier the candidate landed in.
    pub kind: MatchKind,
    /// Score in `0.0 ..= 1.0`.
    pub score: f64,
}

/// Scores how well `candidate` matches `query`, on a `0.0 ..= 1.0` scale.
///
/// See the module docs for the tier table. Whitespace inside the query is
/// ignored, so `"not pad"` matches `"Notepad.exe"`.
///
/// # Examples
///
/// ```
/// use orca_core::{match_score, CONTAINS_SCORE, EXACT_SCORE, PREFIX_SCORE};
///
/// assert_eq!(match_score("notepad.exe", "Notepad.exe"), EXACT_SCORE);
/// assert_eq!(match_score("note", "Notepad"), PREFIX_SCORE);
/// assert_eq!(match_score("note", "Text note"), orca_core::WORD_BOUNDARY_SCORE);
/// assert_eq!(match_score("pad", "Notepad.exe"), CONTAINS_SCORE);
/// assert_eq!(match_score("xen", "notepad"), 0.0);
/// ```
#[must_use]
pub fn match_score(query: &str, candidate: &str) -> f64 {
    analyze(query, candidate, 1.0).score
}

/// Like [`match_score`], but also looks at `subtitle`, discounted by
/// [`SUBTITLE_DISCOUNT`].
///
/// The better of the two wins. With an empty query the discounted browse score
/// (`0.45`) loses to the title's [`BROWSE_SCORE`] (`0.5`), so the browse state
/// stays decided by title and history alone — the subtitle is only allowed to
/// help once the user has actually typed something.
#[must_use]
pub fn match_score_with_subtitle(query: &str, title: &str, subtitle: Option<&str>) -> f64 {
    match_quality_with_subtitle(query, title, subtitle).score
}

/// Like [`match_score_with_subtitle`], but keeps the tier as well as the score.
///
/// Crate-internal: the tier is only meaningful to [`crate::policy`], which is
/// the one caller that needs to apply the min-fuzzy-length gate without losing
/// the score.
pub(crate) fn match_quality_with_subtitle(
    query: &str,
    title: &str,
    subtitle: Option<&str>,
) -> MatchQuality {
    let from_title = analyze(query, title, 1.0);
    let from_subtitle = analyze_subtitle(query, subtitle);
    if from_subtitle.score > from_title.score {
        from_subtitle
    } else {
        from_title
    }
}

fn analyze_subtitle(query: &str, subtitle: Option<&str>) -> MatchQuality {
    let Some(subtitle) = subtitle.filter(|value| !value.trim().is_empty()) else {
        return MatchQuality {
            kind: MatchKind::None,
            score: 0.0,
        };
    };
    analyze(query, subtitle, SUBTITLE_DISCOUNT)
}

/// Classifies a query/candidate pair into a tier, ignoring subtitle matches.
///
/// Useful when the UI wants to *show* why something matched — highlighting the
/// matched word — rather than just order by it.
#[must_use]
pub fn classify(query: &str, candidate: &str) -> MatchKind {
    analyze(query, candidate, 1.0).kind
}

/// Full analysis of one string, with every score multiplied by `discount`.
///
/// `discount` exists so the title and subtitle rules are one code path rather
/// than two, which is the only way to guarantee the tiers stay identical
/// between them.
fn analyze(query: &str, candidate: &str, discount: f64) -> MatchQuality {
    let needle = fold_query(query);
    let haystack = fold(candidate);

    if haystack.is_empty() {
        // An empty candidate can never be a match, not even a browse one: there
        // is no text for the user to have meant.
        return MatchQuality {
            kind: MatchKind::None,
            score: 0.0,
        };
    }
    if needle.is_empty() {
        return MatchQuality {
            kind: MatchKind::Browse,
            score: BROWSE_SCORE * discount,
        };
    }
    if needle.len() > haystack.len() {
        // Cannot be exact, a prefix, or a substring, and an in-order
        // subsequence of a longer needle is impossible in a shorter haystack.
        // Skipping the scan here is the difference between O(n) and O(n*m) on
        // every keystroke.
        return MatchQuality {
            kind: MatchKind::None,
            score: 0.0,
        };
    }

    if haystack == needle {
        return fixed(MatchKind::Exact, discount);
    }
    if haystack.starts_with(needle.as_slice()) {
        return fixed(MatchKind::Prefix, discount);
    }

    let substring_at_boundary = (0..=haystack.len() - needle.len()).any(|start| {
        haystack[start..start + needle.len()] == needle[..]
            && start > 0
            && !is_word_char(haystack[start - 1])
    });
    if substring_at_boundary {
        return fixed(MatchKind::WordBoundary, discount);
    }
    if haystack
        .windows(needle.len())
        .any(|w| w == needle.as_slice())
    {
        return fixed(MatchKind::Substring, discount);
    }

    match fuzzy_span(&needle, &haystack) {
        Some((first, last)) => {
            // Packing is measured against the range that is *achievable for a
            // fuzzy match*, not against zero. A span of exactly `needle.len()`
            // means the matched characters are contiguous, which the substring
            // check above has already claimed — so the best a fuzzy match can do
            // is one skipped character. Normalising against
            // `needle.len() + 1 ..= haystack.len()` therefore makes both ends of
            // the band genuinely reachable, which is what stops `FUZZY_CEILING`
            // from being decoration: a ceiling nothing can reach is a number
            // nobody should be tuning.
            let span = (last - first + 1) as f64;
            let tightest = needle.len() as f64 + 1.0;
            let loosest = haystack.len() as f64;
            let packing = if loosest > tightest {
                ((loosest - span) / (loosest - tightest)).clamp(0.0, 1.0)
            } else {
                // Same length and not equal, so this cannot be an in-order
                // subsequence at all and `fuzzy_span` will not have matched.
                // Unreachable via `analyze`; kept so a future change cannot turn
                // it into a division by zero.
                0.0
            };
            MatchQuality {
                kind: MatchKind::Fuzzy,
                score: (FUZZY_FLOOR + FUZZY_CEILING * packing) * discount,
            }
        }
        None => MatchQuality {
            kind: MatchKind::None,
            score: 0.0,
        },
    }
}

fn fixed(kind: MatchKind, discount: f64) -> MatchQuality {
    MatchQuality {
        kind,
        score: kind.fixed_score() * discount,
    }
}

/// Finds the index span covering an in-order subsequence match of `needle` in
/// `haystack`, or `None` if the characters do not appear in order.
///
/// Walks both sequences once, keeping the first and last consumed position.
/// The walk is greedy-leftmost, which is the right choice for two reasons: it
/// finds a match whenever one exists (a leftmost-greedy subsequence scan is
/// complete for this problem), and it finds the *tightest-left* alignment, so
/// the span it reports is an upper bound on how good any other alignment
/// could have been.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_are_strictly_ordered_and_the_fuzzy_band_fits_under_browse() {
        // A `const` block, so these are checked when the crate is *compiled* and
        // not when the tests are run. The tier order is an invariant of the
        // module, not an observation about the code: if someone retunes
        // `PREFIX_SCORE` below `WORD_BOUNDARY_SCORE`, the crate stops building
        // rather than quietly ranking in the wrong order.
        const {
            assert!(EXACT_SCORE > PREFIX_SCORE);
            assert!(PREFIX_SCORE > WORD_BOUNDARY_SCORE);
            assert!(WORD_BOUNDARY_SCORE > CONTAINS_SCORE);
            assert!(CONTAINS_SCORE > BROWSE_SCORE);
            assert!(BROWSE_SCORE > FUZZY_FLOOR + FUZZY_CEILING);
            assert!(SUBSTRING_SCORE == CONTAINS_SCORE);
        }
    }

    #[test]
    fn match_kind_derived_order_matches_the_documented_tiers() {
        let mut sorted = [
            MatchKind::None,
            MatchKind::Fuzzy,
            MatchKind::Browse,
            MatchKind::Substring,
            MatchKind::Exact,
            MatchKind::WordBoundary,
            MatchKind::Prefix,
        ];
        sorted.sort();
        assert_eq!(
            sorted,
            [
                MatchKind::Exact,
                MatchKind::Prefix,
                MatchKind::WordBoundary,
                MatchKind::Substring,
                MatchKind::Browse,
                MatchKind::Fuzzy,
                MatchKind::None,
            ],
            "MatchKind's Ord must stay best-to-worst; the UI sorts on it"
        );
    }

    #[test]
    fn tiers_land_on_their_documented_scores() {
        let score = |q: &str| match_score(q, "Notepad.exe");
        assert_eq!(score("notepad.exe"), EXACT_SCORE);
        assert_eq!(score("not"), PREFIX_SCORE);
        assert_eq!(score("pad"), CONTAINS_SCORE);

        // A word-boundary hit is the same substring hit, promoted.
        assert_eq!(match_score("note", "Text note"), WORD_BOUNDARY_SCORE);
        assert_eq!(match_score("note", "Text xnote"), CONTAINS_SCORE);
        assert!(match_score("note", "Text note") > match_score("note", "Text xnote"));
    }

    #[test]
    fn word_boundaries_use_identifier_and_path_separators() {
        // `.` separates words, so the extension is findable.
        assert_eq!(match_score("exe", "notepad.exe"), WORD_BOUNDARY_SCORE);
        // `\` and `/` too.
        assert_eq!(
            match_score("users", "C:\\Users\\chris"),
            WORD_BOUNDARY_SCORE
        );
        assert_eq!(match_score("docs", "/home/chris/docs"), WORD_BOUNDARY_SCORE);
        assert_eq!(
            match_score("draft", "chapter-42-draft"),
            WORD_BOUNDARY_SCORE
        );
        // `_` is part of a word, so a query landing inside one is a fragment hit.
        assert_eq!(match_score("bar", "foo_bar_baz"), CONTAINS_SCORE);
        // And so is a query that is a *suffix* of a word, even when the
        // character before it is a separator: `raft` sits inside `draft`.
        assert_eq!(match_score("raft", "chapter-42-draft"), CONTAINS_SCORE);
        // Digits are word characters, and a space is not.
        assert_eq!(match_score("42", "chapter 42 draft"), WORD_BOUNDARY_SCORE);
    }

    #[test]
    fn a_boundary_hit_anywhere_promotes_the_whole_candidate() {
        // `note` occurs at index 1 (inside `anote`) and at index 10 (as a
        // word). The word occurrence is the one the user meant.
        let candidate = "anote-and-note";
        assert_eq!(classify("note", candidate), MatchKind::WordBoundary);
        assert_eq!(match_score("note", candidate), WORD_BOUNDARY_SCORE);
        // Contrast: a candidate with only a fragment hit stays a substring.
        assert_eq!(classify("note", "anote-and-another"), MatchKind::Substring);
    }

    #[test]
    fn match_score_is_case_and_whitespace_insensitive() {
        assert_eq!(match_score("NoTePaD", "notepad"), EXACT_SCORE);
        assert_eq!(match_score("NOTEPAD.EXE", "notepad.exe"), EXACT_SCORE);
        // Case folding must not turn a prefix into an exact match, or the
        // exact band becomes unreachable for anything with an extension.
        assert_eq!(match_score("notepad", "notepad.exe"), PREFIX_SCORE);
        // Whitespace inside the query is dropped before comparison, so a spec
        // copied out of a settings UI lands on the same band as the unspaced
        // equivalent.
        assert_eq!(match_score(" ep ad ", "notepad.exe"), CONTAINS_SCORE);
        assert_eq!(match_score(" notepad ", "notepad"), EXACT_SCORE);
        assert_eq!(match_score("  notepad  ", "  Notepad  "), EXACT_SCORE);
    }

    #[test]
    fn unicode_matches_on_char_boundaries_not_bytes() {
        // Every index in the matcher is a `char` index. A byte-indexed
        // implementation mis-slices all of these, and misaligns *silently*,
        // because a misaligned slice just fails to match anything.
        assert_eq!(match_score("café", "Café Notes"), PREFIX_SCORE);
        assert_eq!(match_score("café", "Grand Café"), WORD_BOUNDARY_SCORE);
        assert_eq!(match_score("note", "日本語 note"), WORD_BOUNDARY_SCORE);
        assert_eq!(match_score("日本", "日本語"), PREFIX_SCORE);
        // Same length, so the whole thing is an exact hit on `char` boundaries.
        assert_eq!(match_score("日本", "日本"), EXACT_SCORE);
        // A single character that is three bytes.
        assert_eq!(match_score("日", "日本"), PREFIX_SCORE);
        // Emoji are one char but four bytes; the span maths must not care.
        assert_eq!(match_score("🎉", "party 🎉 time"), WORD_BOUNDARY_SCORE);
        assert_eq!(match_score("🎉", "🎉"), EXACT_SCORE);
        // A two-character CJK needle against a two-character CJK haystack that
        // is not a prefix, so the substring scan is the code under test.
        assert_eq!(match_score("本語", "日本語"), SUBSTRING_SCORE);
    }

    #[test]
    fn case_folding_is_simple_lowercasing_not_full_case_folding() {
        // Rust's `to_lowercase` is the *simple* Unicode mapping, so `ß` does not
        // fold to `ss` the way full case folding would. Asserted because the
        // module docs promise "case is folded, Unicode is not normalised", and
        // this is the edge of that promise: a user who types `strasse` and sees
        // nothing has typed a `ss` where the name has a `ß`.
        assert_eq!(match_score("strasse", "STRASSE"), EXACT_SCORE);
        assert_eq!(match_score("straße", "STRASSE"), 0.0);
        // And the reverse, to show it is not a one-way accident.
        assert_eq!(match_score("STRASSE", "straße"), 0.0);
    }

    #[test]
    fn a_misspelled_accent_can_still_reach_the_item_via_the_fuzzy_tier() {
        // `cafe` does not match `Café` — accents are not folded. But `cafe` *is*
        // an in-order subsequence of `Café Notes`, because the `e` in `Notes`
        // completes it, so the item is still reachable. The point of the test is
        // the *tier*: a real prefix match outranks a fuzzy rescue by a wide
        // margin, so the fallback never looks like the user got what they typed.
        assert_eq!(match_score("cafe", "café"), 0.0);
        let rescued = match_score("cafe", "Café Notes");
        assert_eq!(classify("cafe", "Café Notes"), MatchKind::Fuzzy);
        assert!(
            rescued < BROWSE_SCORE,
            "a fuzzy rescue must rank below the browse state: {rescued}"
        );
        // And it loses to a genuine prefix of the same query.
        assert!(match_score("cafe", "cafeteria") > rescued);
    }

    #[test]
    fn match_score_rejects_out_of_order_subsequences() {
        // Every character is present, but not in order: not a match at all.
        assert_eq!(match_score("xen", "notepad"), 0.0);
        assert_eq!(match_score("aod", "notepad"), 0.0);
        assert_eq!(classify("xen", "notepad"), MatchKind::None);
    }

    #[test]
    fn a_needle_longer_than_the_haystack_never_matches() {
        // Covered by the early-out; the assertion is that the early-out and the
        // general path agree.
        assert_eq!(match_score("notepad.exe", "note"), 0.0);
        assert_eq!(classify("abcdef", "abc"), MatchKind::None);
    }

    #[test]
    fn the_fuzzy_band_spans_its_full_declared_range() {
        // One skipped character: the best a fuzzy match can do, because a span of
        // exactly `needle.len()` is contiguous and the substring tier claimed
        // it. This is the case that makes the ceiling reachable.
        let tightest = match_score("tap", "nta.p");
        assert_eq!(classify("tap", "nta.p"), MatchKind::Fuzzy);
        assert_eq!(tightest, FUZZY_FLOOR + FUZZY_CEILING);

        // First match at index 0, last at the final character: the floor.
        let scattered = match_score("nt", "nxxxxt");
        assert_eq!(classify("nt", "nxxxxt"), MatchKind::Fuzzy);
        assert_eq!(scattered, FUZZY_FLOOR);

        // Between the two, twice over, and strictly ordered. Both are three
        // characters of `notepad` taken out of order, with the span — the
        // density of the match — as the only difference.
        let tighter = match_score("nop", "notepad");
        let looser = match_score("otd", "notepad");
        assert!(tightest > tighter, "{tightest} !> {tighter}");
        assert!(tighter > looser, "{tighter} !> {looser}");
        assert!(looser > scattered, "{looser} !> {scattered}");
        for score in [tighter, looser] {
            assert!(
                (FUZZY_FLOOR..BROWSE_SCORE).contains(&score),
                "fuzzy out of band: {score}"
            );
        }
    }

    #[test]
    fn a_same_length_candidate_is_exact_or_not_a_fuzzy_match() {
        // With equal lengths, an in-order subsequence match can only be the
        // whole string, which the exact tier already claimed. So a same-length
        // mismatch is never fuzzy, and the `loosest > tightest` guard in the
        // packing maths is unreachable from here. It stays, because it is one
        // line and the alternative is a division by zero waiting for the next
        // refactor.
        assert_eq!(classify("abcdef", "fedcba"), MatchKind::None);
        assert_eq!(match_score("abcdef", "fedcba"), 0.0);
        assert_eq!(classify("abc", "abd"), MatchKind::None);
        assert_eq!(classify("abc", "abc"), MatchKind::Exact);
        // Whitespace is stripped from the query but kept in the candidate, so
        // this one *is* fuzzy — at the floor, since the match spans the whole
        // candidate.
        assert_eq!(classify("ab", "a b"), MatchKind::Fuzzy);
        assert_eq!(match_score("ab", "a b"), FUZZY_FLOOR);
    }

    #[test]
    fn fuzzy_is_disabled_by_a_too_short_needle() {
        // Two characters out of order is the useful fuzzy case — a typo in an
        // acronym. One character cannot be: a single character that is present
        // is always a *substring*, and one that is absent never matches, so the
        // gate is about the two-character case, not the one-character case.
        assert_eq!(classify("np", "notepad"), MatchKind::Fuzzy);
        let policy = crate::policy::RankingPolicy::DEFAULT.with_min_fuzzy_length(3);
        assert_eq!(
            policy.match_quality("np", "Notepad", None),
            MatchQuality {
                kind: MatchKind::None,
                score: 0.0
            }
        );
        // A three-character query gets its tolerance back.
        assert_eq!(
            policy.match_quality("nta", "Notepad", None).kind,
            MatchKind::Fuzzy
        );
    }

    #[test]
    fn empty_query_is_browse_and_empty_candidate_is_none() {
        assert_eq!(match_score("", "notepad.exe"), BROWSE_SCORE);
        assert_eq!(match_score("   ", "notepad.exe"), BROWSE_SCORE);
        assert_eq!(classify("", "notepad.exe"), MatchKind::Browse);
        assert!(MatchKind::Browse.is_real());

        assert_eq!(match_score("notepad", ""), 0.0);
        // Including a whitespace-only candidate, which folds to nothing.
        assert_eq!(match_score("notepad", "   "), 0.0);
        assert_eq!(classify("notepad", "   "), MatchKind::None);
        assert!(!MatchKind::None.is_real());
        assert!(!MatchKind::Fuzzy.is_real());
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
        for query in ["notepad", "notepad.exe", "not", "note", "exe"] {
            let score = match_score(query, "notepad.exe");
            assert!(
                score >= BROWSE_SCORE,
                "{query:?} should rank above browse, got {score}"
            );
        }
    }

    #[test]
    fn classify_agrees_with_match_score_on_every_tier() {
        let pairs = [
            ("notepad", "notepad", MatchKind::Exact),
            ("note", "notepad", MatchKind::Prefix),
            ("note", "text note", MatchKind::WordBoundary),
            ("pad", "notepad", MatchKind::Substring),
            ("", "notepad", MatchKind::Browse),
            ("nte", "notepad", MatchKind::Fuzzy),
            ("xen", "notepad", MatchKind::None),
        ];
        for (query, candidate, expected) in pairs {
            let kind = classify(query, candidate);
            assert_eq!(kind, expected, "{query:?} vs {candidate:?}");
            if expected != MatchKind::Fuzzy {
                assert_eq!(match_score(query, candidate), expected.fixed_score());
            }
        }
    }

    #[test]
    fn subtitle_matches_help_but_never_outrank_the_title() {
        let title = "report";
        let subtitle = "C:\\Users\\chris\\Downloads\\report.pdf";

        // `downloads` appears only in the path. Without the subtitle rule the
        // title alone scores zero and the file is invisible.
        assert_eq!(match_score("downloads", title), 0.0);
        let score = match_score_with_subtitle("downloads", title, Some(subtitle));
        // The path match is a word-boundary hit (`\` is a separator), discounted.
        assert_eq!(score, WORD_BOUNDARY_SCORE * SUBTITLE_DISCOUNT);
        assert!(score > CONTAINS_SCORE, "it should still be a real hit");

        // A real title match still wins outright.
        assert_eq!(
            match_score_with_subtitle("report", title, Some(subtitle)),
            EXACT_SCORE
        );
    }

    #[test]
    fn subtitle_does_not_disturb_the_browse_state() {
        // Discounted browse (0.45) must lose to the title's browse (0.5), so
        // the untyped recents list is not reordered by path text.
        let score = match_score_with_subtitle("", "zeta", Some("alpha"));
        assert_eq!(score, BROWSE_SCORE);
    }

    #[test]
    fn a_blank_subtitle_is_ignored() {
        for blank in ["", "   ", "\t\n"] {
            assert_eq!(match_score_with_subtitle("x", "title", Some(blank)), 0.0);
        }
        assert_eq!(match_score_with_subtitle("x", "title", None), 0.0);
    }

    #[test]
    fn match_quality_keeps_the_tier_of_whichever_field_won() {
        let subtitle = "C:\\Users\\chris\\Downloads";
        // Title wins, so the title's tier comes back.
        let from_title = match_quality_with_subtitle("report", "report", Some(subtitle));
        assert_eq!(from_title.kind, MatchKind::Exact);
        // Subtitle wins, so the subtitle's (discounted) tier comes back, and
        // the score reflects the discount.
        let from_subtitle = match_quality_with_subtitle("downloads", "report", Some(subtitle));
        assert_eq!(from_subtitle.kind, MatchKind::WordBoundary);
        assert!((from_subtitle.score - WORD_BOUNDARY_SCORE * SUBTITLE_DISCOUNT).abs() < 1e-12);
        // An exact title match still beats a stronger subtitle match.
        let mixed = match_quality_with_subtitle("rep", "report", Some("report"));
        assert_eq!(mixed.kind, MatchKind::Prefix);
    }
    #[test]
    fn fuzzy_span_is_complete_and_prefers_the_leftmost_alignment() {
        let needle: Vec<char> = "abc".chars().collect();
        // Two disjoint alignments: greedy must take the earliest, giving the
        // tighter span and therefore the higher score.
        let haystack: Vec<char> = "a--b--c".chars().collect();
        assert_eq!(fuzzy_span(&needle, &haystack), Some((0, 6)));
        // Impossible order is rejected.
        let haystack: Vec<char> = "cba".chars().collect();
        assert_eq!(fuzzy_span(&needle, &haystack), None);
        // Needle longer than haystack.
        let short: Vec<char> = "ab".chars().collect();
        assert_eq!(fuzzy_span(&needle, &short), None);
    }
}
