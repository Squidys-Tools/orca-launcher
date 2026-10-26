//! Which icon a result row shows, and remembering the ones already loaded.
//!
//! Loading an icon is a shell call (`orca-win`) and painting one is a GPUI image
//! (`orca`). What sits between them is pure, so it lives here:
//!
//! * [`icon_key`] decides what an item's icon *is*. Most files share their
//!   icon with every other file of the same extension, so a list of forty
//!   `.txt` files is one icon load, not forty. Only the kinds whose icon is
//!   embedded in the file itself are keyed by path.
//! * [`IconCache`] records what has been asked for, so a keystroke that keeps
//!   the same rows on screen never asks again — including for icons that
//!   failed to load, which would otherwise be retried on every keystroke.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::{LaunchTarget, ResultItem, Source};

/// What an icon is looked up by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IconKey {
    /// The icon stored in, or pointed to by, this particular file.
    Path(PathBuf),
    /// The icon the shell associates with a lowercase extension, without its
    /// dot. Empty for a file with no extension.
    Extension(String),
    /// The generic folder icon.
    Folder,
}

/// Extensions whose icon belongs to the individual file rather than to its
/// type: every `.exe` and every shortcut has its own.
const PER_FILE_EXTENSIONS: [&str; 8] =
    ["exe", "lnk", "ico", "url", "appref-ms", "msc", "cpl", "scr"];

/// The icon an item should show, or `None` for an item with nothing behind it
/// to take an icon from: a command resolved through `PATH`, a URI, a computed
/// answer.
#[must_use]
pub fn icon_key(item: &ResultItem) -> Option<IconKey> {
    let LaunchTarget::Executable { path, .. } = &item.target else {
        return None;
    };
    match item.source {
        Source::Application => Some(IconKey::Path(path.clone())),
        Source::Folder => Some(IconKey::Folder),
        Source::File => Some(file_key(path)),
        _ => None,
    }
}

fn file_key(path: &Path) -> IconKey {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if PER_FILE_EXTENSIONS.contains(&extension.as_str()) {
        IconKey::Path(path.to_path_buf())
    } else {
        IconKey::Extension(extension)
    }
}

/// Icons by key, generic over the image type so it can be tested without a
/// renderer.
///
/// A key is in one of three states: never asked for, asked for and still
/// loading, or settled with an image or with `None` for a load that failed.
/// Only the first is ever handed out by [`IconCache::claim`], so one key is
/// loaded at most once however many searches show it.
#[derive(Debug)]
pub struct IconCache<T> {
    slots: HashMap<IconKey, Option<T>>,
}

impl<T> Default for IconCache<T> {
    fn default() -> Self {
        IconCache {
            slots: HashMap::new(),
        }
    }
}

impl<T: Clone> IconCache<T> {
    /// The loaded icon for `key`, if it has finished loading and succeeded.
    #[must_use]
    pub fn get(&self, key: &IconKey) -> Option<T> {
        self.slots.get(key).cloned().flatten()
    }

    /// Marks every key not yet asked for as loading, and returns those keys in
    /// first-seen order without duplicates.
    ///
    /// The caller is now responsible for loading each returned key and passing
    /// the result to [`IconCache::fill`].
    pub fn claim(&mut self, keys: impl IntoIterator<Item = IconKey>) -> Vec<IconKey> {
        let mut claimed = Vec::new();
        for key in keys {
            if !self.slots.contains_key(&key) {
                self.slots.insert(key.clone(), None);
                claimed.push(key);
            }
        }
        claimed
    }

    /// Records the result of loading a claimed key. `None` records a failure,
    /// which is final: the key is not handed out by `claim` again.
    pub fn fill(&mut self, key: IconKey, icon: Option<T>) {
        self.slots.insert(key, icon);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(source: Source, path: &str) -> ResultItem {
        ResultItem::new(
            format!("test:{path}"),
            path,
            source,
            LaunchTarget::Executable {
                path: PathBuf::from(path),
                args: Vec::new(),
            },
        )
    }

    fn extension(extension: &str) -> Option<IconKey> {
        Some(IconKey::Extension(extension.to_owned()))
    }

    #[test]
    fn an_application_is_keyed_by_its_own_path() {
        assert_eq!(
            icon_key(&item(Source::Application, r"C:\Apps\word.exe")),
            Some(IconKey::Path(PathBuf::from(r"C:\Apps\word.exe")))
        );
    }

    #[test]
    fn every_folder_shares_one_icon() {
        assert_eq!(
            icon_key(&item(Source::Folder, r"C:\Users\me\Downloads")),
            Some(IconKey::Folder)
        );
    }

    #[test]
    fn ordinary_files_share_an_icon_per_lowercase_extension() {
        assert_eq!(
            icon_key(&item(Source::File, r"C:\a\notes.txt")),
            extension("txt")
        );
        assert_eq!(
            icon_key(&item(Source::File, r"C:\b\README.TXT")),
            extension("txt")
        );
        assert_eq!(
            icon_key(&item(Source::File, r"C:\b\Makefile")),
            extension("")
        );
    }

    #[test]
    fn files_that_carry_their_own_icon_are_keyed_by_path() {
        for path in [
            r"C:\a\setup.exe",
            r"C:\a\Tool.LNK",
            r"C:\a\site.url",
            r"C:\a\app.ico",
        ] {
            assert_eq!(
                icon_key(&item(Source::File, path)),
                Some(IconKey::Path(PathBuf::from(path))),
                "{path}"
            );
        }
    }

    #[test]
    fn results_with_nothing_on_disk_have_no_icon() {
        let command = ResultItem::new(
            "cmd:git",
            "git",
            Source::Command,
            LaunchTarget::Command {
                program: "git".into(),
                args: Vec::new(),
            },
        );
        let answer = ResultItem::new(
            "calc:14",
            "14",
            Source::Calculator,
            LaunchTarget::CopyToClipboard("14".into()),
        );
        let link = ResultItem::new(
            "web:x",
            "x",
            Source::WebSearch,
            LaunchTarget::Uri("https://example.com".into()),
        );
        assert_eq!(icon_key(&command), None);
        assert_eq!(icon_key(&answer), None);
        assert_eq!(icon_key(&link), None);
        // A command alias that happens to name an executable by path is still a
        // command, not a file.
        assert_eq!(icon_key(&item(Source::Command, r"C:\a\b.exe")), None);
    }

    #[test]
    fn a_key_is_claimed_once_and_duplicates_collapse() {
        let mut cache = IconCache::<u8>::default();
        let claimed = cache.claim([IconKey::Folder, extension("txt").unwrap(), IconKey::Folder]);
        assert_eq!(claimed, vec![IconKey::Folder, extension("txt").unwrap()]);
        assert!(cache.claim([IconKey::Folder]).is_empty(), "still loading");
    }

    #[test]
    fn nothing_is_returned_until_a_load_succeeds() {
        let mut cache = IconCache::default();
        cache.claim([IconKey::Folder]);
        assert_eq!(cache.get(&IconKey::Folder), None, "loading");
        cache.fill(IconKey::Folder, Some(7));
        assert_eq!(cache.get(&IconKey::Folder), Some(7));
    }

    #[test]
    fn a_failed_load_is_not_retried() {
        let mut cache = IconCache::<u8>::default();
        cache.claim([IconKey::Folder]);
        cache.fill(IconKey::Folder, None);
        assert_eq!(cache.get(&IconKey::Folder), None);
        assert!(cache.claim([IconKey::Folder]).is_empty());
    }
}
