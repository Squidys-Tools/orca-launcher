//! `orca-core` — the pure-logic half of the orca launcher.
//!
//! This crate holds the domain model (what a search result *is*), the ranking
//! rules (how a result is scored against a query), the config file, the
//! frecency store, and the provider abstraction.
//!
//! # What "pure" means here
//!
//! `orca-core` must not depend on `gpui`, nor on any crate that binds Win32.
//! Platform concerns live in `orca-win`; presentation lives in `orca`. See
//! `docs/ARCHITECTURE.md` for the reasoning, which is load-bearing rather than
//! stylistic.
//!
//! The stricter form of the same rule, from the layering notes: **no clock, no
//! filesystem, no environment, no global state.** Every one of those arrives as
//! an explicit argument or as a trait the caller implements:
//!
//! * time is a [`frecency::Timestamp`] passed in, not `SystemTime::now`;
//! * the filesystem is [`config::ConfigPaths`] and
//!   [`providers::DirectoryLister`], both constructed by the caller;
//! * the environment is read by the caller and handed to
//!   [`providers::EnvVarProvider`];
//! * persistence is the [`store::UsageStore`] trait, with an in-memory
//!   implementation here and a SQLite one that only ever opens a path the
//!   caller chose.
//!
//! The payoff is that the ranking rules can be asserted thousands of times in
//! milliseconds. A ranking function you can only test by running a window is a
//! ranking function you will not test.
//!
//! # Module map
//!
//! | module | what it owns |
//! |---|---|
//! | [`model`] | [`ResultItem`], [`Source`], [`LaunchTarget`] |
//! | [`matching`] | how well a candidate answers a query, tier by tier |
//! | [`frecency`] | launch counts, timestamps, and exponential decay |
//! | [`policy`] | the tunable weights, and the one place they are combined |
//! | [`rank`] | ordering a catalog against a query |
//! | [`text`] | case folding and word segmentation |
//! | [`config`] | `config.toml` loading, validation, and path construction |
//! | [`store`] | the [`store::UsageStore`] trait, memory and SQLite backends |
//! | [`providers`] | the [`providers::ResultProvider`] seam and its pure impls, plus the [`providers::QueryProvider`] seam for answers to the query |
//!
//! # Where a caller starts
//!
//! ```
//! use orca_core::{rank_at, LaunchTarget, RankingPolicy, ResultItem, Source, Timestamp};
//!
//! let catalog = vec![
//!     ResultItem::new("app:notepad", "Notepad", Source::Application,
//!         LaunchTarget::Command { program: "notepad".into(), args: vec![] }),
//!     ResultItem::new("app:calc", "Calculator", Source::Application,
//!         LaunchTarget::Command { program: "calc".into(), args: vec![] }),
//! ];
//!
//! let ranked = rank_at(
//!     RankingPolicy::DEFAULT,
//!     "note",
//!     Timestamp::from_unix_seconds(1_700_000_000),
//!     &catalog,
//! );
//! assert_eq!(ranked.len(), 1);
//! assert_eq!(ranked[0].item.title, "Notepad");
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod config;
pub mod frecency;
pub mod matching;
pub mod model;
pub mod policy;
pub mod providers;
pub mod rank;
pub mod store;
mod text;

pub use frecency::{Frecency, Timestamp};
pub use matching::{
    classify, match_score, match_score_with_subtitle, MatchKind, MatchQuality, BROWSE_SCORE,
    CONTAINS_SCORE, EXACT_SCORE, FUZZY_CEILING, FUZZY_FLOOR, PREFIX_SCORE, WORD_BOUNDARY_SCORE,
};
pub use model::{LaunchTarget, ResultItem, Source};
pub use policy::{RankingPolicy, WeightTable};
pub use providers::looks_like_path;
pub use providers::{
    evaluate, format_value, CalculatorProvider, ProviderError, QueryProvider, QueryProviders,
    RawResult, ResultProvider,
};
pub use rank::{rank, rank_at, rank_with, RankedItem};
