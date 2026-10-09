//! The second provider seam: providers that answer the query itself.
//!
//! [`ResultProvider`] is query-blind, and that is the most valuable rule in the
//! provider design: `collect` returns *everything*, the query is applied once
//! by [`crate::rank`], and so two providers can never disagree about what
//! "matches". Keeping it is not optional — the moment one provider filters by
//! query, "matches" becomes a per-provider opinion and the ordering stops being
//! explainable.
//!
//! That same rule is what makes it impossible to serve a computed answer
//! through the first seam. There is no such thing as "every arithmetic
//! expression": a collectible set is finite and enumerable — installed apps,
//! environment variables, the aliases in `config.toml` — while an *answer* is
//! one row whose identity depends on the query in full. A calculator would have
//! to either enumerate an infinite catalogue or reopen the exception, and both
//! shapes end with two providers disagreeing about what matched.
//!
//! So this is a second seam rather than an exception inside the first.
//!
//! # The seam
//!
//! [`QueryProvider::respond`] takes the query and returns candidates or nothing
//! at all. It is the same contract in the other direction: no ranking, no
//! filtering, no I/O. [`QueryProviders`] merges the answers with the
//! first-provider-wins rule [`crate::providers::ProviderSet::collect_all`]
//! documents, copied deliberately — a merge rule that differs between the two
//! seams is a rule nobody remembers, and the one that differs is the one that
//! gets "fixed".
//!
//! # Why not a `respond` method on `ResultProvider`
//!
//! Adding a query-shaped method to a trait whose whole point is that it never
//! sees a query would put the burden on every existing implementation to answer
//! for it, which in practice means an `Ok(Vec::new())` in nine files. The two
//! seam types are wired independently in `orca`, so a build without a calculator
//! is a build that never mentions one.
//!
//! # Synchronous, for the same reason as the first seam
//!
//! The caller owns the concurrency: `orca` runs this on a `BackgroundExecutor`
//! via `cx.spawn`, so <kbd>Esc</kbd> can still cancel a response that is being
//! assembled. A provider that spawned its own thread would take that away.

use std::fmt;

use super::RawResult;

/// A source of answers to the query itself.
///
/// Implementations live here (pure Rust). The distinguishing property is that
/// the whole answer is a function of the query string, so a provider here needs
/// no clock, no filesystem, and no environment — which is why the calculator can
/// live in this crate and the installed-app provider cannot.
pub trait QueryProvider: Send + Sync {
    /// A stable name for logs and for de-duplicating against another provider.
    fn name(&self) -> &str;

    /// Answers `query`, in registration order with everything else that
    /// answered it.
    ///
    /// An empty vec is the normal case and not a failure: most keystrokes are
    /// not a question this seam can answer, and the seam has no error path
    /// precisely so that "nothing to say" needs no special handling downstream.
    fn respond(&self, query: &str) -> Vec<RawResult>;
}

/// A named, type-erased query provider.
///
/// The name is stored alongside for the same reason [`super::ProviderBox`]
/// stores one: a set can report what is wired up without calling into it. The
/// `+ Send + Sync` is load-bearing rather than decorative — the set is built on
/// the UI thread and answered on a background one.
pub struct QueryProviderBox {
    name: String,
    provider: Box<dyn QueryProvider + Send + Sync>,
}

impl QueryProviderBox {
    /// Wraps a provider under an explicit name.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        provider: impl QueryProvider + 'static,
    ) -> QueryProviderBox {
        QueryProviderBox {
            name: name.into(),
            provider: Box::new(provider),
        }
    }

    /// The name this provider was registered under.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Asks the wrapped provider.
    pub fn respond(&self, query: &str) -> Vec<RawResult> {
        self.provider.respond(query)
    }
}

impl fmt::Debug for QueryProviderBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryProviderBox")
            .field("name", &self.name)
            .finish()
    }
}

/// A collection of query providers that merges their answers.
///
/// De-duplication is by [`RawResult::id`], **first provider wins**. That is
/// [`super::ProviderSet::collect_all`]'s rule, restated rather than reinvented:
/// a provider registered earlier is a more authoritative one, and
/// "later overrides earlier" would make the result depend on registration order
/// in a way nothing else in the crate does.
#[derive(Debug, Default)]
pub struct QueryProviders {
    providers: Vec<QueryProviderBox>,
}

impl QueryProviders {
    /// An empty set.
    #[must_use]
    pub fn new() -> QueryProviders {
        QueryProviders::default()
    }

    /// Adds a provider, appending it after any already registered.
    #[must_use]
    pub fn with(
        mut self,
        name: impl Into<String>,
        provider: impl QueryProvider + 'static,
    ) -> QueryProviders {
        self.providers.push(QueryProviderBox::new(name, provider));
        self
    }

    /// The registered names, in registration order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.providers.iter().map(QueryProviderBox::name).collect()
    }

    /// Answers `query` from every provider, in order, dropping duplicate ids.
    ///
    /// No provider can fail here, so there is no `Result` and no error that
    /// stops the merge halfway: a query provider that cannot answer simply
    /// answers nothing. Contrast [`super::ProviderSet::collect_all`], where a
    /// failure stops everything because a partial catalogue is a lie.
    pub fn respond(&self, query: &str) -> Vec<RawResult> {
        let mut merged: Vec<RawResult> = Vec::new();
        for provider in &self.providers {
            for result in provider.respond(query) {
                if !merged.iter().any(|existing| existing.id == result.id) {
                    merged.push(result);
                }
            }
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LaunchTarget, Source};

    /// A query provider with a canned answer, so the merge rule can be observed
    /// without going through a real provider.
    struct Stub {
        name: &'static str,
        rows: Vec<RawResult>,
    }

    impl Stub {
        fn row(id: &str, title: &str) -> RawResult {
            RawResult::new(
                id,
                title,
                Source::Unknown,
                LaunchTarget::Uri(format!("https://example.invalid/{id}")),
            )
        }
    }

    impl QueryProvider for Stub {
        fn name(&self) -> &str {
            self.name
        }

        fn respond(&self, _query: &str) -> Vec<RawResult> {
            self.rows.clone()
        }
    }

    fn ids(rows: &[RawResult]) -> Vec<&str> {
        rows.iter().map(|row| row.id.as_str()).collect()
    }

    #[test]
    fn an_empty_set_answers_nothing() {
        assert!(QueryProviders::new().respond("2*(3+4)").is_empty());
        assert!(QueryProviders::new().names().is_empty());
    }

    #[test]
    fn merges_in_registration_order() {
        let set = QueryProviders::new()
            .with(
                "first",
                Stub {
                    name: "first",
                    rows: vec![Stub::row("a", "a")],
                },
            )
            .with(
                "second",
                Stub {
                    name: "second",
                    rows: vec![Stub::row("b", "b")],
                },
            );
        assert_eq!(ids(&set.respond("anything")), vec!["a", "b"]);
    }

    #[test]
    fn the_first_provider_wins_on_a_duplicate_id() {
        // The rule `ProviderSet::collect_all` documents, asserted here so the
        // two seams cannot drift apart unnoticed: the earlier registration is
        // the authoritative one, and a later provider's version of the same id
        // is discarded rather than overriding it.
        let set = QueryProviders::new()
            .with(
                "first",
                Stub {
                    name: "first",
                    rows: vec![Stub::row("dup", "from first"), Stub::row("only-first", "x")],
                },
            )
            .with(
                "second",
                Stub {
                    name: "second",
                    rows: vec![
                        Stub::row("dup", "from second"),
                        Stub::row("only-second", "y"),
                    ],
                },
            );

        let rows = set.respond("dup");
        assert_eq!(ids(&rows), vec!["dup", "only-first", "only-second"]);
        assert_eq!(
            rows[0].title, "from first",
            "the first registration owns the id"
        );
    }

    #[test]
    fn names_reports_registration_order() {
        let set = QueryProviders::new()
            .with(
                "calculator",
                Stub {
                    name: "ignored",
                    rows: Vec::new(),
                },
            )
            .with(
                "units",
                Stub {
                    name: "ignored",
                    rows: Vec::new(),
                },
            );
        assert_eq!(set.names(), vec!["calculator", "units"]);
    }

    #[test]
    fn a_silent_provider_does_not_disturb_the_others() {
        let set = QueryProviders::new()
            .with(
                "quiet",
                Stub {
                    name: "quiet",
                    rows: Vec::new(),
                },
            )
            .with(
                "loud",
                Stub {
                    name: "loud",
                    rows: vec![Stub::row("a", "a")],
                },
            );
        assert_eq!(ids(&set.respond("anything")), vec!["a"]);
    }
}
