//! The command provider: user-defined aliases from `config.toml`.
//!
//! The highest-trust provider in the crate, and the cheapest: no I/O, no
//! environment, no filesystem. Everything it offers was typed by the user, so
//! its output is entirely a function of the config it was built from — which
//! means every assertion about it is free.
//!
//! ```
//! use orca_core::providers::{Alias, CommandProvider, ResultProvider};
//!
//! let provider = CommandProvider::new(vec![
//!     Alias::new("gh", "gh").with_args(["pr", "status"]),
//!     Alias::new("reload", r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
//! ]);
//!
//! let results = provider.collect().expect("no I/O, so no failure");
//! assert_eq!(results.len(), 2);
//! assert_eq!(results[0].id, "cmd:gh");
//! assert_eq!(results[0].title, "gh");
//! ```

use crate::model::{LaunchTarget, Source};

use super::{ProviderError, RawResult, ResultProvider};

/// One user-defined shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    /// What the user types. Matched like any other title.
    pub name: String,
    /// Program to run, resolved through `PATH` by the launcher.
    pub program: String,
    /// Arguments passed on every activation.
    pub args: Vec<String>,
    /// Optional second line, for a note about what the alias does.
    pub subtitle: Option<String>,
}

impl Alias {
    /// Builds an alias with no arguments and no subtitle.
    ///
    /// Blank names and programs are rejected by returning the alias with an
    /// empty name, which the provider then skips — validation belongs to
    /// [`crate::config`], and a provider handed a bad alias should drop it
    /// quietly rather than panic in the middle of a keystroke.
    #[must_use]
    pub fn new(name: impl Into<String>, program: impl Into<String>) -> Alias {
        Alias {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            subtitle: None,
        }
    }

    /// Builder-style setter for [`Alias::args`].
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Alias
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Builder-style setter for [`Alias::subtitle`]; blank becomes `None`.
    #[must_use]
    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Alias {
        let subtitle = subtitle.into();
        self.subtitle = if subtitle.trim().is_empty() {
            None
        } else {
            Some(subtitle)
        };
        self
    }

    /// Whether this alias is usable. See [`Alias::new`].
    #[must_use]
    pub fn is_usable(&self) -> bool {
        !self.name.trim().is_empty() && !self.program.trim().is_empty()
    }
}

/// Serves the aliases from `[[commands.aliases]]`.
///
/// Ids are `cmd:<lowercased name>`, so the same alias always keys the same
/// frecency row regardless of how the user capitalised it in the config file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandProvider {
    aliases: Vec<Alias>,
}

impl CommandProvider {
    /// Builds a provider from a list of aliases.
    ///
    /// Unusable aliases are dropped here rather than at collection time, so the
    /// count is stable and a test can assert on it.
    #[must_use]
    pub fn new(aliases: impl IntoIterator<Item = Alias>) -> CommandProvider {
        let mut provider = CommandProvider::default();
        for alias in aliases {
            provider.push(alias);
        }
        provider
    }

    /// Builds a provider straight from the config section.
    #[must_use]
    pub fn from_config(config: &crate::config::Commands) -> CommandProvider {
        CommandProvider::new(config.aliases.iter().map(|alias| {
            let mut built = Alias::new(alias.name.clone(), alias.program.clone())
                .with_args(alias.args.iter().cloned());
            if let Some(subtitle) = &alias.subtitle {
                built = built.with_subtitle(subtitle.clone());
            }
            built
        }))
    }

    /// Adds one alias, ignoring an unusable one.
    pub fn push(&mut self, alias: Alias) {
        if alias.is_usable() {
            self.aliases.push(alias);
        }
    }

    /// How many aliases are served.
    #[must_use]
    pub fn len(&self) -> usize {
        self.aliases.len()
    }

    /// Whether there is nothing to serve.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.aliases.is_empty()
    }
}

impl ResultProvider for CommandProvider {
    fn name(&self) -> &str {
        "commands"
    }

    fn collect(&self) -> Result<Vec<RawResult>, ProviderError> {
        Ok(self
            .aliases
            .iter()
            .map(|alias| {
                let mut result = RawResult::new(
                    format!("cmd:{}", alias.name.to_lowercase()),
                    alias.name.clone(),
                    Source::Command,
                    LaunchTarget::Command {
                        program: alias.program.clone(),
                        args: alias.args.clone(),
                    },
                );
                if let Some(subtitle) = &alias.subtitle {
                    result = result.with_subtitle(subtitle.clone());
                }
                result
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn provider() -> CommandProvider {
        CommandProvider::new([
            Alias::new("gh", "gh").with_args(["pr", "status"]),
            Alias::new("Reload", r"C:\Windows\System32\cmd.exe").with_subtitle("restart"),
        ])
    }

    #[test]
    fn collects_one_candidate_per_alias() {
        let results = provider().collect().expect("no I/O");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "cmd:gh");
        assert_eq!(results[0].title, "gh");
        assert_eq!(results[1].id, "cmd:reload", "the id is lowercased");
        assert_eq!(
            results[1].title, "Reload",
            "the title keeps the user's casing"
        );
    }

    #[test]
    fn carries_the_target_and_the_args() {
        let results = provider().collect().expect("no I/O");
        assert_eq!(
            results[0].target,
            LaunchTarget::Command {
                program: "gh".to_owned(),
                args: vec!["pr".to_owned(), "status".to_owned()],
            }
        );
        assert_eq!(results[1].subtitle.as_deref(), Some("restart"));
        assert_eq!(results[0].subtitle, None);
    }

    #[test]
    fn every_result_is_a_command_source_with_no_prior() {
        for result in provider().collect().expect("no I/O") {
            assert_eq!(result.source, Source::Command);
            assert_eq!(result.score, 0.0, "a provider prior is user-signal only");
        }
    }

    #[test]
    fn unusable_aliases_are_dropped_at_construction() {
        let provider = CommandProvider::new([
            Alias::new("ok", "ok"),
            Alias::new("  ", "ok"),
            Alias::new("ok2", "   "),
            Alias::new("", ""),
        ]);
        assert_eq!(provider.len(), 1);
        assert_eq!(provider.collect().expect("no I/O").len(), 1);
    }

    #[test]
    fn an_empty_provider_serves_nothing() {
        let provider = CommandProvider::new([]);
        assert!(provider.is_empty());
        assert!(provider.collect().expect("no I/O").is_empty());
    }

    #[test]
    fn builds_from_the_config_section() {
        let config = config::Config::load_from_str(
            r#"
            [[commands.aliases]]
            name = "ls"
            program = "ls"
            args = ["-la"]
            subtitle = "long listing"
            "#,
            "test.toml",
        )
        .expect("config");
        let results = CommandProvider::from_config(&config.commands)
            .collect()
            .expect("no I/O");
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].target,
            LaunchTarget::Command {
                program: "ls".to_owned(),
                args: vec!["-la".to_owned()],
            }
        );
        assert_eq!(results[0].subtitle.as_deref(), Some("long listing"));
    }

    #[test]
    fn config_rejects_a_blank_alias_before_the_provider_ever_sees_it() {
        // The layering point: validation lives in config, so the provider is
        // only ever handed well-formed aliases. Its own `is_usable` filter is a
        // second line of defence, not the primary check.
        let error = config::Config::load_from_str(
            "[[commands.aliases]]\nname = \"  \"\nprogram = \"nope\"\n",
            "test.toml",
        )
        .expect_err("must fail");
        assert!(error.to_string().contains("aliases[0].name"), "{error}");
    }

    #[test]
    fn aliases_rank_by_name_among_other_providers() {
        use crate::{rank_at, Frecency, RankingPolicy, Timestamp};

        let items: Vec<crate::ResultItem> = provider()
            .collect()
            .expect("no I/O")
            .into_iter()
            .map(|raw| raw.into_item(Frecency::NEVER))
            .collect();
        let now = Timestamp::from_unix_seconds(1_000);
        let ranked = rank_at(RankingPolicy::DEFAULT, "rel", now, &items);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].item.title, "Reload");
    }
}
