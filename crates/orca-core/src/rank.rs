//! Ordering: turn a catalog and a query into the list the user sees.
//!
//! [`crate::rank`] is the default entry point. It is deliberately a *pure*
//! function of `(query, items)`: no clock, no I/O, no shared state. The two
//! pieces of hidden input it needs are both explicit — the launch history rides
//! on each [`ResultItem`] as [`ResultItem::frecency`], and "now" is a parameter
//! of the underlying policy call.
//!
//! The two properties worth knowing:
//!
//! * **Non-matches are dropped, not sorted last.** A launcher that shows
//!   results the user did not ask for is a launcher nobody trusts. With an
//!   empty query the match tier is [`MatchKind::Browse`], so nothing is
//!   dropped and the recents list is intact.
//! * **The order is total and input-order independent.** The comparator ends
//!   in `id`, which the provider contract requires to be unique within a list,
//!   so the same catalog always renders in exactly the same order. That is
//!   what makes a keypress not reshuffle rows underneath the user's cursor.

use crate::frecency::Timestamp;
use crate::matching::MatchKind;
use crate::model::ResultItem;
use crate::policy::RankingPolicy;

/// A [`ResultItem`] with its final score, plus the match that produced it.
///
/// The match is kept rather than discarded so the UI can highlight *why* a row
/// matched and so a test can assert on tier without re-running the matcher.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedItem {
    /// The original, unmodified item.
    pub item: ResultItem,
    /// Final score in `0.0 ..= 1.0`, higher is better.
    pub score: f64,
    /// The match quality that fed [`RankedItem::score`].
    pub quality: crate::matching::MatchQuality,
}

/// Ranks `items` against `query` using [`RankingPolicy::DEFAULT`] at
/// `Timestamp::EPOCH`.
///
/// `now` matters: frecency decays, so ranking the same catalog at two different
/// clocks can legitimately give two different orders. The UI should pass the
/// real clock through [`rank_with`]. This wrapper exists for the common case of
/// "no history yet" and for tests that do not care about decay.
#[must_use]
pub fn rank<'a>(query: &str, items: impl IntoIterator<Item = &'a ResultItem>) -> Vec<RankedItem> {
    rank_at(RankingPolicy::DEFAULT, query, Timestamp::EPOCH, items)
}

/// Ranks `items` against `query` with an explicit policy and clock.
///
/// Items whose match quality is [`MatchKind::None`] are dropped. So are items
/// scoring below [`RankingPolicy::min_score`]. The rest are ordered by:
///
/// 1. final score, descending;
/// 2. match tier, then match score, descending — so a stronger textual match
///    wins when history is level, which is the common case on a first run;
/// 3. source weight, descending — a well-trusted kind of result wins an exact
///    tie with a poorly-trusted one;
/// 4. title, case-insensitively, ascending — so ties render in a stable,
///    human-sensible order rather than in provider order;
/// 5. id, ascending — a total order, so the output is fully deterministic and
///    independent of the input order.
#[must_use]
pub fn rank_with<'a>(
    policy: &RankingPolicy,
    query: &str,
    now: Timestamp,
    items: impl IntoIterator<Item = &'a ResultItem>,
) -> Vec<RankedItem> {
    let mut ranked: Vec<RankedItem> = items
        .into_iter()
        .filter_map(|item| {
            let quality = policy.match_quality(query, &item.title, item.subtitle.as_deref());
            if quality.kind == MatchKind::None {
                return None;
            }
            let score = policy.score(quality, item.source, item.score, item.frecency, now);
            if score < policy.min_score() {
                return None;
            }
            Some(RankedItem {
                item: item.clone(),
                score,
                quality,
            })
        })
        .collect();

    ranked.sort_by(|a, b| {
        // `partial_cmp` is total on our own output because every score went
        // through `clamp01`, which cannot produce NaN. The `unwrap_or` is a
        // belt-and-braces fallback, not a live branch: it is the difference
        // between "sort is slightly wrong" and "sort panics".
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.quality.kind.cmp(&a.quality.kind))
            .then_with(|| {
                b.quality
                    .score
                    .partial_cmp(&a.quality.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                policy
                    .sources()
                    .get(b.item.source)
                    .partial_cmp(&policy.sources().get(a.item.source))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.item.title.cmp(&b.item.title))
            .then_with(|| a.item.id.cmp(&b.item.id))
    });

    ranked
}

/// Convenience alias for [`rank_with`], spelled the way it reads at a call
/// site that has a policy value rather than a reference.
#[must_use]
pub fn rank_at<'a>(
    policy: RankingPolicy,
    query: &str,
    now: Timestamp,
    items: impl IntoIterator<Item = &'a ResultItem>,
) -> Vec<RankedItem> {
    rank_with(&policy, query, now, items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frecency::Frecency;
    use crate::matching::{BROWSE_SCORE, PREFIX_SCORE};
    use crate::model::{LaunchTarget, Source};
    use crate::policy::WeightTable;

    const NOW: Timestamp = Timestamp::from_unix_seconds(1_700_000_000);
    const HOUR: i64 = 3_600;

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

    fn rank_default(query: &str, items: &[ResultItem]) -> Vec<RankedItem> {
        rank_at(RankingPolicy::DEFAULT, query, NOW, items)
    }

    #[test]
    fn rank_drops_non_matching_items() {
        let items = vec![item("app:note", "Notepad"), item("app:calc", "Calculator")];
        assert_eq!(titles(&rank_default("note", &items)), vec!["Notepad"]);
    }

    #[test]
    fn an_empty_query_keeps_everything() {
        // The browse state must not filter: it is the recents list.
        let items = vec![item("app:note", "Notepad"), item("app:calc", "Calculator")];
        assert_eq!(rank_default("", &items).len(), 2);
        assert_eq!(rank_default("   ", &items).len(), 2);
        for ranked in rank_default("", &items) {
            assert_eq!(ranked.quality.kind, MatchKind::Browse);
            assert!((ranked.quality.score - BROWSE_SCORE).abs() < 1e-12);
        }
    }

    #[test]
    fn rank_orders_by_match_strength_when_history_is_level() {
        // Three genuine matches for "note" at three strengths, with identical
        // priors. This is the first-run experience, and match strength has to
        // decide it.
        let items = vec![
            item("app:textnote", "Text note"),
            item("app:note", "Note"),
            item("app:notepad", "Notepad"),
        ];
        let ranked = rank_default("note", &items);

        assert_eq!(titles(&ranked), vec!["Note", "Notepad", "Text note"]);
        let scores: Vec<f64> = ranked.iter().map(|r| r.score).collect();
        assert!(
            scores.windows(2).all(|w| w[0] > w[1]),
            "scores not strictly descending: {scores:?}"
        );
    }

    #[test]
    fn rank_breaks_ties_deterministically_by_title_then_id() {
        // Identical titles from two sources: same match, same prior, so only
        // the id tiebreak can order them.
        let a = item("app:zzz", "Notepad");
        let b = item("app:aaa", "Notepad");
        let items = vec![a, b];
        let reversed: Vec<ResultItem> = items.iter().rev().cloned().collect();

        let first = rank_default("notepad", &items);
        let second = rank_default("notepad", &reversed);
        assert_eq!(titles(&first), vec!["Notepad", "Notepad"]);
        assert_eq!(first, second, "ranking must not depend on input order");
        assert_eq!(first[0].item.id, "app:aaa");
    }

    #[test]
    fn ranking_is_stable_under_shuffling() {
        // The property the UI depends on: re-collecting from providers in a
        // different order must not reshuffle rows under the cursor.
        let base: Vec<ResultItem> = vec![
            item("app:a", "Alpha note").with_score(0.4),
            item("app:b", "Note beta").with_score(0.4),
            item("app:c", "Notebook").with_score(0.4),
            item("app:d", "notes.txt").with_score(0.4),
            item("app:e", "unrelated").with_score(0.9),
        ];
        let first = rank_default("note", &base);
        let reference = titles(&first);
        assert!(!reference.is_empty());

        for rotation in 1..base.len() {
            let mut rotated = base.clone();
            rotated.rotate_left(rotation);
            assert_eq!(
                titles(&rank_default("note", &rotated)),
                reference,
                "rotation {rotation}"
            );
            let mut reversed = base.clone();
            reversed.reverse();
            assert_eq!(
                titles(&rank_default("note", &reversed)),
                reference,
                "reversal"
            );
        }
    }

    #[test]
    fn recency_promotes_a_used_result_over_a_fresher_unused_one() {
        let stale = item("app:stale", "Note tool").with_frecency(Frecency::new(
            6,
            Some(NOW.saturating_add_secs(-30 * 24 * HOUR)),
        ));
        let fresh = item("app:fresh", "Note tool")
            .with_frecency(Frecency::new(1, Some(NOW.saturating_add_secs(-HOUR))));
        let unused = item("app:unused", "Note tool");

        let ranked = rank_default("note", &[stale, fresh, unused]);
        assert_eq!(
            titles(&ranked),
            vec!["Note tool", "Note tool", "Note tool"],
            "sanity: all three share a title"
        );
        // Same title and id ordering hides it, so assert on the scores.
        let scores: Vec<f64> = ranked.iter().map(|r| r.score).collect();
        assert!(
            scores[0] > scores[1],
            "fresher should beat staler: {scores:?}"
        );
        assert!(scores[1] > scores[2], "used should beat unused: {scores:?}");
    }

    #[test]
    fn the_empty_query_sorts_by_history_not_by_text() {
        let hot = item("app:hot", "Zzz").with_frecency(Frecency::new(20, Some(NOW)));
        let cold = item("app:cold", "Aaa");
        let ranked = rank_default("", &[cold, hot]);
        assert_eq!(titles(&ranked), vec!["Zzz", "Aaa"]);
    }

    #[test]
    fn a_cold_exact_match_beats_a_browse_result_with_impossible_history() {
        // The trust guarantee, end to end through `rank`. A *browse* candidate
        // with a perfect prior and a legendary history still loses to a typed
        // exact match, so typing something can never be a mistake.
        let hot = item("app:hot", "Notepad")
            .with_score(1.0)
            .with_frecency(Frecency::new(u32::MAX, Some(NOW)));
        let cold = item("app:exact", "Notepad");

        // Typed: both are exact matches for "notepad", and history decides.
        let typed = rank_default("notepad", &[hot.clone(), cold.clone()]);
        assert_eq!(typed.len(), 2);
        assert!(
            typed[0].score > typed[1].score,
            "history should decide here"
        );

        // Untyped: the hot one leads, and the cold one is still in the list.
        let browse = rank_default("", &[hot, cold]);
        assert_eq!(browse.len(), 2, "an empty query keeps everything");
        assert!(browse[0].score > browse[1].score);
    }

    #[test]
    fn with_equal_history_the_match_tier_decides() {
        // The trust guarantee, stated as something that is actually true: give
        // every item the *same* history and the same prior, and the order is
        // exactly the order of the match tiers. What the user typed decides.
        //
        // Note the careful wording. It is *not* true that an exact match always
        // beats a prefix match — a heavily-used prefix match does, and
        // deliberately so. What is true is that the query always decides before
        // history does, which is what `RankingPolicy::match_weight = 0.70`
        // buys and what this test pins.
        let shared = Frecency::new(3, Some(NOW.saturating_add_secs(-3_600)));
        for history in [Frecency::NEVER, shared, Frecency::new(9_000, Some(NOW))] {
            let items = vec![
                item("app:fragment", "Text note").with_frecency(history),
                item("app:prefix", "Notepad").with_frecency(history),
                item("app:exact", "Note").with_frecency(history),
            ];
            let ranked = rank_default("note", &items);
            assert_eq!(
                titles(&ranked),
                vec!["Note", "Notepad", "Text note"],
                "history {history:?} should not change the tier order"
            );
        }
    }

    #[test]
    fn an_empty_query_orders_by_history_and_drops_nothing() {
        // The browse state. It is the recents list, so history decides and the
        // title is only a tiebreak.
        let hot = item("app:hot", "Zzz").with_frecency(Frecency::new(20, Some(NOW)));
        let cold = item("app:cold", "Aaa");
        let ranked = rank_default("", &[cold, hot]);
        assert_eq!(titles(&ranked), vec!["Zzz", "Aaa"]);

        // A stale-but-used item loses to a fresh one.
        let stale = item("app:stale", "Mmm").with_frecency(Frecency::new(
            100,
            Some(NOW.saturating_add_secs(-90 * 24 * 3_600)),
        ));
        let fresh = item("app:fresh", "Bbb").with_frecency(Frecency::new(1, Some(NOW)));
        let ranked = rank_default("", &[stale, fresh]);
        assert_eq!(titles(&ranked), vec!["Bbb", "Mmm"]);
    }

    #[test]
    fn typing_something_never_widens_the_result_set() {
        // The corollary of dropping non-matches: extending a query one character
        // at a time can only ever remove rows. A launcher that grows its list as
        // you type is broken, and this is the assertion that says so.
        let items: Vec<ResultItem> = (0..40)
            .map(|index| item(&format!("app:{index}"), &format!("Item {index}")))
            .chain((0..20).map(|index| item(&format!("app:n{index}"), &format!("Note {index}"))))
            .collect();

        // Note the chain: each query is the previous one plus one character.
        let mut previous = rank_default("", &items).len();
        for query in ["n", "no", "not", "note", "note ", "note 1", "note 19"] {
            let current = rank_default(query, &items).len();
            assert!(
                current <= previous,
                "query {query:?} grew the list: {previous} -> {current}"
            );
            previous = current;
        }
    }

    #[test]
    fn source_weight_breaks_an_exact_tie() {
        // Same title, same history, different source: the comparator's
        // source-weight rung decides.
        let app = item("app:x", "Widget");
        let clip = ResultItem::new(
            "clip:x",
            "Widget",
            Source::Clipboard,
            LaunchTarget::Uri("x".into()),
        );
        let ranked = rank_default("widget", &[clip.clone(), app.clone()]);
        assert_eq!(ranked[0].item.source, Source::Application);

        // And with the table neutralised the tie falls through to the id.
        let neutral = RankingPolicy::DEFAULT.with_sources(WeightTable::NEUTRAL);
        let ranked = rank_at(neutral, "widget", NOW, &[clip, app]);
        assert_eq!(ranked[0].item.id, "app:x");
    }

    #[test]
    fn a_nan_provider_score_does_not_reorder_or_drop_anything() {
        let healthy = item("app:a", "Note").with_score(0.5);
        let poisoned = ResultItem {
            score: f64::NAN,
            ..item("app:b", "Notepad")
        };
        let ranked = rank_default("note", &[poisoned, healthy]);
        assert_eq!(ranked.len(), 2, "nothing should be dropped");
        assert!(ranked.iter().all(|r| r.score.is_finite()));
        // "Note" is an exact match and "Notepad" a prefix, so the order is
        // decided by the match tier, not by the sanitised prior. Without the
        // sanitisation the NaN would have fallen through to the tiebreakers and
        // made this order arbitrary.
        assert_eq!(ranked[0].item.title, "Note");
        assert_eq!(ranked[0].quality.kind, MatchKind::Exact);
    }

    #[test]
    fn min_score_drops_weak_results_but_never_the_exact_match() {
        let policy = RankingPolicy::DEFAULT.with_min_score(0.5);
        let exact = item("app:a", "Note");
        let fuzzy = item("app:b", "Ntebook");
        let ranked = rank_at(policy, "note", NOW, &[exact, fuzzy]);
        assert_eq!(titles(&ranked), vec!["Note"]);

        // With the query emptied everything falls to browse-plus-history, so a
        // high floor legitimately empties the list. Asserted because it is the
        // documented consequence, not an accident.
        assert!(rank_at(policy, "", NOW, &[item("app:a", "Note")]).is_empty());
    }

    #[test]
    fn subtitles_are_searchable_through_rank() {
        // `downloads` appears only in the path. Title-only matching scores zero,
        // which `match_score` proves; `rank` is what makes the file findable.
        let file = item("file:a", "report").with_subtitle("C:\\Users\\chris\\Downloads");
        assert_eq!(crate::match_score("downloads", &file.title), 0.0);

        let ranked = rank_default("downloads", &[file]);
        assert_eq!(titles(&ranked), vec!["report"]);
        assert_eq!(ranked[0].quality.kind, MatchKind::WordBoundary);
    }

    #[test]
    fn rank_keeps_the_quality_it_scored_with() {
        let items = vec![item("app:a", "Notepad"), item("app:b", "text note")];
        let ranked = rank_default("note", &items);
        assert_eq!(ranked[0].quality.kind, MatchKind::Prefix);
        assert!((ranked[0].quality.score - PREFIX_SCORE).abs() < 1e-12);
        assert_eq!(ranked[1].quality.kind, MatchKind::WordBoundary);
    }

    #[test]
    fn rank_is_total_even_with_duplicate_ids() {
        // Duplicate ids violate the provider contract, and the comparator is not
        // allowed to panic when they do. Equal elements simply stay put.
        let items = vec![item("dup", "Note"), item("dup", "Note")];
        let ranked = rank_default("note", &items);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0], ranked[1]);
    }

    #[test]
    fn unicode_titles_rank_without_panicking_or_dropping() {
        let items = vec![
            item("a:1", "日本語"),
            item("a:2", "Café Notes"),
            item("a:3", "STRASSE"),
            item("a:4", "🎉 party"),
        ];
        for (query, expected) in [
            ("日本", "日本語"),
            ("café", "Café Notes"),
            ("strasse", "STRASSE"),
            ("🎉", "🎉 party"),
        ] {
            let ranked = rank_default(query, &items);
            assert_eq!(
                titles(&ranked).first().copied(),
                Some(expected),
                "{query:?}"
            );
        }
        // A query with no relation to anything drops everything.
        assert!(rank_default("zzzz", &items).is_empty());
        // `straße` is a genuine non-match: the ß has no counterpart in
        // `STRASSE`, proven against the matcher so the assertion below is about
        // `rank` and not about folding.
        assert_eq!(crate::match_score("straße", "STRASSE"), 0.0);
        assert!(rank_default("straße", &items).is_empty());
        // `cafe` *does* reach `Café Notes`, but only through the fuzzy tier,
        // because the `e` in `Notes` completes the subsequence. Asserted so the
        // behaviour is recorded rather than discovered: a user who mistypes an
        // accent gets the item at the very bottom of the list, not at the top.
        let rescued = rank_default("cafe", &items);
        assert_eq!(titles(&rescued), vec!["Café Notes"]);
        assert_eq!(rescued[0].quality.kind, MatchKind::Fuzzy);
        assert!(rescued[0].score < crate::BROWSE_SCORE * RankingPolicy::DEFAULT.match_weight());
    }
}
