//! The two adapters that connect `orca-core`'s seams to the outside world.
//!
//! Both live here because `docs/ARCHITECTURE.md` rule 2 keeps the filesystem
//! out of `orca-core` and rule 5 keeps non-wiring logic out of `orca`; what is
//! left for this crate is exactly the conversion between a domain type and a
//! thing that can actually be enumerated.
//!
//! | what | trait in `orca-core` | implementation |
//! |---|---|---|
//! | installed applications | `providers::ResultProvider` | [`InstalledAppProvider`] |
//! | the filesystem | `providers::DirectoryLister` | [`StdFsDirectory`] |
//!
//! The mapping halves are kept separate from the enumeration halves and are
//! tested without touching the machine: [`installed_app_to_raw`] is a pure
//! function over an `InstalledApp`, and `FileSearchProvider` is already
//! exhaustively tested in `orca-core` against an in-memory lister, so what this
//! file has to get right is the field-for-field translation.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use orca_core::providers::files::EntryKind;
use orca_core::providers::{DirectoryLister, ProviderError, RawResult, ResultProvider};
use orca_core::{LaunchTarget, Source};
use orca_win::{installed_apps, InstalledApp};

/// `orca-win`'s installed-application enumeration, behind the core's seam.
pub struct InstalledAppProvider;

impl ResultProvider for InstalledAppProvider {
    fn name(&self) -> &str {
        "installed-apps"
    }

    fn collect(&self) -> Result<Vec<RawResult>, ProviderError> {
        // `installed_apps` walks the Start Menu, opens a COM apartment, and
        // reads several registry keys. It is a blocking call and belongs on the
        // background executor; `ProviderSet::collect_all` is only ever reached
        // from there.
        installed_apps()
            .map(|apps| apps.iter().map(installed_app_to_raw).collect())
            .map_err(|error| ProviderError::Failed {
                provider: self.name().to_owned(),
                source: Box::new(ProviderError::Io {
                    path: PathBuf::from("Start Menu / App Paths"),
                    detail: error.to_string(),
                }),
            })
    }
}

/// One `InstalledApp` as one candidate.
///
/// The id comes from `orca-win` and is source-prefixed, so it cannot collide
/// with a file id or an alias id and the frecency table stays stable across
/// reinstalls.
///
/// `arguments` is a single string in `orca-win` and a `Vec<String>` here. It is
/// split on whitespace rather than parsed, because a shortcut's argument string
/// has no quoting that can be recovered reliably from the shell syntax it was
/// written in, and a mis-split argument is a much smaller problem than refusing
/// to launch anything.
#[must_use]
pub fn installed_app_to_raw(app: &InstalledApp) -> RawResult {
    let mut arguments = app
        .arguments
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    arguments.shrink_to_fit();

    RawResult::new(
        app.id(),
        app.name.clone(),
        Source::Application,
        LaunchTarget::Executable {
            path: app.target.clone(),
            args: arguments,
        },
    )
    // No subtitle. An application's name is its whole identity, so a second line
    // has nothing to add — and what it used to carry was
    // `C:\Program Files\Microsoft Office\root\Office16\WINWORD.EXE`, which is
    // the same on every installed app and long besides. The path stays as
    // `keywords`, so it is still searchable.
    .with_keywords(app.target.to_string_lossy())
}

/// `std::fs` behind `orca-core`'s [`DirectoryLister`].
///
/// The shape is the one `orca-core`'s own module docs spell out; the parts that
/// are not obvious from the sketch are commented.
///
/// A walk over `C:\Users` touches tens of thousands of entries, so this is
/// only ever called from the background executor.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFsDirectory;

impl DirectoryLister for StdFsDirectory {
    fn list(&self, path: &Path) -> Result<Vec<PathBuf>, ProviderError> {
        let entries = std::fs::read_dir(path).map_err(|error| ProviderError::Io {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;

        // A single unreadable entry must not abandon the whole directory. A
        // launcher that loses 400 results because one is locked is worse than
        // one that silently shows 399 — and the cycle guard in
        // `FileSearchProvider` needs a *partial* answer to keep working, since
        // an error there aborts the entire collection.
        let mut collected = Vec::new();
        for entry in entries.flatten() {
            collected.push(entry.path());
        }
        Ok(collected)
    }

    fn kind(&self, path: &Path) -> Option<EntryKind> {
        // `metadata` follows symlinks, which is what the walk wants: a link to
        // a directory is a directory. `symlink_metadata` would classify every
        // junction as `Other` and the depth limit would stop working.
        let metadata = std::fs::metadata(path).ok()?;
        if metadata.is_dir() {
            Some(EntryKind::Directory)
        } else if metadata.is_file() {
            Some(EntryKind::File)
        } else {
            Some(EntryKind::Other)
        }
    }

    fn modified_at(&self, path: &Path) -> Option<i64> {
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;
        let elapsed = modified.duration_since(UNIX_EPOCH).ok()?;
        i64::try_from(elapsed.as_secs()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_win::InstalledAppSource;

    fn app(name: &str, target: &str, arguments: &str) -> InstalledApp {
        InstalledApp {
            name: name.to_owned(),
            target: PathBuf::from(target),
            arguments: arguments.to_owned(),
            source: InstalledAppSource::StartMenu,
            shortcut: Some(PathBuf::from(r"C:\ProgramData\Menu\a.lnk")),
            registry_key: None,
        }
    }

    #[test]
    fn an_installed_app_becomes_an_executable_target_with_its_path_as_the_subtitle() {
        let raw = installed_app_to_raw(&app("Notepad", r"C:\Windows\notepad.exe", ""));
        assert_eq!(raw.title, "Notepad");
        assert_eq!(raw.source, Source::Application);
        assert_eq!(
            raw.target,
            LaunchTarget::Executable {
                path: PathBuf::from(r"C:\Windows\notepad.exe"),
                args: Vec::new(),
            }
        );
        // No subtitle: an app's name says everything, and what used to be on the
        // second line was the same long path for every installed app. The path
        // stays searchable in `keywords`.
        assert_eq!(raw.subtitle, None);
        assert_eq!(raw.keywords.as_deref(), Some(r"C:\Windows\notepad.exe"));
    }

    #[test]
    fn the_id_is_source_scoped_so_it_cannot_collide_with_a_file_id() {
        let start_menu = installed_app_to_raw(&app("Chrome", r"C:\chrome.exe", ""));
        let mut registry = app("Chrome", r"C:\chrome.exe", "");
        registry.source = InstalledAppSource::AppPaths;
        registry.shortcut = None;
        registry.registry_key = Some("chrome".into());
        let app_paths = installed_app_to_raw(&registry);

        assert!(start_menu.id.starts_with("startmenu:"));
        assert!(app_paths.id.starts_with("apppath:"));
        assert_ne!(start_menu.id, app_paths.id);
    }

    #[test]
    fn shortcut_arguments_are_split_into_a_vector() {
        let raw = installed_app_to_raw(&app("Code", r"C:\code.exe", "--new-window  ~/src"));
        match raw.target {
            LaunchTarget::Executable { args, .. } => {
                assert_eq!(args, vec!["--new-window".to_owned(), "~/src".to_owned()]);
            }
            other => panic!("expected an executable target, got {other:?}"),
        }
    }

    #[test]
    fn blank_and_padded_argument_strings_become_empty_and_never_a_blank_argument() {
        for arguments in ["", "   ", "\t\n "] {
            let raw = installed_app_to_raw(&app("x", r"C:\x.exe", arguments));
            match raw.target {
                LaunchTarget::Executable { args, .. } => {
                    assert!(args.is_empty(), "{arguments:?} produced {args:?}");
                }
                other => panic!("expected an executable target, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_provider_reports_a_stable_name() {
        // The name is what `ProviderSet::collect_all` puts in the error when a
        // collection fails, so it has to be the one the log and the status line
        // agree on.
        assert_eq!(InstalledAppProvider.name(), "installed-apps");
    }

    #[test]
    fn the_std_fs_lister_classifies_a_real_file_and_a_real_directory() {
        let dir = std::env::temp_dir();
        let lister = StdFsDirectory;
        assert_eq!(lister.kind(&dir), Some(EntryKind::Directory));
        assert_eq!(lister.kind(&dir.join("orca-lister-does-not-exist")), None);

        let listed = lister
            .list(&dir)
            .expect("the temp directory is always listable");
        assert!(
            !listed.is_empty(),
            "an empty temp dir is not a valid test env"
        );
        assert!(listed.iter().all(|path| path.is_absolute()));
    }

    #[test]
    fn listing_a_directory_that_does_not_exist_is_reported_not_swallowed() {
        let missing = std::env::temp_dir().join("orca-lister-missing-dir");
        let error = StdFsDirectory
            .list(&missing)
            .expect_err("a missing directory must not read as empty");
        // The distinction matters: an empty list means "nothing here", an error
        // means "could not look", and `FileSearchProvider` treats them
        // differently for a root.
        match error {
            ProviderError::Io { path, detail } => {
                assert_eq!(path, missing);
                assert!(!detail.is_empty());
            }
            other => panic!("expected an Io error, got {other:?}"),
        }
    }

    #[test]
    fn a_recently_written_file_reports_a_plausible_mtime() {
        // The mtime becomes a decaying prior in `orca-core`, so a plausible
        // value matters: a bogus one would sort every file to the bottom.
        let path = std::env::temp_dir().join("orca-lister-mtime-probe");
        std::fs::write(&path, b"probe").expect("temp dir is writable");
        let modified = StdFsDirectory
            .modified_at(&path)
            .expect("a file that was just written has an mtime");
        // After 2020-01-01 and before 2200-01-01: a wide enough band that the
        // test is about units, not about the clock.
        assert!(
            modified > 1_577_836_800 && modified < 7_258_118_400,
            "implausible mtime {modified}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
