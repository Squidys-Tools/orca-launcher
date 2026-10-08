//! The environment-variable provider.
//!
//! Orca does not read the environment; the caller does. `EnvVarProvider` is
//! handed a list of `(name, value)` pairs — `std::env::vars()` collected once
//! by the composition root, or a hand-written list in a test — and turns each
//! into a candidate.
//!
//! That indirection buys three things:
//!
//! * the provider is pure and its tests need no environment at all;
//! * a caller can filter the variable list before it gets here, so a secret
//!   never reaches a result row just because it was in the process
//!   environment;
//! * a test can prove the exclusion rules below, which are the only interesting
//!   part of this provider.
//!
//! # What is excluded, and why
//!
//! * **Name shape.** A Windows variable name is upper-case ASCII plus digits
//!   and `_`, which is exactly what `std::env::vars` produces. Anything else
//!   did not come from a real environment and is dropped.
//! * **`=` in the value.** A variable containing `=` cannot be passed through a
//!   shell-style command line unambiguously. It is still indexed and
//!   searchable — dropping it entirely would be worse than not being able to
//!   *run* it — but it is marked not-runnable. See
//!   [`EnvVar::is_runnable`].
//! * **Boring prefixes.** `__COMPAT_LAYER`, `=C:`, and the rest of what a
//!   shell seeds. They are noise in a launcher and they are never what anyone
//!   types.
//! * **Length.** A value over [`EnvVar::MAX_VALUE_LEN`] is truncated. A
//!   multi-megabyte `PATH`-adjacent variable rendered into a result row is a
//!   frame-time problem, and the prefix is what a human reads anyway.

#[cfg(test)]
use std::collections::BTreeMap;

use crate::model::{LaunchTarget, Source};

use super::{ProviderError, RawResult, ResultProvider};

/// Longest value rendered into a candidate's subtitle before truncation.
pub const MAX_VALUE_LEN: usize = 200;

/// Whether a string would read as a filesystem path on screen.
///
/// Windows-shaped on purpose, because this is a Windows-only app: a drive
/// letter, a UNC prefix, or any backslash. A forward slash does **not** count,
/// so a URL — which is full of them — is not mistaken for a path.
///
/// This exists because `subtitle` is rendered and `keywords` is not, and putting
/// a path in the wrong one is the mistake worth making impossible to repeat.
/// `no_provider_renders_a_path` holds every provider to it.
///
/// No separator and no drive letter means there is nothing here to click, copy,
/// or mistype into a file manager, so it is not a path for this purpose however
/// much it happens to look like one.
#[must_use]
pub fn looks_like_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    let has_drive = bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic();
    text.starts_with(r"\\") || has_drive || text.contains('\\')
}

/// One environment variable, as offered to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    /// The variable name, e.g. `EDITOR`.
    pub name: String,
    /// Its value. Truncated for display by the provider.
    pub value: String,
    /// Whether the value was truncated.
    pub truncated: bool,
    /// Whether the value can be used on a command line at all.
    ///
    /// `false` means the value contains `=`; the variable is still listed and
    /// still searchable, but activating it will not produce a sane command
    /// line, and the UI should say so rather than pretending.
    pub runnable: bool,
}

impl EnvVar {
    /// Builds a variable, truncating the value and deciding runnability.
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> EnvVar {
        let name = name.into();
        let raw = value.into();
        let truncated = raw.chars().count() > MAX_VALUE_LEN;
        EnvVar {
            name,
            runnable: !raw.contains('='),
            value: truncate(&raw),
            truncated,
        }
    }

    /// Whether the name looks like a real environment variable.
    ///
    /// Upper-case ASCII, digits, `_`, `=`, and parentheses, non-empty. The
    /// parentheses are there because real Windows variables use them —
    /// `ProgramFiles(x86)` is a genuine environment entry on most machines, and
    /// a rule that rejected it would be a rule that rejected a common case.
    ///
    /// `=` is allowed in a *name* because the process environment block is not
    /// required to hold tidy names, and dropping a variable because its name is
    /// unusual would be a silent hole in the index — a hole a user cannot see
    /// and cannot report. The `=` in the *value* is what
    /// [`EnvVar::is_runnable`] refuses, because that is the one that cannot be
    /// passed on a command line.
    ///
    /// Lower case is rejected: on Windows it does not occur in a real
    /// environment, and accepting it would put lookalikes into the index.
    #[must_use]
    pub fn has_plausible_name(name: &str) -> bool {
        !name.is_empty()
            && name.chars().all(|c| {
                c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '=' | '(' | ')')
            })
    }

    /// Whether the value can be passed on a command line.
    #[must_use]
    pub fn is_runnable(&self) -> bool {
        self.runnable
    }
}

/// Cuts `value` to [`MAX_VALUE_LEN`] characters, appending an ellipsis marker.
///
/// Sliced on a `char` boundary, so a truncated multi-byte value cannot panic
/// and cannot end mid-codepoint.
fn truncate(value: &str) -> String {
    if value.chars().count() <= MAX_VALUE_LEN {
        return value.to_owned();
    }
    let mut truncated: String = value.chars().take(MAX_VALUE_LEN).collect();
    truncated.push('…');
    truncated
}

/// Serves environment variables as candidates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvVarProvider {
    variables: Vec<EnvVar>,
}

impl EnvVarProvider {
    /// Builds a provider from whatever the caller collected.
    ///
    /// Filtering happens here, once, so [`ResultProvider::collect`] is a pure
    /// projection of an already-valid list.
    #[must_use]
    pub fn new<I, N, V>(variables: I) -> EnvVarProvider
    where
        I: IntoIterator<Item = (N, V)>,
        N: Into<String>,
        V: Into<String>,
    {
        let mut provider = EnvVarProvider::default();
        for (name, value) in variables {
            let name = name.into();
            if Self::is_boring(&name) || !EnvVar::has_plausible_name(&name) {
                continue;
            }
            provider.variables.push(EnvVar::new(name, value));
        }
        // Sorted so the candidate order does not depend on how the caller
        // happened to enumerate the environment. The ranker is order-independent
        // today, but a provider that is itself deterministic is one less thing
        // to reason about.
        provider.variables.sort_by(|a, b| a.name.cmp(&b.name));
        provider
    }

    /// Names that are never useful and are usually numerous.
    fn is_boring(name: &str) -> bool {
        name.starts_with("__COMPAT_LAYER")
            || name.starts_with("__PSLockDownPolicy")
            || name == "="
            || name.starts_with("=C:")
    }

    /// How many variables are served.
    #[must_use]
    pub fn len(&self) -> usize {
        self.variables.len()
    }

    /// Whether there is nothing to serve.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.variables.is_empty()
    }

    /// Looks up one served variable by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&EnvVar> {
        self.variables.iter().find(|variable| variable.name == name)
    }
}

impl ResultProvider for EnvVarProvider {
    fn name(&self) -> &str {
        "env"
    }

    fn collect(&self) -> Result<Vec<RawResult>, ProviderError> {
        Ok(self
            .variables
            .iter()
            .map(|variable| {
                let mut result = RawResult::new(
                    format!("env:{}", variable.name),
                    variable.name.clone(),
                    Source::Command,
                    LaunchTarget::Command {
                        program: variable.name.clone(),
                        args: Vec::new(),
                    },
                );
                // An environment variable's value is its content — `EDITOR` and
                // `NODE_OPTIONS` are useless without it — so it is the subtitle
                // whenever it is safe to read.
                //
                // It is frequently *not* safe: `USERPROFILE`, `TEMP`, and
                // `COMSPEC` are all paths, and half of `PATH` is a wall of them.
                // A path-valued variable keeps its value as `keywords`, so
                // searching still works, and renders no second line at all. An
                // empty row is the honest answer; a row of `C:\Users\chris` is
                // the noise the rule exists to prevent.
                if looks_like_path(&variable.value) {
                    result = result.with_keywords(variable.value.clone());
                } else {
                    result = result.with_subtitle(variable.value.clone());
                }
                // A non-runnable variable still shows up, and the subtitle says
                // so, because a launcher that hides a variable the user can see
                // in `set` is just lying about its own index.
                if !variable.is_runnable() {
                    result = result.with_score(0.0);
                }
                result
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> EnvVarProvider {
        EnvVarProvider::new([
            ("EDITOR", "code --wait"),
            ("USERPROFILE", r"C:\Users\chris"),
            ("__COMPAT_LAYER=RunAsInvoker", ""),
            ("=C:", "C:\\Windows"),
            ("lowercase", "no"),
            ("WITH=EQUALS", "a=b"),
        ])
    }

    #[test]
    fn filters_boring_names_lowercase_and_unbindable_shells() {
        let provider = provider();
        let results = provider.collect().expect("no I/O");
        let names: Vec<&str> = results.iter().map(|result| result.id.as_str()).collect();
        assert_eq!(names, ["env:EDITOR", "env:USERPROFILE", "env:WITH=EQUALS"]);
        assert_eq!(provider.len(), 3);
    }

    #[test]
    fn output_is_sorted_so_it_does_not_depend_on_enumeration_order() {
        let forwards = EnvVarProvider::new([("A", "1"), ("B", "2"), ("C", "3")]);
        let backwards = EnvVarProvider::new([("C", "3"), ("B", "2"), ("A", "1")]);
        assert_eq!(forwards, backwards);
        let names: Vec<String> = forwards
            .collect()
            .expect("no I/O")
            .into_iter()
            .map(|r| r.title)
            .collect();
        assert_eq!(names, ["A", "B", "C"]);
    }

    #[test]
    fn a_value_containing_equals_is_listed_but_not_runnable() {
        let provider = provider();
        let variable = provider.get("WITH=EQUALS").expect("still listed");
        assert!(!variable.is_runnable());

        let results = provider.collect().expect("no I/O");
        let awkward = results
            .iter()
            .find(|r| r.title == "WITH=EQUALS")
            .expect("still served");
        // Searchable, with a zero prior so it does not outrank a real alias.
        assert_eq!(awkward.subtitle.as_deref(), Some("a=b"));
        assert_eq!(awkward.score, 0.0);
    }

    #[test]
    fn a_path_valued_variable_is_searchable_but_renders_nothing() {
        // `USERPROFILE` and `TEMP` are the variables people most often look up,
        // so dropping them would be worse than useless — and a row reading
        // `C:\Users\chris` is exactly what the no-paths rule is about.
        //
        // Both halves asserted: the value moves to `keywords` rather than being
        // discarded, and it does not stay in `subtitle`.
        let provider = EnvVarProvider::new(vec![
            ("USERPROFILE".to_owned(), r"C:\Users\chris".to_owned()),
            ("EDITOR".to_owned(), "code --wait".to_owned()),
        ]);
        let results = provider.collect().expect("no I/O");

        let profile = results
            .iter()
            .find(|r| r.title == "USERPROFILE")
            .expect("still served");
        assert_eq!(profile.subtitle, None, "a path must never be rendered");
        assert_eq!(profile.keywords.as_deref(), Some(r"C:\Users\chris"));

        // A non-path value is still the content of the row, and still shown.
        let editor = results
            .iter()
            .find(|r| r.title == "EDITOR")
            .expect("still served");
        assert_eq!(editor.subtitle.as_deref(), Some("code --wait"));
    }

    #[test]
    fn looks_like_path_is_windows_shaped_and_spares_urls() {
        // The predicate decides what gets hidden, so it is worth pinning on both
        // sides. A false positive costs a row its value; a false negative puts a
        // path back on screen.
        for path in [
            r"C:\Users\chris\notes.txt",
            r"\\server\share\file",
            r"relative\to\thing",
            "C:file.txt",
        ] {
            assert!(looks_like_path(path), "{path:?} is a path");
        }
        // A URL is full of forward slashes and is not a path. Mistaking one for
        // a path would blank the subtitle of every web-search result.
        for text in [
            "https://example.com/search?q=rust",
            "code --wait",
            "Windows_NT",
            "a=b",
            "plain text",
        ] {
            assert!(!looks_like_path(text), "{text:?} is not a path");
        }
    }

    #[test]
    fn a_long_value_is_truncated_on_a_char_boundary() {
        let value = "é".repeat(MAX_VALUE_LEN + 50);
        let variable = EnvVar::new("BIG", value);
        assert!(variable.truncated);
        assert!(variable.is_runnable());
        // MAX_VALUE_LEN chars plus the marker, and no panic from slicing.
        assert_eq!(variable.value.chars().count(), MAX_VALUE_LEN + 1);
        assert!(variable.value.ends_with('…'));
    }

    #[test]
    fn a_short_value_is_untouched() {
        let variable = EnvVar::new("K", "v");
        assert!(!variable.truncated);
        assert_eq!(variable.value, "v");
        assert!(!variable.value.contains('…'));
    }

    #[test]
    fn names_are_validated_by_shape() {
        assert!(EnvVar::has_plausible_name("PATH"));
        assert!(EnvVar::has_plausible_name("PROGRAMFILES(X86)"));
        assert!(EnvVar::has_plausible_name("_A1"));
        // `=` in a *name* is allowed: the environment block is not required to
        // hold tidy names, and dropping one silently would be an invisible hole
        // in the index.
        assert!(EnvVar::has_plausible_name("ODD=NAME"));
        assert!(!EnvVar::has_plausible_name(""));
        assert!(!EnvVar::has_plausible_name("lowercase"));
        assert!(!EnvVar::has_plausible_name("WITH SPACE"));
        assert!(!EnvVar::has_plausible_name("É"));
    }

    #[test]
    fn an_empty_environment_serves_nothing() {
        let provider = EnvVarProvider::new(Vec::<(String, String)>::new());
        assert!(provider.is_empty());
        assert!(provider.collect().expect("no I/O").is_empty());
    }

    #[test]
    fn a_real_environment_is_never_consulted_by_this_module() {
        // Not a test of behaviour but of the boundary: constructing a provider
        // from an empty list must give an empty provider even though this
        // process certainly has variables set.
        let provider = EnvVarProvider::new(Vec::<(String, String)>::new());
        assert!(provider.is_empty());
    }

    #[test]
    fn results_rank_by_name() {
        use crate::{rank_at, Frecency, RankingPolicy, Timestamp};

        let items: Vec<crate::ResultItem> = provider()
            .collect()
            .expect("no I/O")
            .into_iter()
            .map(|raw| raw.into_item(Frecency::NEVER))
            .collect();
        let ranked = rank_at(
            RankingPolicy::DEFAULT,
            "editor",
            Timestamp::from_unix_seconds(1),
            &items,
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].item.title, "EDITOR");
    }

    #[test]
    fn the_subtitle_is_searchable_too() {
        // Typing a path fragment should find the variable that holds it.
        use crate::{rank_at, Frecency, RankingPolicy, Timestamp};

        let items: Vec<crate::ResultItem> = provider()
            .collect()
            .expect("no I/O")
            .into_iter()
            .map(|raw| raw.into_item(Frecency::NEVER))
            .collect();
        let ranked = rank_at(
            RankingPolicy::DEFAULT,
            "chris",
            Timestamp::from_unix_seconds(1),
            &items,
        );
        assert!(ranked.iter().any(|r| r.item.title == "USERPROFILE"));
    }

    #[test]
    fn a_lookup_is_correct_for_many_variables() {
        let variables: Vec<(String, String)> = (0..500)
            .map(|index| (format!("VAR{index:04}"), index.to_string()))
            .collect();
        let provider = EnvVarProvider::new(variables);
        assert_eq!(provider.len(), 500);
        assert_eq!(
            provider.get("VAR0250").map(|v| v.value.as_str()),
            Some("250")
        );
        assert!(provider.get("VAR9999").is_none());
    }

    #[test]
    fn an_id_is_derived_from_the_name_so_history_is_stable() {
        use crate::store::{LaunchRecord, MemoryUsageStore, UsageStore};

        let mut store = MemoryUsageStore::new();
        let results = provider().collect().expect("no I/O");
        let first = results[0].clone();
        store
            .record_launch(LaunchRecord {
                item_id: &first.id,
                source: first.source,
                title: &first.title,
                at: crate::Timestamp::from_unix_seconds(10),
            })
            .expect("launch");
        assert_eq!(
            store
                .frecency(&first.id)
                .expect("frecency")
                .map(|f| f.launches()),
            Some(1)
        );
        // Rebuilding the provider yields the same id, so the history is found.
        let rebuilt = provider().collect().expect("no I/O");
        assert_eq!(rebuilt[0].id, first.id);
    }

    #[test]
    fn a_btreemap_of_variables_is_accepted() {
        // The shape a caller most naturally has after collecting
        // `std::env::vars()` and de-duplicating.
        let map: BTreeMap<String, String> = BTreeMap::from([
            ("EDITOR".to_owned(), "code".to_owned()),
            ("SHELL".to_owned(), "pwsh".to_owned()),
        ]);
        let provider = EnvVarProvider::new(map);
        assert_eq!(provider.len(), 2);
        assert_eq!(
            provider.get("SHELL").map(|v| v.value.as_str()),
            Some("pwsh")
        );
    }
}
