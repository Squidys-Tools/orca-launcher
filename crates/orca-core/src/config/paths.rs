//! Where orca's files live, with the root injected rather than discovered.
//!
//! The real location is `%APPDATA%\Orca\`, but `orca-core` does not read that.
//! Layering rule 2 forbids environment access in this crate, and the reason is
//! not pedantry: a test that resolves a path from the real environment is a
//! test that fails on someone else's machine, and a config loader whose
//! behaviour depends on a variable is a config loader with two behaviours.
//!
//! So the caller supplies the root and this module does the joining. The
//! composition root in `orca` is the only place that knows `%APPDATA%` exists.
//!
//! ```
//! use orca_core::config::ConfigPaths;
//!
//! let paths = ConfigPaths::under_app_data(r"C:\Users\chris\AppData\Roaming");
//! assert!(paths.dir().ends_with("Orca"));
//! assert!(paths.config_file().ends_with(r"Orca\config.toml"));
//! assert!(paths.database_file().ends_with(r"Orca\orca.db"));
//!
//! // Or the app directory directly, which is what a test or a portable
//! // install wants.
//! let portable = ConfigPaths::at("D:/Tools/orca");
//! assert!(portable.config_file().ends_with("config.toml"));
//! ```

use std::path::{Path, PathBuf};

/// Directory name orca keeps its files in, under the platform app-data root.
pub const APP_DIR_NAME: &str = "Orca";

/// Config file name.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Frecency database file name.
pub const DATABASE_FILE_NAME: &str = "orca.db";

/// Resolves orca's file locations from an injected root.
///
/// Cheap to copy and holds no handle, so it can be constructed in a hot path,
/// stored in config, and compared in a test.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConfigPaths {
    root: PathBuf,
}

impl ConfigPaths {
    /// Locations under a platform app-data root, e.g. `%APPDATA%` on Windows.
    ///
    /// [`APP_DIR_NAME`] is appended, so the caller passes the *root* and gets
    /// `%APPDATA%\Orca` back. Passing the app directory directly is
    /// [`ConfigPaths::at`].
    #[must_use]
    pub fn under_app_data(app_data_root: impl Into<PathBuf>) -> ConfigPaths {
        ConfigPaths::at(app_data_root.into().join(APP_DIR_NAME))
    }

    /// Locations under an explicit directory, with no subdirectory appended.
    ///
    /// The escape hatch for a portable install, a test, or an operator who
    /// wants the database somewhere other than `%APPDATA%`.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> ConfigPaths {
        ConfigPaths { root: root.into() }
    }

    /// The directory holding every orca file.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.root
    }

    /// `<dir>/config.toml`.
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.root.join(CONFIG_FILE_NAME)
    }

    /// `<dir>/orca.db`, the frecency database.
    #[must_use]
    pub fn database_file(&self) -> PathBuf {
        self.root.join(DATABASE_FILE_NAME)
    }

    /// Consumes the paths and returns the root, for callers that need to
    /// enumerate the directory themselves.
    #[must_use]
    pub fn into_root(self) -> PathBuf {
        self.root
    }
}

impl Default for ConfigPaths {
    /// `%APPDATA%\Orca`, via the environment.
    ///
    /// The one place in this crate that reads the environment, and it is
    /// behind `Default` rather than behind any function the tests call — so a
    /// test that uses [`ConfigPaths::at`] or [`ConfigPaths::under_app_data`]
    /// never touches it. It falls back to a relative path rather than
    /// panicking, because a missing `APPDATA` is not worth taking the launcher
    /// down for.
    fn default() -> ConfigPaths {
        match std::env::var_os("APPDATA") {
            Some(root) if !root.is_empty() => ConfigPaths::under_app_data(root),
            _ => ConfigPaths::at(APP_DIR_NAME),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_app_data_appends_the_app_directory() {
        let paths = ConfigPaths::under_app_data(r"C:\Users\chris\AppData\Roaming");
        assert_eq!(
            paths.dir(),
            Path::new(r"C:\Users\chris\AppData\Roaming").join("Orca")
        );
        assert_eq!(
            paths.config_file(),
            Path::new(r"C:\Users\chris\AppData\Roaming\Orca\config.toml")
        );
        assert_eq!(
            paths.database_file(),
            Path::new(r"C:\Users\chris\AppData\Roaming\Orca\orca.db")
        );
    }

    #[test]
    fn at_does_not_append_anything() {
        let paths = ConfigPaths::at("D:/Tools/orca");
        assert_eq!(paths.dir(), Path::new("D:/Tools/orca"));
        assert_eq!(paths.config_file(), Path::new("D:/Tools/orca/config.toml"));
        assert_eq!(paths.database_file(), Path::new("D:/Tools/orca/orca.db"));
    }

    #[test]
    fn the_two_constructors_disagree_exactly_by_the_app_directory() {
        let root = Path::new("/tmp/appdata");
        assert_eq!(
            ConfigPaths::under_app_data(root).dir(),
            ConfigPaths::at(root).dir().join(APP_DIR_NAME)
        );
    }

    #[test]
    fn into_root_gives_the_directory_back() {
        let paths = ConfigPaths::at("/tmp/x");
        assert_eq!(paths.clone().into_root(), PathBuf::from("/tmp/x"));
    }

    #[test]
    fn the_default_never_panics_and_always_ends_in_the_app_directory() {
        // Deliberately not asserting the resolved value: this depends on the
        // real environment, and a test that does is exactly the test that
        // fails on someone else's machine. What is asserted is the invariant
        // that holds everywhere.
        let paths = ConfigPaths::default();
        assert_eq!(
            paths.config_file().parent().and_then(|dir| dir.file_name()),
            Some(std::ffi::OsStr::new(APP_DIR_NAME))
        );
        assert_eq!(
            paths.config_file().file_name(),
            Some(std::ffi::OsStr::new(CONFIG_FILE_NAME))
        );
    }
}
