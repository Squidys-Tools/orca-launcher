//! `config.toml`: loading, validation, and the errors you get when it is wrong.
//!
//! The file lives at `%APPDATA%\Orca\config.toml`. `orca-core` does not know
//! that: it takes a [`ConfigPaths`] built by the caller, which is what keeps
//! the crate off `std::env` and lets every test point at a temporary
//! directory. See [`paths`].
//!
//! # Failure is a value, not a panic
//!
//! Every way this module can fail is a [`ConfigError`] carrying the file path,
//! the offending key, the value it read, and what to do about it. A launcher
//! that will not start because a config file has a typo is worse than a launcher
//! that starts with defaults and prints one actionable line — so
//! [`Config::load`] treats *every* parse and validation failure as a value the
//! caller can show, and falls back to [`Config::default`] on its own terms.
//!
//! ```
//! use orca_core::config::{Config, ConfigPaths};
//!
//! let config = Config::load_from_str(
//!     r#"
//!     [general]
//!     hotkey = "Ctrl+Alt+Space"
//!     theme = "dark"
//!     "#,
//!     "test.toml",
//! )
//! .expect("valid config");
//! assert_eq!(config.general.theme.as_str(), "dark");
//! assert_eq!(config.general.hotkey.as_str(), "Ctrl+Alt+Space");
//!
//! // A missing file is a first run, not a failure: the defaults come back.
//! // The root is the caller's choice, which is what keeps this crate off
//! // `std::env`.
//! let paths = ConfigPaths::at("Z:/orca-config-doctest-does-not-exist");
//! let config = Config::load(&paths).expect("a missing file is a first run");
//! assert_eq!(config, Config::default());
//!
//! // A malformed one is an error, and the message says where and how to fix it.
//! let error = Config::load_from_str("[search]\nhalf_life_days = 0\n", "test.toml")
//!     .expect_err("out of range");
//! assert!(error.to_string().contains("search.half_life_days"), "{error}");
//! ```

mod hotkey;
pub mod paths;

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use hotkey::{HotkeySpec, HotkeySpecError};
pub use paths::ConfigPaths;

use crate::frecency::SECONDS_PER_DAY;
use crate::model::Source;
use crate::policy::{RankingPolicy, WeightTable};

/// Largest `general.max_results` the UI is willing to be asked for.
///
/// A cap rather than a range: an unbounded `max_results` is a way for a typo to
/// ask the UI for a million rows, and the honest fix for that is to render fewer
/// rows, not to allocate them.
pub const MAX_RESULT_LIMIT: usize = 1_000;

/// Largest `files.max_results` a walk is allowed to return.
///
/// Deliberately much higher than [`MAX_RESULT_LIMIT`]: this bounds the *walk*,
/// not the list. A user who searches 200 000 files still wants 50 rows, and
/// capping the walk at the row count would silently truncate a legitimate index
/// to whatever the popup happens to display.
pub const MAX_FILE_RESULT_LIMIT: usize = 200_000;

/// Largest half-life the decay maths will accept, in days.
pub const MAX_HALF_LIFE_DAYS: u32 = 3_650;

/// The whole config file.
///
/// Every section defaults, so a config with a single key is valid and a config
/// with none is valid. Unknown keys are *rejected* rather than ignored: a
/// silently-ignored `thme = "dark"` is a bug report three weeks later.
///
/// `Default` is derived rather than written out, which is only possible because
/// every section derives it from the crate's own policy constants — a
/// hand-written `Default` would be a second place for the shipped defaults to
/// live and drift.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// What the launcher does at startup.
    pub general: General,
    /// How results are scored.
    pub search: Search,
    /// Per-kind overrides of the default trust weights.
    pub sources: Sources,
    /// Where and how deep to search for files.
    pub files: Files,
    /// User-defined command shortcuts.
    pub commands: Commands,
}

/// `[general]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// Global hotkey, as a spec string. See [`HotkeySpec`].
    pub hotkey: HotkeySpec,
    /// Light, dark, or follow the OS.
    pub theme: ThemePreference,
    /// How many rows the result list may show.
    pub max_results: usize,
}

impl Default for General {
    fn default() -> General {
        General {
            hotkey: HotkeySpec::DEFAULT_SPEC
                .parse()
                .expect("the compiled-in default hotkey is a valid spec"),
            theme: ThemePreference::default(),
            max_results: 50,
        }
    }
}

/// `[search]` — the tunable half of [`RankingPolicy`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Search {
    /// Drop results scoring below this. `0.0` shows everything that matched.
    pub min_score: f64,
    /// Recency half-life, in days.
    pub half_life_days: u32,
    /// Share of the final score driven by the query match.
    pub match_weight: f64,
    /// Share of the *prior* driven by the provider's own score.
    pub provider_share: f64,
    /// Shortest query that still gets typo tolerance.
    pub min_fuzzy_length: usize,
}

impl Default for Search {
    fn default() -> Search {
        Search {
            min_score: crate::policy::RankingPolicy::DEFAULT.min_score(),
            half_life_days: (crate::frecency::DEFAULT_HALF_LIFE.as_secs() / SECONDS_PER_DAY as u64)
                as u32,
            match_weight: crate::policy::DEFAULT_MATCH_WEIGHT,
            provider_share: crate::policy::DEFAULT_PROVIDER_SHARE,
            min_fuzzy_length: crate::policy::DEFAULT_MIN_FUZZY_LENGTH,
        }
    }
}

/// `[sources]` — optional per-kind trust overrides.
///
/// Each field is an `Option`, and `None` means "use the built-in default". That
/// is different from `0.0`, which means "trust this kind of result's history
/// not at all" — a distinction a plain `f64` field would lose.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sources {
    /// Override for [`Source::Application`].
    pub application: Option<f64>,
    /// Override for [`Source::File`].
    pub file: Option<f64>,
    /// Override for [`Source::Folder`].
    pub folder: Option<f64>,
    /// Override for [`Source::Command`].
    pub command: Option<f64>,
    /// Override for [`Source::WebSearch`].
    pub web_search: Option<f64>,
    /// Override for [`Source::Calculator`].
    pub calculator: Option<f64>,
    /// Override for [`Source::Clipboard`].
    pub clipboard: Option<f64>,
    /// Override for [`Source::Unknown`].
    pub unknown: Option<f64>,
}

impl Sources {
    /// The weight table this section describes.
    #[must_use]
    pub fn to_weight_table(self) -> WeightTable {
        let mut table = WeightTable::DEFAULT;
        for (source, override_) in [
            (Source::Application, self.application),
            (Source::File, self.file),
            (Source::Folder, self.folder),
            (Source::Command, self.command),
            (Source::WebSearch, self.web_search),
            (Source::Calculator, self.calculator),
            (Source::Clipboard, self.clipboard),
            (Source::Unknown, self.unknown),
        ] {
            if let Some(weight) = override_ {
                table = table.with(source, weight);
            }
        }
        table
    }
}

/// `[files]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Files {
    /// Whether the file provider runs at all.
    ///
    /// Defaults to **off**. Walking a directory tree is the most expensive
    /// thing orca does, and a user who has never heard of `[files]` should not
    /// pay for it. Turning this on without setting `roots` is a config error
    /// rather than a silent no-op.
    pub enabled: bool,
    /// Directories to search.
    pub roots: Vec<PathBuf>,
    /// How many levels below each root to descend.
    pub max_depth: usize,
    /// How many files a single collection may return.
    pub max_results: usize,
    /// Whether to follow directory symlinks and junctions.
    ///
    /// Off by default because a junction loop turns a bounded walk into an
    /// unbounded one, and `std::fs` has no way to detect it.
    pub follow_links: bool,
}

impl Default for Files {
    fn default() -> Files {
        Files {
            enabled: false,
            roots: Vec::new(),
            max_depth: 6,
            max_results: 2_000,
            follow_links: false,
        }
    }
}

/// `[commands]`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Commands {
    /// User-defined shortcuts, each written as `[[commands.aliases]]`.
    pub aliases: Vec<Alias>,
}

/// One `[[commands.aliases]]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    /// What the user types. Matched like any other title.
    pub name: String,
    /// Program to run, resolved through `PATH`.
    pub program: String,
    /// Arguments passed on every activation.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional second line, e.g. what the alias actually does.
    #[serde(default)]
    pub subtitle: Option<String>,
}

/// Which theme the UI should use.
///
/// Deserialised from a string case-insensitively, with a hand-written
/// [`serde::Deserializer`] rather than `rename_all = "lowercase"`, because the
/// derive is case-*sensitive* and a config written by a human as `Dark` should
/// work. The error message names the field and lists what is accepted, which
/// is more use to someone with a broken config than `unknown variant` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemePreference {
    /// Follow the OS setting. The default, and the right one: a launcher is
    /// not the place to make the user declare their preferences twice.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

impl ThemePreference {
    /// The spelling used in `config.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ThemePreference::System => "system",
            ThemePreference::Light => "light",
            ThemePreference::Dark => "dark",
        }
    }

    /// Every accepted spelling, for error messages.
    pub const VALUES: [&'static str; 3] = ["system", "light", "dark"];

    /// Parses one spelling, case- and whitespace-insensitively.
    ///
    /// `auto` is accepted as an alias for `system` because it is what a user
    /// reaches for first, and rejecting it would be a pointless papercut.
    #[must_use]
    pub fn parse(value: &str) -> Option<ThemePreference> {
        match value.trim().to_ascii_lowercase().as_str() {
            "system" | "auto" => Some(ThemePreference::System),
            "light" => Some(ThemePreference::Light),
            "dark" => Some(ThemePreference::Dark),
            _ => None,
        }
    }
}

impl std::fmt::Display for ThemePreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ThemePreference {
    type Err = ();

    fn from_str(value: &str) -> Result<ThemePreference, ()> {
        ThemePreference::parse(value).ok_or(())
    }
}

impl Serialize for ThemePreference {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ThemePreference {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        ThemePreference::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "theme must be one of {}, or \"auto\" for the system theme; got {raw:?}",
                ThemePreference::VALUES
                    .iter()
                    .map(|value| format!("{value:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }
}

impl Config {
    /// Reads and validates `paths.config_file()`.
    ///
    /// A **missing** file is not an error: it returns [`Config::default`],
    /// because first run has to work. An *unreadable* file, a *malformed* file,
    /// and a file that *fails validation* are all errors carrying enough detail
    /// to fix.
    pub fn load(paths: &ConfigPaths) -> Result<Config, ConfigError> {
        let path = paths.config_file();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.clone(),
                    source,
                })
            }
        };
        Config::load_from_str(&text, &path)
    }

    /// Parses and validates `text`, labelling errors with `origin`.
    ///
    /// `origin` is a path for a real load and an arbitrary label for a test, so
    /// the same code path is exercised by both.
    pub fn load_from_str(text: &str, origin: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let origin = origin.as_ref();
        let config: Config = toml::from_str(text).map_err(|error| ConfigError::Parse {
            path: origin.to_path_buf(),
            // `toml`'s Display already carries line, column, a caret, and
            // the offending line. Re-wrapping it would only lose detail.
            detail: error.to_string(),
        })?;
        config.validate(origin)?;
        Ok(config)
    }

    /// Writes this config back to `paths.config_file()`, creating the parent
    /// directory if needed.
    pub fn save(&self, paths: &ConfigPaths) -> Result<(), ConfigError> {
        self.validate(paths.config_file())?;
        let path = paths.config_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = toml::to_string_pretty(self).map_err(|error| ConfigError::Serialise {
            path: path.clone(),
            detail: error.to_string(),
        })?;
        std::fs::write(&path, text).map_err(|source| ConfigError::Write { path, source })
    }

    /// Checks every field that has a range or a shape, reporting the first
    /// problem with the file, the key, the value, and the fix.
    ///
    /// Runs on every load, including defaults. A default that fails its own
    /// validation is a bug in this file, and the same check catches it.
    pub fn validate(&self, origin: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = origin.as_ref().to_path_buf();
        let invalid = |key: &str, value: String, reason: &str| ConfigError::Invalid {
            path: path.clone(),
            key: key.to_owned(),
            value,
            reason: reason.to_owned(),
        };

        // HotkeySpec already parsed to get here; re-checking its canonical form
        // is the cheap way to catch a spec that parsed but serialises to
        // something unusable.
        if self.general.hotkey.as_str().is_empty() {
            return Err(invalid(
                "general.hotkey",
                String::new(),
                "a hotkey spec must name a key, e.g. \"Ctrl+Shift+Space\"",
            ));
        }

        if self.general.max_results == 0 || self.general.max_results > MAX_RESULT_LIMIT {
            return Err(invalid(
                "general.max_results",
                self.general.max_results.to_string(),
                &format!("must be between 1 and {MAX_RESULT_LIMIT}"),
            ));
        }

        check_unit(
            &invalid,
            "search.min_score",
            self.search.min_score,
            "must be a number between 0.0 and 1.0",
        )?;
        check_unit(
            &invalid,
            "search.match_weight",
            self.search.match_weight,
            "must be a number between 0.0 and 1.0; 1.0 means history never affects the order",
        )?;
        check_unit(
            &invalid,
            "search.provider_share",
            self.search.provider_share,
            "must be a number between 0.0 and 1.0; 1.0 means provider score replaces launch history",
        )?;

        if self.search.half_life_days == 0 || self.search.half_life_days > MAX_HALF_LIFE_DAYS {
            return Err(invalid(
                "search.half_life_days",
                self.search.half_life_days.to_string(),
                &format!("must be between 1 and {MAX_HALF_LIFE_DAYS}"),
            ));
        }
        if self.search.min_fuzzy_length > 32 {
            return Err(invalid(
                "search.min_fuzzy_length",
                self.search.min_fuzzy_length.to_string(),
                "must be 32 or less; 0 disables typo tolerance entirely",
            ));
        }

        for (key, value) in [
            ("sources.application", self.sources.application),
            ("sources.file", self.sources.file),
            ("sources.folder", self.sources.folder),
            ("sources.command", self.sources.command),
            ("sources.web_search", self.sources.web_search),
            ("sources.calculator", self.sources.calculator),
            ("sources.clipboard", self.sources.clipboard),
            ("sources.unknown", self.sources.unknown),
        ] {
            if let Some(weight) = value {
                check_unit(
                    &invalid,
                    key,
                    weight,
                    "must be a number between 0.0 and 1.0",
                )?;
            }
        }

        if self.files.enabled && self.files.roots.is_empty() {
            return Err(ConfigError::Invalid {
                path: path.clone(),
                key: "files.roots".to_owned(),
                value: "[]".to_owned(),
                reason: format!(
                    "files.enabled is true but no roots are listed; add files.roots = [\"C:\\\\Users\\\\{}\"] or set files.enabled = false",
                    who_am_i()
                ),
            });
        }
        for (index, root) in self.files.roots.iter().enumerate() {
            if root.as_os_str().is_empty() {
                return Err(invalid(
                    &format!("files.roots[{index}]"),
                    String::new(),
                    "a search root cannot be empty",
                ));
            }
        }
        if self.files.max_depth == 0 {
            return Err(invalid(
                "files.max_depth",
                "0".to_owned(),
                "must be at least 1; a depth of 0 would search nothing",
            ));
        }
        if self.files.max_results == 0 || self.files.max_results > MAX_FILE_RESULT_LIMIT {
            return Err(invalid(
                "files.max_results",
                self.files.max_results.to_string(),
                &format!("must be between 1 and {MAX_FILE_RESULT_LIMIT}"),
            ));
        }

        let mut seen = std::collections::BTreeSet::new();
        for (index, alias) in self.commands.aliases.iter().enumerate() {
            let name = alias.name.trim();
            if name.is_empty() {
                return Err(invalid(
                    &format!("commands.aliases[{index}].name"),
                    alias.name.clone(),
                    "an alias needs a name to match on",
                ));
            }
            if alias.program.trim().is_empty() {
                return Err(invalid(
                    &format!("commands.aliases[{index}].program"),
                    alias.program.clone(),
                    "an alias needs a program to run",
                ));
            }
            if !seen.insert(name.to_ascii_lowercase()) {
                return Err(invalid(
                    &format!("commands.aliases[{index}].name"),
                    alias.name.clone(),
                    "two aliases share this name; the first one in the file would always win",
                ));
            }
        }

        Ok(())
    }

    /// The ranking policy this config describes.
    #[must_use]
    pub fn ranking_policy(&self) -> RankingPolicy {
        RankingPolicy::DEFAULT
            .with_match_weight(self.search.match_weight)
            .with_provider_share(self.search.provider_share)
            .with_half_life(Duration::from_secs(
                self.search.half_life_days as u64 * SECONDS_PER_DAY as u64,
            ))
            .with_min_fuzzy_length(self.search.min_fuzzy_length)
            .with_min_score(self.search.min_score)
            .with_sources(self.sources.clone().to_weight_table())
    }

    /// A commented, valid config file.
    ///
    /// Kept next to the parser so the two cannot drift: this string is parsed
    /// and validated in the tests below, so a key added to [`Config`] without
    /// a line here is a test failure rather than an undocumented feature.
    #[must_use]
    pub fn example_toml() -> String {
        let mut text = toml::to_string_pretty(&Config::default()).unwrap_or_default();
        text.insert_str(
            0,
            "# orca configuration.\n\
             # Every key is optional; this file shows the defaults.\n\
             # Unknown keys are an error, so a typo fails loudly here rather than\n\
             # being silently ignored.\n\
             #\n\
             # The default location is %APPDATA%\\Orca\\config.toml.\n\n",
        );
        text
    }
}

/// Rejects a value that is not a finite `0.0 ..= 1.0` fraction.
fn check_unit<F>(invalid: &F, key: &str, value: f64, reason: &str) -> Result<(), ConfigError>
where
    F: Fn(&str, String, &str) -> ConfigError,
{
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(invalid(key, format!("{value}"), reason))
    }
}

/// The current user name, for one error message.
///
/// Only ever used to build a *suggested* config line. It reads the environment,
/// which `Config::validate` would otherwise be careful not to do — so it is
/// reached for in exactly one place, and every other path stays
/// environment-free and testable.
fn who_am_i() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "<you>".to_owned())
}

/// Everything that can go wrong reading or validating a config file.
///
/// Recoverable on purpose: a caller can show the message and carry on with
/// [`Config::default`]. Nothing here panics, and nothing here is a process
/// exit.
#[derive(Debug)]
pub enum ConfigError {
    /// The file exists but could not be read.
    Read {
        /// The path that failed.
        path: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// The file could not be written.
    Write {
        /// The path that failed.
        path: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// The file is not valid TOML, or a value has the wrong type.
    ///
    /// `detail` is `toml`'s own `Display`, which already includes the line,
    /// column, a caret, and the source line.
    Parse {
        /// The path that failed.
        path: PathBuf,
        /// The parser's message, verbatim.
        detail: String,
    },
    /// A valid TOML document with a value outside its allowed range.
    Invalid {
        /// The path that failed.
        path: PathBuf,
        /// Dotted key, e.g. `search.half_life_days`.
        key: String,
        /// The value as written, rendered for a human.
        value: String,
        /// What is wrong and what to do.
        reason: String,
    },
    /// The in-memory config could not be rendered back to TOML.
    Serialise {
        /// The path it was headed for.
        path: PathBuf,
        /// The serialiser's message.
        detail: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "could not read {}: {source}", path.display())
            }
            ConfigError::Write { path, source } => {
                write!(f, "could not write {}: {source}", path.display())
            }
            ConfigError::Parse { path, detail } => {
                // `detail` is multi-line and already points at a column.
                write!(f, "could not parse {}:\n{detail}", path.display())
            }
            ConfigError::Invalid {
                path,
                key,
                value,
                reason,
            } => write!(
                f,
                "invalid value in {}:\n  {key} = {value}\n  {reason}",
                path.display()
            ),
            ConfigError::Serialise { path, detail } => {
                write!(
                    f,
                    "could not serialise config for {}: {detail}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Read { source, .. } | ConfigError::Write { source, .. } => Some(source),
            ConfigError::Parse { .. }
            | ConfigError::Invalid { .. }
            | ConfigError::Serialise { .. } => None,
        }
    }
}

impl ConfigError {
    /// The file the failure relates to.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            ConfigError::Read { path, .. }
            | ConfigError::Write { path, .. }
            | ConfigError::Parse { path, .. }
            | ConfigError::Invalid { path, .. }
            | ConfigError::Serialise { path, .. } => path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::load_from_str(text, "test.toml")
    }

    fn key_of(error: &ConfigError) -> &str {
        match error {
            ConfigError::Invalid { key, .. } => key,
            other => panic!("expected an Invalid error, got {other}"),
        }
    }

    #[test]
    fn defaults_are_valid_and_parse_as_an_empty_document() {
        // A completely empty file must produce the defaults, not an error. That
        // is what makes first run work.
        let config = parse("").expect("empty config is valid");
        assert_eq!(config, Config::default());
        Config::default()
            .validate("default")
            .expect("defaults validate");
    }

    #[test]
    fn a_partial_config_only_overrides_what_it_mentions() {
        let config = parse("[general]\nmax_results = 7\n").expect("should parse");
        assert_eq!(config.general.max_results, 7);
        // Everything else is untouched.
        assert_eq!(config.general.theme, ThemePreference::System);
        assert_eq!(config.search, Search::default());
        assert_eq!(config.files, Files::default());
    }

    #[test]
    fn the_example_file_is_valid_and_round_trips() {
        // Guards against the documented example drifting from the parser.
        let text = Config::example_toml();
        let config = parse(&text).expect("the example config must be valid");
        assert_eq!(config, Config::default());
        let rendered = toml::to_string_pretty(&config).expect("should render");
        assert_eq!(
            parse(&rendered).expect("round trip"),
            config,
            "a saved config must reload identically"
        );
    }

    #[test]
    fn an_unknown_key_is_an_error_rather_than_silently_ignored() {
        // The whole point of `deny_unknown_fields`.
        let error = parse("[general]\nthme = \"dark\"\n").expect_err("typo must fail");
        assert!(matches!(error, ConfigError::Parse { .. }));
        let message = error.to_string();
        assert!(message.contains("thme"), "{message}");
        assert!(
            message.contains("test.toml"),
            "the path must be named: {message}"
        );

        let error = parse("[totally_not_a_section]\nx = 1\n").expect_err("must fail");
        assert!(error.to_string().contains("totally_not_a_section"));
    }

    #[test]
    fn a_wrong_type_is_an_error_with_a_position() {
        let error = parse("[general]\nmax_results = \"many\"\n").expect_err("must fail");
        let message = error.to_string();
        assert!(message.contains("test.toml"), "{message}");
        assert!(
            message.contains("line 2") || message.contains("line 1"),
            "the message should point at a line: {message}"
        );
    }

    #[test]
    fn every_out_of_range_field_is_rejected_by_name() {
        let cases: [(&str, &str); 8] = [
            ("[general]\nmax_results = 0\n", "general.max_results"),
            ("[general]\nmax_results = 100000\n", "general.max_results"),
            ("[search]\nmin_score = 1.5\n", "search.min_score"),
            ("[search]\nmin_score = -0.1\n", "search.min_score"),
            ("[search]\nmatch_weight = 2.0\n", "search.match_weight"),
            ("[search]\nprovider_share = -1.0\n", "search.provider_share"),
            ("[search]\nhalf_life_days = 0\n", "search.half_life_days"),
            (
                "[search]\nmin_fuzzy_length = 1000\n",
                "search.min_fuzzy_length",
            ),
        ];
        for (text, expected_key) in cases {
            let error = parse(text).expect_err("must fail");
            assert_eq!(key_of(&error), expected_key, "for {text:?}");
            let message = error.to_string();
            assert!(message.contains(expected_key), "{message}");
            assert!(message.contains("invalid value"), "{message}");
        }
    }

    #[test]
    fn nan_is_rejected_rather_than_sanitised_silently() {
        // `inf` parses as a TOML float; the point is that an unbounded value
        // does not reach the blend.
        let error = parse("[search]\nmin_score = inf\n").expect_err("must fail");
        assert_eq!(key_of(&error), "search.min_score");
    }

    #[test]
    fn source_weight_overrides_are_range_checked_and_optional() {
        let config = parse("[sources]\nclipboard = 0.0\n").expect("0.0 is legal");
        assert_eq!(config.sources.clipboard, Some(0.0));
        assert_eq!(config.sources.file, None, "None means 'use the default'");

        let table = config.sources.to_weight_table();
        assert_eq!(
            table.get(Source::Clipboard),
            0.0,
            "an explicit 0.0 must survive, which is why the fields are Option"
        );
        assert_eq!(
            table.get(Source::File),
            WeightTable::DEFAULT.get(Source::File)
        );

        let error = parse("[sources]\nclipboard = 1.5\n").expect_err("must fail");
        assert_eq!(key_of(&error), "sources.clipboard");
    }

    #[test]
    fn a_full_source_override_table_maps_every_key() {
        let text = "[sources]\n\
                    application = 0.11\nfile = 0.22\nfolder = 0.33\ncommand = 0.44\n\
                    web_search = 0.55\ncalculator = 0.66\nclipboard = 0.77\nunknown = 0.88\n";
        let table = parse(text).expect("should parse").sources.to_weight_table();
        for (source, expected) in [
            (Source::Application, 0.11),
            (Source::File, 0.22),
            (Source::Folder, 0.33),
            (Source::Command, 0.44),
            (Source::WebSearch, 0.55),
            (Source::Calculator, 0.66),
            (Source::Clipboard, 0.77),
            (Source::Unknown, 0.88),
        ] {
            assert!(
                (table.get(source) - expected).abs() < 1e-12,
                "{source:?} did not take its override: {} != {expected}",
                table.get(source)
            );
        }
    }

    #[test]
    fn enabling_files_without_roots_is_an_error_with_a_fix() {
        let error = parse("[files]\nenabled = true\n").expect_err("must fail");
        assert_eq!(key_of(&error), "files.roots");
        let message = error.to_string();
        assert!(
            message.contains("files.enabled = false"),
            "the fix: {message}"
        );

        // And with a root it is fine.
        let config = parse("[files]\nenabled = true\nroots = [\"C:\\\\Users\"]\n").expect("ok");
        assert_eq!(config.files.roots.len(), 1);
    }

    #[test]
    fn empty_roots_and_a_zero_depth_are_rejected() {
        let error = parse("[files]\nroots = [\"\"]\n").expect_err("must fail");
        assert_eq!(key_of(&error), "files.roots[0]");
        let error = parse("[files]\nmax_depth = 0\n").expect_err("must fail");
        assert_eq!(key_of(&error), "files.max_depth");
        let error = parse("[files]\nmax_results = 0\n").expect_err("must fail");
        assert_eq!(key_of(&error), "files.max_results");
    }

    #[test]
    fn aliases_must_be_named_programmed_and_unique() {
        let text =
            "[[commands.aliases]]\nname = \"gh\"\nprogram = \"gh\"\nargs = [\"pr\", \"status\"]\n";
        let config = parse(text).expect("should parse");
        assert_eq!(config.commands.aliases.len(), 1);
        assert_eq!(config.commands.aliases[0].args, ["pr", "status"]);

        let error = parse("[[commands.aliases]]\nname = \"  \"\nprogram = \"gh\"\n")
            .expect_err("must fail");
        assert_eq!(key_of(&error), "commands.aliases[0].name");

        let error =
            parse("[[commands.aliases]]\nname = \"gh\"\nprogram = \"\"\n").expect_err("must fail");
        assert_eq!(key_of(&error), "commands.aliases[0].program");

        let duplicate = "[[commands.aliases]]\nname = \"gh\"\nprogram = \"gh\"\n\
                            [[commands.aliases]]\nname = \"GH\"\nprogram = \"gh2\"\n";
        let error = parse(duplicate).expect_err("must fail");
        assert_eq!(key_of(&error), "commands.aliases[1].name");
    }

    #[test]
    fn an_alias_may_omit_args() {
        let config =
            parse("[[commands.aliases]]\nname = \"ls\"\nprogram = \"ls\"\n").expect("should parse");
        assert!(config.commands.aliases[0].args.is_empty());
    }

    #[test]
    fn ranking_policy_reflects_the_config() {
        let text = "[search]\nmatch_weight = 0.5\nprovider_share = 1.0\n\
                    half_life_days = 1\nmin_fuzzy_length = 0\nmin_score = 0.25\n\
                    [sources]\nfile = 0.1\n";
        let config = parse(text).expect("should parse");
        let policy = config.ranking_policy();

        assert!((policy.match_weight() - 0.5).abs() < 1e-12);
        assert!((policy.provider_share() - 1.0).abs() < 1e-12);
        assert_eq!(policy.half_life().as_secs(), 86_400);
        assert!((policy.min_score() - 0.25).abs() < 1e-12);
        assert!((policy.sources().get(Source::File) - 0.1).abs() < 1e-12);
        // And the min-fuzzy gate actually took effect.
        assert_eq!(
            policy.match_quality("nte", "notepad", None).kind,
            crate::MatchKind::None
        );
    }

    #[test]
    fn the_default_config_produces_the_default_policy() {
        // Guards against the two defaults drifting apart, which is the most
        // likely way this section rots.
        let policy = Config::default().ranking_policy();
        assert_eq!(policy.match_weight(), RankingPolicy::DEFAULT.match_weight());
        assert_eq!(policy.half_life(), RankingPolicy::DEFAULT.half_life());
        assert_eq!(policy.min_score(), RankingPolicy::DEFAULT.min_score());
        assert_eq!(policy.sources(), RankingPolicy::DEFAULT.sources());
    }

    #[test]
    fn theme_spellings_round_trip_and_reject_the_rest() {
        for (spelling, expected) in [
            ("system", ThemePreference::System),
            ("light", ThemePreference::Light),
            ("dark", ThemePreference::Dark),
            ("DARK", ThemePreference::Dark),
            ("  light  ", ThemePreference::Light),
            ("auto", ThemePreference::System),
        ] {
            let config = parse(&format!("[general]\ntheme = \"{spelling}\"\n")).expect("ok");
            assert_eq!(config.general.theme, expected, "{spelling:?}");
            assert_eq!(expected.to_string(), expected.as_str());
        }

        let error = parse("[general]\ntheme = \"blue\"\n").expect_err("must fail");
        let message = error.to_string();
        assert!(message.contains("blue"), "{message}");
        // The parser's own message names the accepted values, which is the
        // actionable part.
        for accepted in ThemePreference::VALUES {
            assert!(
                message.contains(accepted),
                "must list {accepted}: {message}"
            );
        }
    }

    #[test]
    fn a_malformed_hotkey_is_rejected_with_a_message_that_shows_the_fix() {
        let error = parse("[general]\nhotkey = \"Ctrl+\"\n").expect_err("must fail");
        let message = error.to_string();
        assert!(message.contains("hotkey"), "{message}");
        assert!(
            message.contains(HotkeySpec::DEFAULT_SPEC),
            "must show a working spec: {message}"
        );
    }

    #[test]
    fn load_reports_a_missing_file_as_defaults() {
        // A path that definitely does not exist, and is not even on a drive this
        // machine has. No tempfile dependency and no touching the real profile.
        let paths = ConfigPaths::at("Z:/orca-config-test-does-not-exist");
        assert!(!paths.config_file().exists());
        let config = Config::load(&paths).expect("a missing file is a first run");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn load_reports_an_unreadable_file_as_an_error() {
        // A *directory* where the config file should be. The read fails with
        // something other than NotFound, which is the branch that must not be
        // mistaken for a first run. Under the system temp dir, not `%APPDATA%`.
        let base = std::env::temp_dir().join(format!("orca-core-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = ConfigPaths::at(&base);
        std::fs::create_dir_all(paths.config_file()).expect("could not prepare fixture");

        let error = Config::load(&paths).expect_err("a directory is not a config file");
        assert!(matches!(error, ConfigError::Read { .. }), "{error}");
        assert_eq!(error.path(), paths.config_file());
        assert!(error.to_string().contains("config.toml"), "{error}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn error_display_names_the_file_key_value_and_fix() {
        let error = parse("[search]\nhalf_life_days = 0\n").expect_err("must fail");
        let message = error.to_string();
        assert!(message.contains("test.toml"), "{message}");
        assert!(message.contains("search.half_life_days"), "{message}");
        assert!(message.contains("= 0"), "{message}");
        assert!(message.contains("between 1 and"), "{message}");
    }
}
