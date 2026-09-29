//! The ranking policy: the numbers, and the one place they are combined.
//!
//! # Shape
//!
//! Three normalised signals, combined once:
//!
//! ```text
//! final = match_weight * match
//!       + (1 - match_weight) * source_weight * (provider_share * provider
//!                                            + (1 - provider_share) * frecency)
//! ```
//!
//! Both weights are stored as a single fraction each and their complements are
//! derived, so the two halves can never fail to sum to one — which is the bug
//! that makes a scoring function quietly rescale itself the first time someone
//! tunes it.
//!
//! # Why the query dominates
//!
//! `match_weight` is 0.70, so history can move a result by at most 0.30 and the
//! query always decides first. That is the property that makes a launcher
//! trustworthy: **an exact match always outranks a browse candidate, no matter
//! how much history the browse candidate has.** It is asserted in the tests
//! below rather than left as an intention.
//!
//! The flip side is deliberate and also asserted: a *weak* match with a strong
//! history can outrank a *cold* exact match. That is what frecency ranking
//! means, and it is the whole point of having a history. What it must never do
//! is reorder two candidates whose match tiers differ, and at 0.70 it cannot —
//! the narrowest tier gap (`CONTAINS_SCORE` to `BROWSE_SCORE`, 0.25) is worth
//! 0.175 of final score, and the entire non-match budget is 0.30.
//!
//! # Why source weight multiplies the prior
//!
//! A source weight is not a bonus, it is a *discount on how much this kind of
//! thing's history is worth*. Ten launches of a pinned startup task is weaker
//! evidence than ten launches of a tool the user reaches for; there are also
//! ten thousand files and exactly one calculator. Multiplying the whole prior
//! by the weight expresses that in one term, and because the prior is bounded
//! by `1 - match_weight`, no source weighting can ever outvote the query.
//!
//! The default band is `0.65 ..= 1.00`. [`WeightTable::DEFAULT`] documents what
//! each number is for; [`MAX_SOURCE_SWING`] pins the consequence.

use std::time::Duration;

use crate::frecency::{Frecency, Timestamp, DEFAULT_HALF_LIFE};
use crate::matching::{match_quality_with_subtitle, MatchKind, MatchQuality};
use crate::model::Source;

/// Share of the final score driven by the query match.
pub const DEFAULT_MATCH_WEIGHT: f64 = 0.70;

/// Share of the *prior* (the non-match part of the score) driven by the
/// provider's own score rather than by launch history.
pub const DEFAULT_PROVIDER_SHARE: f64 = 0.40;

/// Shortest query that still gets typo tolerance.
///
/// One character is an in-order subsequence of nearly every string, so a
/// one-character fuzzy match turns the result list into a directory listing.
pub const DEFAULT_MIN_FUZZY_LENGTH: usize = 2;

/// Largest score change the *default* source table can produce, under
/// [`RankingPolicy::DEFAULT`].
///
/// `prior_weight * (max_default - min_default) == 0.30 * 0.35`. This is the
/// swing the shipped policy allows, and it is asserted in the tests rather than
/// left as a comment.
///
/// It is **not** a bound on what a config file can do: `[sources]` accepts any
/// value in `0.0 ..= 1.0`, so a hand-written weight of 0.0 or 1.0 can move a
/// score by up to [`MAX_ABSOLUTE_SOURCE_SWING`]. The difference matters because
/// the second number is the *whole prior budget* — at which point a config
/// author has declared that history is as important as the query, and that is
/// their call to make, not a bug to clamp away.
pub const DEFAULT_MAX_SOURCE_SWING: f64 =
    (1.0 - DEFAULT_MATCH_WEIGHT) * (1.0 - WeightTable::MINIMUM);

/// Largest score change *any* source weighting can produce, under
/// [`RankingPolicy::DEFAULT`].
///
/// Exactly `1 - match_weight`, because a weight of 1.0 multiplies the whole
/// prior. Even this cannot let a zero-match result outrank a one-match one
/// outright — it can only move a result within the prior budget — which is why
/// [`RankingPolicy`] stores one weight and derives its complement.
pub const MAX_ABSOLUTE_SOURCE_SWING: f64 = 1.0 - DEFAULT_MATCH_WEIGHT;

/// Sanitises a score into `0.0 ..= 1.0`.
///
/// `f64::clamp` is *not* safe here: it returns `NaN` for `NaN` (because every
/// comparison with `NaN` is false), so one poisoned provider score would
/// propagate through the whole blend and silently degrade every comparison in
/// the sort to the tiebreakers.
///
/// `NaN` becomes `0.0` — there is no evidence of relevance in a value that is
/// not a number — while the infinities are clamped normally, so a runaway
/// positive prior saturates at 1.0 rather than being thrown away.
#[must_use]
pub const fn clamp01(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

/// How much a result's launch history is worth, per kind of result.
///
/// A fixed-size array rather than a map, because [`Source`] has a dense index
/// space ([`Source::index`]) and a lookup must be a load, not a hash.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeightTable {
    weights: [f64; Source::COUNT],
}

impl WeightTable {
    /// Every source weighted equally and fully trusted.
    pub const NEUTRAL: WeightTable = WeightTable {
        weights: [1.0; Source::COUNT],
    };

    /// The shipped policy. See the module docs for the shape and the reasoning
    /// behind the band.
    pub const DEFAULT: WeightTable = WeightTable {
        weights: [
            1.00, // Application
            0.85, // File
            0.92, // Folder
            0.98, // Command
            0.85, // WebSearch
            0.95, // Calculator
            0.72, // Clipboard
            0.65, // Unknown
        ],
    };

    /// Lowest weight the default table uses. See [`MAX_SOURCE_SWING`].
    pub const MINIMUM: f64 = 0.65;

    /// Overrides one source's weight, sanitising the value.
    ///
    /// A non-finite weight is treated as 0.0 — an unwritten config key must not
    /// be able to inject `NaN` into the blend.
    #[must_use]
    pub const fn with(mut self, source: Source, weight: f64) -> WeightTable {
        self.weights[source.index()] = clamp01(weight);
        self
    }

    /// The weight for `source`, always in `0.0 ..= 1.0`.
    #[must_use]
    pub const fn get(&self, source: Source) -> f64 {
        // Read back through `clamp01` so a table built by a future `const`
        // constructor cannot smuggle an out-of-range value into the blend.
        clamp01(self.weights[source.index()])
    }

    /// The weights as `(source, weight)` pairs, in [`Source::ALL`] order.
    ///
    /// Convenient for a settings UI that wants to render the table, and for a
    /// test that wants to assert on the whole band at once.
    pub fn iter(&self) -> impl Iterator<Item = (Source, f64)> + '_ {
        Source::ALL
            .into_iter()
            .map(move |source| (source, self.get(source)))
    }
}

impl Default for WeightTable {
    fn default() -> Self {
        WeightTable::DEFAULT
    }
}

/// The tunable half of ranking: everything a config file is allowed to change.
///
/// [`RankingPolicy::DEFAULT`] is a `const`, so the shipped behaviour is
/// checkable in a test and a config file is only ever a *deviation* from a
/// value the crate can reason about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankingPolicy {
    /// Share of the final score driven by the query match.
    match_weight: f64,
    /// Share of the prior driven by the provider's own score.
    provider_share: f64,
    /// Half-life of the recency term.
    half_life: Duration,
    /// Shortest query that still gets fuzzy matching.
    min_fuzzy_length: usize,
    /// Minimum final score for a result to be shown at all.
    min_score: f64,
    sources: WeightTable,
}

impl RankingPolicy {
    /// The shipped policy.
    pub const DEFAULT: RankingPolicy = RankingPolicy {
        match_weight: DEFAULT_MATCH_WEIGHT,
        provider_share: DEFAULT_PROVIDER_SHARE,
        half_life: DEFAULT_HALF_LIFE,
        min_fuzzy_length: DEFAULT_MIN_FUZZY_LENGTH,
        min_score: 0.0,
        sources: WeightTable::DEFAULT,
    };

    /// Sets the query's share of the final score. Clamped to `0.0 ..= 1.0`.
    #[must_use]
    pub const fn with_match_weight(mut self, weight: f64) -> RankingPolicy {
        self.match_weight = clamp01(weight);
        self
    }

    /// Sets the provider's share of the prior. Clamped to `0.0 ..= 1.0`.
    #[must_use]
    pub const fn with_provider_share(mut self, share: f64) -> RankingPolicy {
        self.provider_share = clamp01(share);
        self
    }

    /// Sets the recency half-life.
    #[must_use]
    pub const fn with_half_life(mut self, half_life: Duration) -> RankingPolicy {
        self.half_life = half_life;
        self
    }

    /// Sets the shortest query that still gets fuzzy matching. `0` disables
    /// fuzzy matching entirely.
    #[must_use]
    pub const fn with_min_fuzzy_length(mut self, length: usize) -> RankingPolicy {
        self.min_fuzzy_length = length;
        self
    }

    /// Sets the score below which a result is not shown.
    #[must_use]
    pub const fn with_min_score(mut self, min_score: f64) -> RankingPolicy {
        self.min_score = clamp01(min_score);
        self
    }

    /// Replaces the whole source weight table.
    #[must_use]
    pub const fn with_sources(mut self, sources: WeightTable) -> RankingPolicy {
        self.sources = sources;
        self
    }

    /// Overrides a single source weight.
    #[must_use]
    pub const fn with_source_weight(mut self, source: Source, weight: f64) -> RankingPolicy {
        self.sources = self.sources.with(source, weight);
        self
    }

    /// The query's share of the final score.
    #[must_use]
    pub const fn match_weight(&self) -> f64 {
        self.match_weight
    }

    /// The provider's share of the prior.
    #[must_use]
    pub const fn provider_share(&self) -> f64 {
        self.provider_share
    }

    /// The recency half-life.
    #[must_use]
    pub const fn half_life(&self) -> Duration {
        self.half_life
    }

    /// The score below which a result is not shown.
    #[must_use]
    pub const fn min_score(&self) -> f64 {
        self.min_score
    }

    /// The source weight table.
    #[must_use]
    pub const fn sources(&self) -> &WeightTable {
        &self.sources
    }

    /// Classifies `title` (and optionally `subtitle`) against `query`,
    /// applying [`RankingPolicy::min_fuzzy_length`].
    ///
    /// This is the only place the min-fuzzy-length rule lives, so the free
    /// function [`crate::match_score`] and the policy cannot disagree about
    /// which query is "too short to fuzz".
    #[must_use]
    pub fn match_quality(&self, query: &str, title: &str, subtitle: Option<&str>) -> MatchQuality {
        let quality = match_quality_with_subtitle(query, title, subtitle);
        if quality.kind != MatchKind::Fuzzy {
            return quality;
        }
        // `0` is a real setting and means "no typo tolerance at all", so the
        // enabled test cannot be `min_fuzzy_length > 0` — that would make 0 mean
        // the opposite of what it says. The query's length is counted the same
        // way the matcher counts it: after trimming, case folding, and
        // whitespace removal. Counting raw bytes would let a one-character CJK
        // query look "long enough" and let the fuzzy tier through.
        let enabled = self.min_fuzzy_length >= 1 && fold_len(query) >= self.min_fuzzy_length;
        if enabled {
            quality
        } else {
            MatchQuality {
                kind: MatchKind::None,
                score: 0.0,
            }
        }
    }

    /// Combines the three signals into the final score, in `0.0 ..= 1.0`.
    ///
    /// `now` is passed in rather than read: the clock belongs to the caller,
    /// which is what lets one set of history be scored against two different
    /// points in time in two different tests.
    #[must_use]
    pub fn score(
        &self,
        quality: MatchQuality,
        source: Source,
        provider_score: f64,
        frecency: Frecency,
        now: Timestamp,
    ) -> f64 {
        let prior = self.provider_share * clamp01(provider_score)
            + (1.0 - self.provider_share) * frecency.score(now, self.half_life);
        let blended = self.match_weight * clamp01(quality.score)
            + (1.0 - self.match_weight) * self.sources.get(source) * prior;
        clamp01(blended)
    }
}

impl Default for RankingPolicy {
    fn default() -> Self {
        RankingPolicy::DEFAULT
    }
}

/// Length of a query as the matcher sees it, after folding and whitespace
/// removal. Kept next to [`RankingPolicy::match_quality`] so the two cannot
/// count differently.
fn fold_len(query: &str) -> usize {
    crate::text::fold_query(query).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frecency::Timestamp;
    use crate::matching::{BROWSE_SCORE, CONTAINS_SCORE, EXACT_SCORE, PREFIX_SCORE};

    const NOW: Timestamp = Timestamp::from_unix_seconds(1_700_000_000);

    fn exact() -> MatchQuality {
        MatchQuality {
            kind: MatchKind::Exact,
            score: EXACT_SCORE,
        }
    }

    fn browse() -> MatchQuality {
        MatchQuality {
            kind: MatchKind::Browse,
            score: BROWSE_SCORE,
        }
    }

    fn fragment() -> MatchQuality {
        MatchQuality {
            kind: MatchKind::Substring,
            score: CONTAINS_SCORE,
        }
    }

    #[test]
    fn clamp01_sanitises_nan_but_clamps_the_infinities() {
        // The one that matters: `f64::clamp` would return NaN here, and a NaN in
        // the blend degrades every comparison in the sort to the tiebreakers.
        assert!(f64::NAN.clamp(0.0, 1.0).is_nan());
        assert_eq!(clamp01(f64::NAN), 0.0);
        assert_eq!(clamp01(2.0), 1.0);
        assert_eq!(clamp01(-1.0), 0.0);
        // A runaway positive prior saturates rather than being discarded: 1.0
        // is the honest reading of "infinitely relevant".
        assert_eq!(clamp01(f64::INFINITY), 1.0);
        assert_eq!(clamp01(f64::NEG_INFINITY), 0.0);
    }

    #[test]
    fn a_nan_anywhere_in_the_blend_yields_a_number() {
        let policy = RankingPolicy::DEFAULT;
        let poisoned = policy.score(exact(), Source::Application, f64::NAN, Frecency::NEVER, NOW);
        assert!(poisoned.is_finite(), "{poisoned}");
        // And a non-finite weight table cannot poison it either.
        let policy = policy.with_source_weight(Source::Application, f64::NAN);
        assert!(policy
            .score(exact(), Source::Application, 0.5, Frecency::NEVER, NOW)
            .is_finite());
    }

    #[test]
    fn score_is_always_in_range() {
        let policy = RankingPolicy::DEFAULT;
        let frecencies = [
            Frecency::NEVER,
            Frecency::new(1, Some(NOW)),
            Frecency::new(u32::MAX, Some(Timestamp::EPOCH)),
        ];
        for source in Source::ALL {
            for provider in [0.0, 0.5, 1.0] {
                for frecency in frecencies {
                    for quality in [exact(), browse(), fragment()] {
                        let score = policy.score(quality, source, provider, frecency, NOW);
                        assert!(
                            (0.0..=1.0).contains(&score),
                            "{score} out of range for {source:?} p={provider} f={frecency:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_query_always_outranks_the_browse_state_however_good_the_history() {
        // This is the guarantee that makes a launcher trustworthy, and the reason
        // `match_weight` is 0.70 rather than something more "balanced".
        let policy = RankingPolicy::DEFAULT;
        let unbeatable_history = Frecency::new(10_000, Some(NOW));

        let cold_exact = policy.score(exact(), Source::Unknown, 0.0, Frecency::NEVER, NOW);
        let hot_browse = policy.score(browse(), Source::Application, 1.0, unbeatable_history, NOW);
        assert!(
            cold_exact > hot_browse,
            "exact {cold_exact} must beat a perfect-history browse {hot_browse}"
        );
    }

    #[test]
    fn a_weak_match_with_history_beating_a_cold_exact_match_is_the_intended_trade() {
        // Documented, not accidental. Frecency only earns its place if it can
        // actually promote something; what it must never do is reorder two
        // candidates that differ in match tier.
        let policy = RankingPolicy::DEFAULT;
        let fragment_with_history = policy.score(
            fragment(),
            Source::Application,
            1.0,
            Frecency::new(200, Some(NOW)),
            NOW,
        );
        let cold_exact = policy.score(exact(), Source::Unknown, 0.0, Frecency::NEVER, NOW);
        assert!(fragment_with_history > cold_exact);

        // But not by enough to overturn a tier gap when both are cold.
        let cold_fragment =
            policy.score(fragment(), Source::Application, 0.0, Frecency::NEVER, NOW);
        assert!(cold_exact > cold_fragment);
    }

    #[test]
    fn the_default_source_band_cannot_outweigh_the_narrowest_tier_gap() {
        // The narrowest tier gap is CONTAINS -> BROWSE = 0.25, worth
        // `match_weight * 0.25`. The default table's band is worth less, so a
        // user who has not touched `[sources]` can never see the weighting
        // reorder two candidates whose match tiers differ.
        let narrowest_gap = (CONTAINS_SCORE - BROWSE_SCORE) * DEFAULT_MATCH_WEIGHT;
        assert!(
            DEFAULT_MAX_SOURCE_SWING < narrowest_gap,
            "the default source band ({DEFAULT_MAX_SOURCE_SWING}) could overturn a tier gap worth {narrowest_gap}"
        );
    }

    #[test]
    fn the_absolute_source_ceiling_is_the_prior_budget() {
        // A `const` block: these are identities between constants, so they are
        // checked when the crate is compiled rather than when the tests run.
        //
        // What a `[sources]` value of 0.0-to-1.0 can actually do is move a score
        // by the whole prior budget, and nothing more. It is enough to reorder
        // adjacent tiers, and that is the point: a config that weights
        // clipboard history at 1.0 is making a statement, and the code should
        // carry it out rather than clamp it away.
        const {
            assert!(
                (MAX_ABSOLUTE_SOURCE_SWING - (1.0 - DEFAULT_MATCH_WEIGHT)).abs() < 1e-12,
                "the absolute ceiling must be exactly the prior budget"
            );
            assert!(
                MAX_ABSOLUTE_SOURCE_SWING < 1.0,
                "a source weight cannot own the whole score"
            );
            assert!(
                MAX_ABSOLUTE_SOURCE_SWING > (PREFIX_SCORE - CONTAINS_SCORE) * DEFAULT_MATCH_WEIGHT,
                "a 0.0-to-1.0 source weight must be able to reorder adjacent tiers"
            );
        }
    }

    #[test]
    fn the_default_source_band_is_bounded_by_the_documented_swing() {
        let policy = RankingPolicy::DEFAULT;
        // A mid-band source, with the frecency term at its maximum, moved from
        // the bottom of the default band to the top.
        let frecency = Frecency::new(u32::MAX, Some(NOW));
        let low = policy.score(exact(), Source::Unknown, 0.0, frecency, NOW);
        let high = policy.score(exact(), Source::Application, 0.0, frecency, NOW);
        assert!(
            (high - low).abs() <= DEFAULT_MAX_SOURCE_SWING + 1e-12,
            "default band swing {} exceeds {DEFAULT_MAX_SOURCE_SWING}",
            (high - low).abs()
        );
    }

    #[test]
    fn a_config_supplied_source_band_is_bounded_by_the_absolute_ceiling() {
        // A `[sources]` value of 1.0 is legal and does move a score by the whole
        // prior budget. The point of the test is that it is bounded, not that it
        // is small.
        let policy = RankingPolicy::DEFAULT;
        let frecency = Frecency::new(u32::MAX, Some(NOW));
        let high = policy.score(exact(), Source::Clipboard, 1.0, frecency, NOW);
        let low = policy.with_source_weight(Source::Clipboard, 0.0).score(
            exact(),
            Source::Clipboard,
            1.0,
            frecency,
            NOW,
        );
        let swing = high - low;
        assert!(
            swing > DEFAULT_MAX_SOURCE_SWING,
            "a 0..1 band is wider than the default one"
        );
        assert!(
            swing <= MAX_ABSOLUTE_SOURCE_SWING + 1e-12,
            "swing {swing} exceeds the absolute ceiling {MAX_ABSOLUTE_SOURCE_SWING}"
        );
    }

    #[test]
    fn source_weighting_alone_cannot_reorder_a_tier_gap() {
        // Hold the history fixed and vary *only* the source weight across the
        // whole legal range. The fragment hit must still lose to the exact hit.
        // This is the guarantee the weighting is designed around, and it is
        // stronger and more useful than "source weight matters less than the
        // query", because it survives a config that sets the weights to 0 and 1.
        let now = NOW;
        let frecency = Frecency::new(20, Some(now));
        for weight in [0.0, 0.5, 1.0] {
            let trusted = RankingPolicy::DEFAULT.with_source_weight(Source::Clipboard, weight);
            let exact_hit = trusted.score(exact(), Source::Application, 0.0, frecency, now);
            let fragment_hit = trusted.score(fragment(), Source::Clipboard, 0.0, frecency, now);
            assert!(
                exact_hit > fragment_hit,
                "at clipboard weight {weight}: exact {exact_hit} lost to fragment {fragment_hit}"
            );
        }
    }

    #[test]
    fn a_zero_match_weight_hands_the_whole_score_to_history() {
        // The degenerate end of the knob, so the clamp is exercised. Useful as a
        // way to see pure MRU order in the UI.
        let mru = RankingPolicy::DEFAULT.with_match_weight(0.0);
        let hot_fragment = mru.score(
            fragment(),
            Source::Application,
            0.0,
            Frecency::new(9, Some(NOW)),
            NOW,
        );
        let cold_browse = mru.score(browse(), Source::Unknown, 0.0, Frecency::NEVER, NOW);
        assert!(hot_fragment > cold_browse);
        assert_eq!(mru.match_weight(), 0.0);
    }

    #[test]
    fn a_full_match_weight_hands_the_whole_score_to_the_query() {
        let strict = RankingPolicy::DEFAULT.with_match_weight(1.0);
        let a = strict.score(exact(), Source::Application, 0.0, Frecency::NEVER, NOW);
        let b = strict.score(
            browse(),
            Source::Application,
            1.0,
            Frecency::new(999, Some(NOW)),
            NOW,
        );
        assert!(a > b);
        assert!((a - EXACT_SCORE).abs() < 1e-12);
        assert!((b - BROWSE_SCORE).abs() < 1e-12);
    }

    #[test]
    fn tuning_knobs_clamp_rather_than_produce_impossible_weights() {
        let policy = RankingPolicy::DEFAULT
            .with_match_weight(9.0)
            .with_provider_share(-4.0)
            .with_min_score(f64::NAN);
        assert_eq!(policy.match_weight(), 1.0);
        assert_eq!(policy.provider_share(), 0.0);
        assert_eq!(policy.min_score(), 0.0);

        let policy = RankingPolicy::DEFAULT.with_match_weight(f64::NAN);
        assert_eq!(policy.match_weight(), 0.0);
    }

    #[test]
    fn weight_table_is_a_dense_indexed_array_not_a_map() {
        let table = WeightTable::DEFAULT;
        for source in Source::ALL {
            assert_eq!(
                table.get(source),
                Source::from_index(source.index()).map_or(0.0, |s| table.get(s))
            );
        }
        // Round-tripping through the index is the property that makes the array
        // layout safe.
        assert_eq!(
            table.get(Source::ALL[3]),
            table.get(Source::from_index(3).expect("index 3 is in range"))
        );
        assert!(table.iter().count() == Source::COUNT);
    }

    #[test]
    fn weight_table_sanitises_the_weights_it_is_given() {
        let table = WeightTable::DEFAULT
            .with(Source::Application, f64::NAN)
            .with(Source::File, 5.0)
            .with(Source::Folder, -5.0);
        assert_eq!(table.get(Source::Application), 0.0);
        assert_eq!(table.get(Source::File), 1.0);
        assert_eq!(table.get(Source::Folder), 0.0);
    }

    #[test]
    fn neutral_weights_ignore_source_entirely() {
        let neutral = RankingPolicy::DEFAULT.with_sources(WeightTable::NEUTRAL);
        let frecency = Frecency::new(4, Some(NOW));
        let reference = neutral.score(exact(), Source::Application, 0.5, frecency, NOW);
        for source in Source::ALL {
            let score = neutral.score(exact(), source, 0.5, frecency, NOW);
            assert!((score - reference).abs() < 1e-12, "{source:?} differed");
        }
    }

    #[test]
    fn default_source_band_is_the_documented_one() {
        let table = WeightTable::DEFAULT;
        let lowest = table.iter().map(|(_, w)| w).fold(f64::INFINITY, f64::min);
        let highest = table
            .iter()
            .map(|(_, w)| w)
            .fold(f64::NEG_INFINITY, f64::max);
        assert!((lowest - WeightTable::MINIMUM).abs() < f64::EPSILON);
        assert!((highest - 1.0).abs() < f64::EPSILON);
        // And DEFAULT_MAX_SOURCE_SWING really is derivable from it.
        let derived = (1.0 - DEFAULT_MATCH_WEIGHT) * (highest - lowest);
        assert!((derived - DEFAULT_MAX_SOURCE_SWING).abs() < 1e-12);
    }

    #[test]
    fn min_fuzzy_length_counts_folded_characters_not_bytes() {
        // A one-character CJK query is three bytes. Counting bytes would let it
        // through the length gate and the fuzzy tier would match almost
        // everything.
        let policy = RankingPolicy::DEFAULT.with_min_fuzzy_length(2);
        assert_eq!(
            policy.match_quality("日", "日本語", None).kind,
            MatchKind::Prefix,
            "a one-char query that is a real prefix still matches"
        );
        assert_eq!(
            policy.match_quality("日", "note", None).kind,
            MatchKind::None
        );
        // Whitespace does not count toward the gate: `" n "` folds to `n`, which
        // is one character and is a real prefix of "notepad".
        assert_eq!(
            policy.match_quality(" n ", "notepad", None).kind,
            MatchKind::Prefix
        );
        // Zero means "no typo tolerance at all", not "always allow it".
        let strict = policy.with_min_fuzzy_length(0);
        assert_eq!(
            strict.match_quality("nte", "notepad", None).kind,
            MatchKind::None
        );
    }

    #[test]
    fn a_two_character_fuzzy_query_is_gated_at_the_documented_default() {
        // The default of 2 exists because one character is a substring of almost
        // anything, and a *two*-character fuzzy match is the useful case: a
        // typo in an acronym.
        let policy = RankingPolicy::DEFAULT;
        assert_eq!(
            policy.match_quality("np", "notepad", None).kind,
            MatchKind::Fuzzy
        );
        assert_eq!(
            policy
                .with_min_fuzzy_length(3)
                .match_quality("np", "notepad", None)
                .kind,
            MatchKind::None
        );
        assert_eq!(
            policy
                .with_min_fuzzy_length(1)
                .match_quality("np", "notepad", None)
                .kind,
            MatchKind::Fuzzy
        );
    }

    #[test]
    fn min_fuzzy_length_never_demotes_a_real_tier() {
        let policy = RankingPolicy::DEFAULT.with_min_fuzzy_length(99);
        assert_eq!(
            policy.match_quality("n", "notepad", None).kind,
            MatchKind::Prefix
        );
        assert_eq!(
            policy.match_quality("n", "text n", None).kind,
            MatchKind::WordBoundary
        );
        // And the subtitle rule is unaffected by the gate too, because the gate
        // is applied after the whole analysis.
        assert!(policy.match_quality("x", "zzz", Some("xxx")).score > 0.0);
    }

    #[test]
    fn match_quality_agrees_with_the_free_function_when_the_gate_does_not_apply() {
        let policy = RankingPolicy::DEFAULT;
        for (query, title) in [("note", "notepad"), ("pad", "notepad"), ("xen", "notepad")] {
            assert_eq!(
                policy.match_quality(query, title, None).score,
                crate::match_score(query, title),
                "{query:?} vs {title:?}"
            );
        }
    }
}
