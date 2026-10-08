//! Installed applications: Start Menu shortcuts and `App Paths` registry
//! entries.
//!
//! # Two sources, because neither is complete
//!
//! * **Start Menu `.lnk` files.** What a user installs puts a shortcut here, and
//!   the shortcut is the only place the *display name* lives: the executable's
//!   own metadata is a ProductName resource that installers frequently leave
//!   as the file name, or set to something the user never sees.
//! * **`App Paths`.** The registry key Windows itself consults when something
//!   runs a program by bare name. It is the only source that knows about
//!   programs with *no* shortcut at all — command-line tools, background
//!   services, SDK bits.
//!
//! Unioning them and de-duplicating by resolved target gives a catalog that
//! covers both what was installed visibly and what is merely runnable.
//!
//! # Deliberately not an icon loader
//!
//! Extracting icons means opening every executable, which is slow enough to
//! matter on a `BackgroundExecutor` and buys a `ResultItem` the ranking layer
//! does not need. Paths and names are what the index wants.
//!
//! # Mapping onto `orca-core`
//!
//! Each [`InstalledApp`] becomes one `ResultItem`:
//!
//! ```text
//! ResultItem::new(app.id(), app.name.clone(), Source::Application,
//!                LaunchTarget::Command { program: app.target,
//!                                        args: app.arguments.split_whitespace() })
//!   .with_subtitle(app.target.display().to_string())
//! ```
//!
//! `id()` is `startmenu:<lowercased shortcut path>` or `apppath:<lowercased
//! registry key>`, so it is stable across runs and safe to use as a key in the
//! index.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    KEY_READ,
};
use windows::Win32::UI::Shell::{
    FOLDERID_CommonPrograms, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, KF_FLAG_DEFAULT,
};

use crate::single_instance::last_error_code;
use crate::wide::{path_from_wide, string_from_wide, wide_nul, wide_path_nul};

/// `CLSID_ShellLink`, which the `windows` crate does not export.
///
/// Spelled out rather than pulled from a crate that will: it is a five-line
/// literal, and it is the interface this module cannot work without.
const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x00021401_0000_0000_C000_000000000046);

/// Where an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstalledAppSource {
    /// A Start Menu `.lnk` shortcut.
    StartMenu,
    /// An `App Paths` registry key.
    AppPaths,
}

impl fmt::Display for InstalledAppSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstalledAppSource::StartMenu => write!(f, "start menu"),
            InstalledAppSource::AppPaths => write!(f, "App Paths"),
        }
    }
}

/// One runnable program found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    /// A display name: the shortcut's file name, or the registry key name
    /// without its `.exe`.
    pub name: String,
    /// The resolved executable.
    pub target: PathBuf,
    /// Arguments carried by the shortcut or the App Paths value. Empty when
    /// there are none.
    pub arguments: String,
    /// Which source produced this entry.
    pub source: InstalledAppSource,
    /// The `.lnk` this came from, for Start Menu entries.
    pub shortcut: Option<PathBuf>,
    /// The registry key name, for App Paths entries.
    pub registry_key: Option<String>,
}

impl InstalledApp {
    /// A stable identifier for the index: source-prefixed and lowercased.
    ///
    /// The shortcut path and the registry key name are both facts about the
    /// machine rather than about the program, so this stays stable when an
    /// application is reinstalled at a different version.
    pub fn id(&self) -> String {
        match self.source {
            InstalledAppSource::StartMenu => format!(
                "startmenu:{}",
                self.shortcut
                    .as_deref()
                    .map(|p| p.to_string_lossy().to_lowercase())
                    .unwrap_or_default()
            ),
            InstalledAppSource::AppPaths => format!(
                "apppath:{}",
                self.registry_key
                    .as_deref()
                    .map(str::to_lowercase)
                    .unwrap_or_default()
            ),
        }
    }
}

/// Something went wrong enumerating installed applications.
///
/// Enumeration is best-effort by nature — a machine has Start Menu entries that
/// point at uninstalled programs, and registry keys that no longer resolve — so
/// failures are collected rather than propagated per entry. This type is only
/// returned when *every* source failed, which on a real Windows install it
/// should not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEnumerationError {
    /// One message per source that failed.
    pub failures: Vec<String>,
}

impl fmt::Display for AppEnumerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no installed-app source could be read: {}",
            self.failures.join("; ")
        )
    }
}

impl std::error::Error for AppEnumerationError {}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Enumerates the Start Menu and `App Paths` on this machine.
///
/// Intended for a background executor: it walks directories, opens a COM
/// apartment for shortcut resolution, and reads several registry keys. Never
/// call it from `render`.
///
/// On failure of every source, returns [`AppEnumerationError`] with one entry
/// per failure. A partial result is still a result: losing the Start Menu
/// because one key is unreadable would be worse than a short list.
pub fn installed_apps() -> Result<Vec<InstalledApp>, AppEnumerationError> {
    let mut failures = Vec::new();
    let mut apps = Vec::new();

    match start_menu_roots() {
        Ok(roots) => apps.extend(start_menu_apps_in(&roots, &ShellLinkResolver)),
        Err(message) => failures.push(message),
    }

    match enumerate_app_paths() {
        Ok(found) => apps.extend(found),
        Err(message) => failures.push(message),
    }

    if apps.is_empty() && !failures.is_empty() {
        return Err(AppEnumerationError { failures });
    }

    Ok(dedupe(apps))
}

/// Enumerates Start Menu shortcuts under `roots`, resolving them with
/// `resolve`.
///
/// The injectable half of [`installed_apps`]: point `roots` at a temporary
/// directory and `resolve` at something that does not need a COM apartment, and
/// the walk, the naming, and the id scheme are all covered without touching the
/// machine's real Start Menu. `installed_apps` is this with the real roots and
/// the real resolver, plus the `App Paths` half.
///
/// De-duplication is applied, so an injected run is comparable with a real one.
pub fn start_menu_apps_in(roots: &[PathBuf], resolve: &dyn ShortcutResolver) -> Vec<InstalledApp> {
    dedupe(collect_from_roots(roots, resolve))
}

// ---------------------------------------------------------------------------
// Shortcut resolution
// ---------------------------------------------------------------------------

/// What a `.lnk` resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedShortcut {
    /// The executable the shortcut launches.
    pub target: PathBuf,
    /// Arguments baked into the shortcut, possibly empty.
    pub arguments: String,
}

/// Resolves a `.lnk` file to the program it launches.
///
/// A trait because the real implementation needs a COM apartment on the calling
/// thread, and a test that has to initialise COM to check a file-naming rule is
/// a test that fails on a machine without a shell.
pub trait ShortcutResolver: Send + Sync {
    /// Resolves `shortcut`, or `None` if it is broken.
    fn resolve(&self, shortcut: &Path) -> Option<ResolvedShortcut>;
}

/// Resolves shortcuts through `IShellLinkW`.
pub struct ShellLinkResolver;

impl ShortcutResolver for ShellLinkResolver {
    fn resolve(&self, shortcut: &Path) -> Option<ResolvedShortcut> {
        // COM apartments are per-thread and must be initialised before any
        // COM call on that thread. The guard makes the matching uninitialise
        // impossible to forget on an early return.
        let _com = ComApartment::enter()?;

        let path = wide_path_nul(shortcut);

        // SAFETY: `CoCreateInstance` is given a class id and a context with no
        // outer object; the result is checked before use and both COM objects
        // are released when the returned interfaces go out of scope.
        let shell_link: IShellLinkW =
            match unsafe { CoCreateInstance(&CLSID_SHELL_LINK, None, CLSCTX_INPROC_SERVER) } {
                Ok(link) => link,
                Err(_) => return None,
            };

        // SAFETY: `shell_link` is a live interface; `cast` only queries its
        // IUnknown for another interface and does not consume the original.
        let persist: IPersistFile = match shell_link.cast() {
            Ok(persist) => persist,
            Err(_) => return None,
        };

        // SAFETY: `path` is a NUL-terminated UTF-16 buffer that outlives the
        // call, and STGM_READ opens the file without modifying it.
        if unsafe { persist.Load(PCWSTR(path.as_ptr()), STGM_READ) }.is_err() {
            return None;
        }

        // Generous: MAX_PATH is the legacy limit, but App-V and store apps
        // produce longer resolved paths and a too-small buffer would turn a
        // valid shortcut into a dropped entry.
        let mut buffer = vec![0u16; 32768];
        let mut found: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        // SAFETY: `buffer` is a live allocation of 32 768 code units and the
        // length is passed to the shell, which is the documented contract;
        // `found` is a live, correctly sized out-parameter.
        let resolved = unsafe { shell_link.GetPath(&mut buffer, &mut found, 0) };
        if resolved.is_err() {
            return None;
        }

        let target = path_from_wide(&buffer);
        if target.as_os_str().is_empty() {
            // A shortcut with no target is a leftover from an uninstall. Not an
            // error, just not a runnable program.
            return None;
        }

        let mut arguments = vec![0u16; 32768];
        // SAFETY: as above, for the arguments buffer. A failure here is not
        // fatal — a shortcut with no arguments is the common case, and the
        // buffer stays zeroed, which decodes to an empty string.
        let got_arguments = unsafe { shell_link.GetArguments(&mut arguments) }.is_ok();

        Some(ResolvedShortcut {
            target,
            arguments: if got_arguments {
                string_from_wide(&arguments).unwrap_or_default()
            } else {
                String::new()
            },
        })
    }
}

/// Initialises COM for the current thread and uninitialises it on drop.
struct ComApartment {
    /// Whether this guard performed the initialisation. When COM was already up
    /// in a different apartment model, initialising fails with
    /// `RPC_E_CHANGED_MODE`; proceeding is correct, and uninitialising would
    /// be a bug that tears down a caller we do not own.
    owned: bool,
}

impl ComApartment {
    fn enter() -> Option<ComApartment> {
        // SAFETY: passes no interface pointer, so there is nothing to marshal
        // and the only state it changes is this thread's apartment.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_ok() {
            return Some(ComApartment { owned: true });
        }
        // S_FALSE means "already initialised on this thread with the same
        // model", which still obliges us to balance with a CoUninitialize.
        if hr == windows::Win32::Foundation::S_FALSE {
            return Some(ComApartment { owned: true });
        }
        // RPC_E_CHANGED_MODE: the caller already put this thread in another
        // apartment. Use it as-is; do not uninitialise it.
        if hr == windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            return Some(ComApartment { owned: false });
        }
        None
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: paired with the successful CoInitializeEx in `enter`,
            // and only when this guard performed it.
            unsafe { CoUninitialize() }
        }
    }
}

// ---------------------------------------------------------------------------
// Start Menu
// ---------------------------------------------------------------------------

/// The per-user and all-users Start Menu `Programs` folders.
///
/// A missing folder is not an error: a machine can have either one and never
/// both, and `FOLDERID_CommonPrograms` is absent on some editions.
fn start_menu_roots() -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    let mut failures = Vec::new();

    for (label, folder) in [
        ("Programs", &FOLDERID_Programs),
        ("CommonPrograms", &FOLDERID_CommonPrograms),
    ] {
        match known_folder(folder) {
            Ok(path) => roots.push(path),
            Err(message) => failures.push(format!("{label}: {message}")),
        }
    }

    if roots.is_empty() {
        Err(failures.join("; "))
    } else {
        Ok(roots)
    }
}

/// Resolves a known folder, freeing the string the shell allocated.
fn known_folder(folder: &GUID) -> Result<PathBuf, String> {
    // SAFETY: `folder` is a static GUID, the flags request the default path,
    // and the null token means "this user's". The returned pointer is shell-
    // allocated and is freed below on every path.
    let result = unsafe { SHGetKnownFolderPath(folder, KF_FLAG_DEFAULT, None) };
    match result {
        Ok(raw) => {
            // The shell returns a bare pointer, not a slice, so the length has
            // to be found the only way it can be: by scanning for the NUL.
            // SAFETY: `raw` is the shell's NUL-terminated allocation, valid
            // until the free on the next line.
            let decoded = unsafe { crate::wide::os_from_c_wide(raw.0) };
            // SAFETY: `raw` came from SHGetKnownFolderPath, which allocates
            // with CoTaskMemAlloc. This is its only free.
            unsafe { CoTaskMemFree(Some(raw.0.cast())) };
            Ok(PathBuf::from(decoded))
        }
        Err(e) => Err(format!(
            "SHGetKnownFolderPath failed (Win32 error {})",
            last_error_code(&e)
        )),
    }
}

/// Walks `roots` for `.lnk` files and resolves each one.
fn collect_from_roots(roots: &[PathBuf], resolve: &dyn ShortcutResolver) -> Vec<InstalledApp> {
    let mut found = Vec::new();
    let mut queue: Vec<PathBuf> = roots.to_vec();
    let mut visited = 0usize;

    // Bounded: a Start Menu with a pathological directory tree should cost
    // time, not the process. The real tree is a few hundred entries; a
    // thousand is already generous.
    const MAX_ENTRIES: usize = 8192;

    while let Some(dir) = queue.pop() {
        if visited >= MAX_ENTRIES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            // A directory we cannot read (permissions, a broken reparse point)
            // costs us that folder, not the whole enumeration.
            continue;
        };
        for entry in entries.flatten() {
            if visited >= MAX_ENTRIES {
                break;
            }
            visited += 1;
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_dir() {
                queue.push(path);
                continue;
            }

            if !has_lnk_extension(&path) {
                continue;
            }
            let Some(resolved) = resolve.resolve(&path) else {
                continue;
            };

            let Some(name) = shortcut_name(&path) else {
                continue;
            };

            found.push(InstalledApp {
                name,
                target: resolved.target,
                arguments: resolved.arguments,
                source: InstalledAppSource::StartMenu,
                shortcut: Some(path),
                registry_key: None,
            });
        }
    }

    found
}

/// Whether a path ends in `.lnk`, case-insensitively.
fn has_lnk_extension(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
}

/// The display name for a shortcut: its file name, minus the parts of it that
/// are not the program's name.
///
/// Taken from the file name rather than `IShellLink::GetDescription`, because
/// the description is a free-text field installers leave empty or set to
/// something the user never sees, and an entry with no name is unsearchable.
pub fn shortcut_name(shortcut: &Path) -> Option<String> {
    let stem = shortcut.file_stem()?;
    let raw = stem.to_string_lossy().into_owned();
    let cleaned = display_name_from(&raw);
    // A name is not optional in practice — an entry with no title is a row of
    // nothing — so a stem that is *entirely* noise still yields the raw stem
    // rather than `None`.
    Some(if cleaned.is_empty() { raw } else { cleaned })
}

/// Strips the two pieces of shortcut naming that are not part of a program's
/// name, and nothing else.
///
/// Both were found in a real Start Menu folder: a shortcut to
/// `windhawk.exe` sitting in `Startup` was named `windhawk.exe - Shortcut.lnk`,
/// so the launcher offered `windhawk.exe - Shortcut` as the name of a program
/// called Windhawk. That string is the row's title, which makes it the first
/// thing a person reads, and it is not a name anyone would type.
///
/// * ` - Shortcut`, which Windows appends on "Create shortcut". A program
///   genuinely called `Foo - Shortcut` loses four characters; the alternative is
///   every shortcut-based entry reading like a file operation.
/// * A trailing `.exe`, for the same reason [`app_path_entry_to_app`] drops it:
///   the extension is already on the target path, and repeating it in the title
///   is noise.
///
/// No other rewriting. Trimming, casing, and inner punctuation are left alone,
/// because a launcher that second-guesses a program's name is worse than one
/// that shows a slightly untidy one.
fn display_name_from(stem: &str) -> String {
    let trimmed = stem.trim();
    let base = strip_ignore_ascii_case(trimmed, " - shortcut")
        .unwrap_or(trimmed)
        .trim_end();
    strip_ignore_ascii_case(base, ".exe")
        .unwrap_or(base)
        .trim()
        .to_owned()
}

/// Removes `suffix` from the end of `text`, ignoring ASCII case.
///
/// Byte-indexed on purpose, and safe: an ASCII suffix cannot match bytes inside
/// a multi-byte UTF-8 sequence, so the split always lands on a character
/// boundary.
fn strip_ignore_ascii_case<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    let cut = text.len().checked_sub(suffix.len())?;
    text.get(cut..)
        .filter(|tail| tail.eq_ignore_ascii_case(suffix))
        .map(|_| &text[..cut])
}

// ---------------------------------------------------------------------------
// App Paths
// ---------------------------------------------------------------------------

/// Registry keys that hold `App Paths` entries.
///
/// `Wow6432Node` is listed alongside the native view because a 64-bit launcher
/// on 64-bit Windows needs both: the 32-bit view is where a lot of what users
/// actually run is registered.
const APP_PATHS_KEYS: &[(&str, HKEY, &str)] = &[
    (
        "HKLM",
        HKEY_LOCAL_MACHINE,
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths",
    ),
    (
        "HKLM\\Wow6432Node",
        HKEY_LOCAL_MACHINE,
        r"SOFTWARE\Wow6432Node\Microsoft\Windows\CurrentVersion\App Paths",
    ),
    (
        "HKCU",
        HKEY_CURRENT_USER,
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths",
    ),
];

/// Reads every `App Paths` entry on this machine.
pub fn enumerate_app_paths() -> Result<Vec<InstalledApp>, String> {
    let mut found = Vec::new();
    let mut failures = Vec::new();

    for (label, hive, subkey) in APP_PATHS_KEYS {
        match read_app_paths(*hive, subkey) {
            Ok(apps) => found.extend(apps),
            Err(message) => failures.push(format!("{label}: {message}")),
        }
    }

    if found.is_empty() && !failures.is_empty() {
        Err(failures.join("; "))
    } else {
        Ok(found)
    }
}

fn read_app_paths(hive: HKEY, subkey: &str) -> Result<Vec<InstalledApp>, String> {
    let subkey_wide = wide_nul(subkey);
    let mut key = HKEY::default();
    // SAFETY: `subkey_wide` is a NUL-terminated UTF-16 buffer that outlives the
    // call, and `key` is a live out-pointer. KEY_READ is the minimum for
    // enumeration and needs no elevation under either hive.
    let status =
        unsafe { RegOpenKeyExW(hive, PCWSTR(subkey_wide.as_ptr()), None, KEY_READ, &mut key) };
    if status != ERROR_SUCCESS {
        // A machine with no App Paths at all is a normal state, not a failure.
        return Ok(Vec::new());
    }
    let key = OwnedHkey(key);

    let mut apps = Vec::new();
    let mut index = 0u32;

    loop {
        // Names are capped at 255 characters by the registry itself; the extra
        // room is so a violation is a decode failure rather than a silent
        // truncation that could collide two keys.
        let mut name = vec![0u16; 260];
        let mut name_len = name.len() as u32;
        // SAFETY: `key` is live; `name` is a live allocation whose capacity is
        // passed in `name_len` and updated in place; the remaining out-pointers
        // are null, which the API accepts.
        let status = unsafe {
            RegEnumKeyExW(
                key.get(),
                index,
                Some(windows::core::PWSTR(name.as_mut_ptr())),
                &mut name_len,
                None,
                None,
                None,
                None,
            )
        };

        if status != ERROR_SUCCESS {
            break;
        }
        index += 1;

        let Some(key_name) = string_from_wide(&name[..name_len as usize]) else {
            continue;
        };
        let Some(value) = read_default_string(key.get()) else {
            continue;
        };
        let Some(app) = app_path_entry_to_app(&key_name, &value) else {
            continue;
        };
        apps.push(app);
    }

    Ok(apps)
}

/// Turns one App Paths subkey, and the string it holds, into an
/// [`InstalledApp`].
///
/// Takes the value as a `&str` rather than a registry handle so the naming and
/// argument-splitting rules are testable without a registry — and so this
/// crate's public API does not make consumers depend on `windows` for one
/// function.
pub fn app_path_entry_to_app(key_name: &str, value: &str) -> Option<InstalledApp> {
    // Only executable entries are runnable. App Paths also holds `.com` and
    // bare-name keys that a launcher should not offer as applications.
    if !key_name.to_ascii_lowercase().ends_with(".exe") {
        return None;
    }

    let (target, arguments) = split_app_paths_value(value);
    if target.is_empty() {
        return None;
    }

    Some(InstalledApp {
        // The key is `name.exe`; the extension is dropped for the display name
        // because the extension is on the target path, where the user expects
        // to see it, and repeating it in the title is noise.
        name: key_name[..key_name.len() - 4].to_owned(),
        target: PathBuf::from(target),
        arguments,
        source: InstalledAppSource::AppPaths,
        shortcut: None,
        registry_key: Some(key_name.to_owned()),
    })
}

/// Splits an App Paths value into an executable and its arguments.
///
/// The documented form is `"C:\path\app.exe" %1` — quoted path, then
/// substitution parameters. A value with no quotes is taken whole as the path,
/// because that is how unquoted-but-space-free values are written in practice.
pub fn split_app_paths_value(value: &str) -> (String, String) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return (String::new(), String::new());
    }

    if let Some(rest) = trimmed.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            let path = rest[..end].to_owned();
            let args = rest[end + 1..].trim().to_owned();
            return (path, args);
        }
    }

    // Unquoted. Split on the first run of whitespace so a value like
    // `C:\app.exe --flag` still resolves, but only when the leading token
    // actually looks like a path; otherwise the whole value is the path.
    match trimmed.find(char::is_whitespace) {
        Some(index) => (
            trimmed[..index].to_owned(),
            trimmed[index..].trim().to_owned(),
        ),
        None => (trimmed.to_owned(), String::new()),
    }
}

fn read_default_string(key: HKEY) -> Option<String> {
    use windows::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_SZ};

    let mut size: u32 = 0;
    // SAFETY: `key` is live, a null value name asks for the key's default
    // value, and a null data pointer with a size out-parameter is the
    // documented size query.
    let status = unsafe {
        RegGetValueW(
            key,
            None,
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }

    let units = (size as usize).div_ceil(2).max(1);
    let mut buffer = vec![0u16; units];
    let mut actual: u32 = 0;
    // SAFETY: `buffer` is a live allocation of `units` code units, which is
    // what the kernel is told it may write, and `actual` is a live
    // out-pointer.
    let status = unsafe {
        RegGetValueW(
            key,
            None,
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut actual),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }

    let written = ((actual as usize) / 2).min(buffer.len());
    string_from_wide(&buffer[..written])
}

// ---------------------------------------------------------------------------
// De-duplication
// ---------------------------------------------------------------------------

/// Collapses entries that point at the same executable and sorts by name.
///
/// Start Menu wins over `App Paths` for the same target: it carries the name
/// the user actually sees and any arguments the installer set up, which is
/// strictly more information than the bare registry path. That precedence is
/// also why the Start Menu list is ordered first.
fn dedupe(apps: Vec<InstalledApp>) -> Vec<InstalledApp> {
    let mut by_target: BTreeMap<String, InstalledApp> = BTreeMap::new();

    for app in apps {
        let key = app.target.to_string_lossy().to_lowercase();
        match by_target.get(&key) {
            Some(existing) if existing.source == InstalledAppSource::StartMenu => {
                // The Start Menu entry already won; keep it.
            }
            _ => {
                by_target.insert(key, app);
            }
        }
    }

    let mut out: Vec<InstalledApp> = by_target.into_values().collect();
    out.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id().cmp(&b.id()))
    });
    out
}

/// An owned `HKEY`, closed exactly once on drop.
struct OwnedHkey(HKEY);

impl OwnedHkey {
    fn get(&self) -> HKEY {
        self.0
    }
}

impl Drop for OwnedHkey {
    fn drop(&mut self) {
        // SAFETY: live and this is its only close. A leaked key handle survives
        // process exit, which matters for a long-lived tray launcher.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Resolves shortcuts from a table instead of the shell, so the walk and
    /// the naming rules are testable with no COM apartment and no Start Menu.
    struct TableResolver {
        table: Mutex<BTreeMap<PathBuf, ResolvedShortcut>>,
    }

    impl TableResolver {
        fn new() -> TableResolver {
            TableResolver {
                table: Mutex::new(BTreeMap::new()),
            }
        }

        /// Teaches the resolver about one shortcut, keyed by its full path —
        /// the walk hands back absolute paths, so a relative key would silently
        /// never match and the test would pass for the wrong reason.
        fn with(self, shortcut: &Path, target: &str, arguments: &str) -> TableResolver {
            self.table.lock().unwrap().insert(
                shortcut.to_path_buf(),
                ResolvedShortcut {
                    target: PathBuf::from(target),
                    arguments: arguments.to_owned(),
                },
            );
            self
        }
    }

    impl ShortcutResolver for TableResolver {
        fn resolve(&self, shortcut: &Path) -> Option<ResolvedShortcut> {
            self.table.lock().unwrap().get(shortcut).cloned()
        }
    }

    /// Builds a temporary Start-Menu-shaped tree. `TempTree` removes it on
    /// drop so a failing test does not leave debris behind.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> TempTree {
            let base = std::env::temp_dir().join(format!(
                "orca-win-apps-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).expect("temp dir");
            TempTree(base)
        }

        fn dir(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(&path).expect("subdir");
            path
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent");
            }
            std::fs::write(&path, b"stub").expect("stub file");
            path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_shortcut_is_named_after_its_file_stem() {
        assert_eq!(
            shortcut_name(Path::new(r"C:\Start Menu\Programs\Visual Studio Code.lnk")).as_deref(),
            Some("Visual Studio Code")
        );
    }

    /// The real case, from a real Start Menu folder: a Startup shortcut to
    /// `windhawk.exe` named `windhawk.exe - Shortcut.lnk`. The launcher offered
    /// `windhawk.exe - Shortcut` as the program's name, which is the row's title
    /// and therefore the first thing a person reads.
    #[test]
    fn shortcut_naming_noise_is_stripped_from_the_display_name() {
        for (path, expected) in [
            (r"C:\...\Startup\windhawk.exe - Shortcut.lnk", "windhawk"),
            (r"C:\...\Programs\Foo - shortcut.lnk", "Foo"),
            (r"C:\...\Programs\Foo - SHORTCUT.lnk", "Foo"),
            (r"C:\...\Programs\tool.EXE.lnk", "tool"),
            (r"C:\...\Programs\tool.exe - Shortcut.lnk", "tool"),
            // Not noise, and must survive untouched: the name is the whole point.
            (r"C:\...\Programs\Shortcut Manager.lnk", "Shortcut Manager"),
            (r"C:\...\Programs\My Shortcut Tool.lnk", "My Shortcut Tool"),
            // All-noise stems still produce a name. An entry with no title is a
            // row of nothing, which is worse than an untidy one.
            (r"C:\...\Programs\Shortcut.lnk", "Shortcut"),
            (r"C:\...\Programs\.exe.lnk", ".exe"),
        ] {
            assert_eq!(
                shortcut_name(Path::new(path)).as_deref(),
                Some(expected),
                "{path}"
            );
        }
    }

    #[test]
    fn the_lnk_extension_check_is_case_insensitive() {
        // Installers write both `.lnk` and `.LNK`; treating them differently
        // would silently drop half a Start Menu.
        assert!(has_lnk_extension(Path::new("a.lnk")));
        assert!(has_lnk_extension(Path::new("a.LNK")));
        assert!(has_lnk_extension(Path::new("a.Lnk")));
        assert!(!has_lnk_extension(Path::new("a.url")));
        assert!(!has_lnk_extension(Path::new("a")));
    }

    #[test]
    fn the_walk_finds_shortcuts_in_nested_folders() {
        let tree = TempTree::new("walk");
        let root = tree.dir("Programs");
        let top = tree.file("Programs/Top.lnk");
        tree.dir("Programs/Nested");
        let deep = tree.file("Programs/Nested/Deep.lnk");
        tree.file("Programs/Ignored.url");
        tree.file("Programs/Readme.txt");

        let resolve = TableResolver::new()
            .with(&top, r"C:\apps\top.exe", "--fast")
            .with(&deep, r"C:\apps\deep.exe", "");

        let found = collect_from_roots(std::slice::from_ref(&root), &resolve);
        let names: Vec<&str> = found.iter().map(|a| a.name.as_str()).collect();

        assert!(names.contains(&"Top"), "{names:?}");
        assert!(names.contains(&"Deep"), "{names:?}");
        assert!(!names.contains(&"Readme"), "{names:?}");
        assert!(!names.contains(&"Ignored"), "{names:?}");
    }

    #[test]
    fn a_broken_shortcut_is_dropped_rather_than_reported() {
        // A shortcut pointing at an uninstalled program is routine, and one
        // unreadable entry must not fail the enumeration.
        let tree = TempTree::new("broken");
        let root = tree.dir("Programs");
        let good = tree.file("Programs/Good.lnk");
        tree.file("Programs/Dangling.lnk");

        let resolve = TableResolver::new().with(&good, r"C:\apps\good.exe", "");

        let found = collect_from_roots(std::slice::from_ref(&root), &resolve);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Good");
    }

    #[test]
    fn a_shortcut_entry_carries_its_path_arguments_and_id() {
        let tree = TempTree::new("fields");
        let root = tree.dir("Programs");
        let lnk = tree.file("Programs/Editor.lnk");
        let resolve = TableResolver::new().with(&lnk, r"C:\apps\editor.exe", "--new");

        let found = collect_from_roots(std::slice::from_ref(&root), &resolve);
        let app = found.first().expect("one entry");

        assert_eq!(app.name, "Editor");
        assert_eq!(app.target, PathBuf::from(r"C:\apps\editor.exe"));
        assert_eq!(app.arguments, "--new");
        assert_eq!(app.source, InstalledAppSource::StartMenu);
        assert_eq!(app.shortcut.as_deref(), Some(lnk.as_path()));
        assert!(app.id().starts_with("startmenu:"));
        assert_eq!(app.id(), app.id().to_lowercase());
    }

    #[test]
    fn ids_are_stable_and_distinct_per_source() {
        let start_menu = InstalledApp {
            name: "Editor".into(),
            target: PathBuf::from(r"C:\apps\editor.exe"),
            arguments: String::new(),
            source: InstalledAppSource::StartMenu,
            shortcut: Some(PathBuf::from(r"C:\Start Menu\Programs\Editor.lnk")),
            registry_key: None,
        };
        let app_path = InstalledApp {
            name: "editor".into(),
            target: PathBuf::from(r"C:\apps\editor.exe"),
            arguments: String::new(),
            source: InstalledAppSource::AppPaths,
            shortcut: None,
            registry_key: Some("editor.exe".into()),
        };

        assert_ne!(start_menu.id(), app_path.id());
        assert_eq!(start_menu.id(), start_menu.id());
        // Case-insensitive, because Windows paths and registry keys are.
        assert_eq!(
            app_path.id(),
            InstalledApp {
                registry_key: Some("EDITOR.EXE".into()),
                ..app_path.clone()
            }
            .id()
        );
    }

    #[test]
    fn an_app_paths_entry_keeps_its_key_name_as_the_id_and_drops_the_extension() {
        let app = app_path_entry_to_app("Code.exe", r#""C:\apps\code.exe""#).expect("valid entry");
        assert_eq!(app.name, "Code");
        assert_eq!(app.target, PathBuf::from(r"C:\apps\code.exe"));
        assert_eq!(app.registry_key.as_deref(), Some("Code.exe"));
        assert_eq!(app.id(), "apppath:code.exe");
    }

    #[test]
    fn an_app_paths_entry_that_is_not_an_executable_is_skipped() {
        // App Paths also holds `.com` and bare-name keys; neither is an
        // application a launcher should offer.
        assert!(app_path_entry_to_app("cmd.com", r"C:\Windows\System32\cmd.com").is_none());
        assert!(app_path_entry_to_app("notepad", r"C:\Windows\notepad.exe").is_none());
    }

    #[test]
    fn an_app_paths_entry_with_no_usable_value_is_skipped() {
        assert!(app_path_entry_to_app("ghost.exe", "   ").is_none());
    }

    #[test]
    fn splits_a_quoted_app_paths_value_from_its_arguments() {
        let (target, args) = split_app_paths_value(r#""C:\Program Files\app.exe" --flag %1"#);
        assert_eq!(target, r"C:\Program Files\app.exe");
        assert_eq!(args, r"--flag %1");
    }

    #[test]
    fn an_unquoted_app_paths_value_with_no_arguments_is_whole() {
        let (target, args) = split_app_paths_value(r"C:\apps\app.exe");
        assert_eq!(target, r"C:\apps\app.exe");
        assert_eq!(args, "");
    }

    #[test]
    fn an_unquoted_app_paths_value_with_arguments_splits_at_the_space() {
        let (target, args) = split_app_paths_value(r"C:\apps\app.exe --flag");
        assert_eq!(target, r"C:\apps\app.exe");
        assert_eq!(args, "--flag");
    }

    #[test]
    fn an_empty_app_paths_value_yields_nothing() {
        assert_eq!(split_app_paths_value("   "), (String::new(), String::new()));
    }

    #[test]
    fn the_walk_is_bounded_by_a_budget() {
        // Sanity check on the constant rather than a stress test: the point is
        // that the bound exists, and that a normal tree is nowhere near it.
        let tree = TempTree::new("bounded");
        let root = tree.dir("Programs");
        let one = tree.file("Programs/One.lnk");
        let resolve = TableResolver::new().with(&one, r"C:\apps\one.exe", "");
        let found = collect_from_roots(std::slice::from_ref(&root), &resolve);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn the_injected_entry_point_matches_a_real_enumeration() {
        // Exercises the same path `installed_apps` uses for the Start Menu
        // half, with the two things a test cannot supply swapped out.
        let tree = TempTree::new("injected");
        let root = tree.dir("Programs");
        let editor = tree.file("Programs/Editor.lnk");
        let browser = tree.file("Programs/Nested/Browser.lnk");

        let resolve = TableResolver::new()
            .with(&editor, r"C:\apps\editor.exe", "--new")
            .with(&browser, r"C:\apps\browser.exe", "");

        let apps = start_menu_apps_in(std::slice::from_ref(&root), &resolve);
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();

        // Sorted case-insensitively, and only resolvable entries.
        assert_eq!(names, vec!["Browser", "Editor"]);
        let editor_app = apps.iter().find(|a| a.name == "Editor").expect("Editor");
        assert_eq!(editor_app.arguments, "--new");
        assert!(editor_app.id().starts_with("startmenu:"));
    }

    #[test]
    fn enumerating_this_machine_does_not_panic() {
        // A read-only smoke test with no assertions about content: the machine's
        // Start Menu differs per install, so anything more specific would be a
        // test of the machine rather than of the code. The point is that the
        // real COM and registry paths are reachable and do not abort.
        let _ = installed_apps();
    }

    #[test]
    fn dedupe_prefers_the_start_menu_entry_for_the_same_target() {
        let tree = TempTree::new("dedupe");
        let lnk = tree.file("Programs/Editor.lnk");
        let resolve = TableResolver::new().with(&lnk, r"C:\apps\editor.exe", "--gui");

        let mut all = collect_from_roots(std::slice::from_ref(&tree.0), &resolve);
        all.push(InstalledApp {
            name: "editor".into(),
            target: PathBuf::from(r"C:\apps\editor.exe"),
            arguments: String::new(),
            source: InstalledAppSource::AppPaths,
            shortcut: None,
            registry_key: Some("editor.exe".into()),
        });

        let deduped = dedupe(all);
        assert_eq!(deduped.len(), 1, "same target, one entry");
        assert_eq!(
            deduped[0].source,
            InstalledAppSource::StartMenu,
            "the Start Menu entry carries the name the user sees and the real arguments"
        );
    }

    #[test]
    fn dedupe_sorts_case_insensitively_by_name() {
        let make = |name: &str, target: &str| InstalledApp {
            name: name.into(),
            target: PathBuf::from(target),
            arguments: String::new(),
            source: InstalledAppSource::AppPaths,
            shortcut: None,
            registry_key: Some(name.into()),
        };
        let deduped = dedupe(vec![
            make("zebra", r"C:\z.exe"),
            make("Apple", r"C:\a.exe"),
            make("banana", r"C:\b.exe"),
        ]);
        let names: Vec<&str> = deduped.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Apple", "banana", "zebra"]);
    }

    #[test]
    fn dedupe_is_case_insensitive_on_the_target() {
        let make = |target: &str| InstalledApp {
            name: "editor".into(),
            target: PathBuf::from(target),
            arguments: String::new(),
            source: InstalledAppSource::AppPaths,
            shortcut: None,
            registry_key: Some("editor.exe".into()),
        };
        // Windows paths are case-insensitive, so these are the same program.
        let deduped = dedupe(vec![
            make(r"C:\Apps\Editor.exe"),
            make(r"C:\apps\editor.exe"),
        ]);
        assert_eq!(deduped.len(), 1);
    }

    #[test]
    fn the_app_paths_view_listings_are_both_present() {
        // A 64-bit launcher on 64-bit Windows needs the 32-bit view too: that
        // is where much of what users run is registered.
        let labels: Vec<&str> = APP_PATHS_KEYS.iter().map(|(l, _, _)| *l).collect();
        assert!(labels.contains(&"HKLM"));
        assert!(labels.contains(&"HKLM\\Wow6432Node"));
        assert!(labels.contains(&"HKCU"));
    }
}
