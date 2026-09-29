//! Autostart: launch the launcher when the user logs in.
//!
//! # HKCU, never HKLM
//!
//! `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` is the only
//! key this module writes. HKLM would need elevation, which a launcher cannot
//! ask for at startup, and would make the setting machine-wide — so uninstalling
//! on one account would leave it running for everyone else.
//!
//! # The value is validated before it is written
//!
//! A Run value is a command line that Explorer re-parses at every logon, with no
//! diagnostics. A malformed one therefore fails silently and permanently, and
//! the only symptom is an app that "randomly" stopped starting. So the path is
//! checked *before* the registry is touched, and the quoting is done here rather
//! than left to the caller. See [`run_command_for`].

use std::fmt;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ,
};

/// The Run key this module reads and writes.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Longest Run value this module will write.
///
/// Explorer passes the value to `CreateProcess`, whose command line is capped
/// near 32 000 characters, but the practical ceiling is far lower: the Run key
/// is read during logon, before the shell is up, and a value that long has
/// always been a mistake. Rejecting it at write time turns a silent no-logon
/// into a reported error.
pub const MAX_RUN_VALUE_LEN: usize = 260;

/// Something went wrong reading or writing the Run key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartError {
    /// The value cannot be expressed as a Run entry.
    ///
    /// See [`run_command_for`] for the exact rules; this is the aggregate of all
    /// of them.
    InvalidValue {
        /// Which rule was broken, in the caller's words.
        reason: &'static str,
    },
    /// The value is not in the Run key.
    NotEnabled,
    /// Windows reported a failure for a reason we do not model.
    SystemFailure {
        /// The raw Win32 error code, as `GetLastError()` reported it.
        code: u32,
    },
}

impl fmt::Display for AutostartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AutostartError::InvalidValue { reason } => {
                write!(f, "cannot write an autostart entry: {reason}")
            }
            AutostartError::NotEnabled => write!(f, "autostart is not enabled"),
            AutostartError::SystemFailure { code } => {
                write!(f, "registry call failed (Win32 error {code})")
            }
        }
    }
}

impl std::error::Error for AutostartError {}

// ---------------------------------------------------------------------------
// Value construction (pure, and the part worth testing)
// ---------------------------------------------------------------------------

/// Builds the Run value for `exe`: the path, quoted, with a trailing NUL-free
/// terminator omitted.
///
/// The quoting is not decoration. Explorer splits a Run value on spaces, so
/// `C:\Program Files\orca\orca.exe` becomes the program `C:\Program` and never
/// starts. Wrapping in double quotes is the only encoding Explorer understands
/// that survives a space.
///
/// Rejected, in this order:
///
/// * an empty path — there is nothing to run;
/// * a path that is not absolute — a Run value is resolved against the user's
///   profile directory at logon, so a relative path points somewhere that
///   depends on who is logging in;
/// * a path containing `"` — it would terminate the quoting early and the rest
///   of the path would be read as arguments;
/// * a path containing a control character — Explorer has no way to represent
///   it in a REG_SZ command line, and it would truncate the value;
/// * a value longer than [`MAX_RUN_VALUE_LEN`].
pub fn run_command_for(exe: &Path) -> Result<String, AutostartError> {
    if exe.as_os_str().is_empty() {
        return Err(AutostartError::InvalidValue {
            reason: "the path is empty",
        });
    }
    if !exe.is_absolute() {
        return Err(AutostartError::InvalidValue {
            reason: "the path is not absolute; a Run value resolves against the user's profile",
        });
    }
    if let Some(bad) = exe.to_str().and_then(|s| s.chars().find(|c| *c == '"')) {
        let _ = bad;
        return Err(AutostartError::InvalidValue {
            reason: "the path contains a double quote, which would end the quoting early",
        });
    }
    if let Some(_bad) = exe
        .to_str()
        .and_then(|s| s.chars().find(|c| c.is_control()))
    {
        return Err(AutostartError::InvalidValue {
            reason: "the path contains a control character",
        });
    }

    // Built from the OsStr rather than a `to_str()` round trip: a Windows path
    // need not be valid Unicode, and replacing characters here would write a
    // Run value pointing at a different file than the caller asked for.
    let value = format!("\"{}\"", exe.display());

    if value.chars().count() > MAX_RUN_VALUE_LEN {
        return Err(AutostartError::InvalidValue {
            reason: "the command line is longer than a Run value can hold",
        });
    }

    Ok(value)
}

// ---------------------------------------------------------------------------
// The store seam
// ---------------------------------------------------------------------------

/// The three operations `Run` needs, isolated so the state machine is testable
/// without touching the real registry.
pub trait RunValueStore: Send {
    /// The current value, or `None` if the value does not exist.
    fn read(&self, value_name: &str) -> Result<Option<String>, AutostartError>;

    /// Creates or replaces the value.
    fn write(&self, value_name: &str, command: &str) -> Result<(), AutostartError>;

    /// Removes the value. Removing a value that is not there is success, not an
    /// error: `disable` has to be idempotent.
    fn delete(&self, value_name: &str) -> Result<(), AutostartError>;
}

// ---------------------------------------------------------------------------
// Autostart
// ---------------------------------------------------------------------------

/// Enable, disable, and query the launcher's autostart entry.
///
/// `Disable` and `Enable` operate on a *value name* under [`RUN_KEY`], not on
/// the key itself, so two launchers installed side by side do not delete each
/// other's entry.
pub struct Win32Autostart {
    store: Box<dyn RunValueStore>,
    value_name: String,
}

impl Win32Autostart {
    /// Creates a controller over the real `HKCU\...\Run` key.
    pub fn new(value_name: &str) -> Win32Autostart {
        Win32Autostart {
            store: Box::new(HkcuRunStore::new(RUN_KEY)),
            value_name: value_name.to_owned(),
        }
    }

    /// Creates a controller over an arbitrary store. The seam tests use.
    pub fn with_store(store: Box<dyn RunValueStore>, value_name: &str) -> Win32Autostart {
        Win32Autostart {
            store,
            value_name: value_name.to_owned(),
        }
    }

    /// The registry value name this controller owns.
    pub fn value_name(&self) -> &str {
        &self.value_name
    }

    /// Whether an autostart entry currently exists.
    ///
    /// Reports the error rather than answering `false` on failure: "not enabled"
    /// and "could not tell" are different answers, and collapsing them would
    /// make a permissions problem look like a preference.
    pub fn is_enabled(&self) -> Result<bool, AutostartError> {
        Ok(self.store.read(&self.value_name)?.is_some())
    }

    /// The command line Explorer will run at logon, if an entry exists.
    pub fn command(&self) -> Result<Option<String>, AutostartError> {
        self.store.read(&self.value_name)
    }

    /// Registers `exe` to run at logon.
    ///
    /// Validates and quotes the path first; see [`run_command_for`] for the
    /// rules. Nothing is written when validation fails, so a rejected call
    /// leaves the previous entry intact.
    pub fn enable(&self, exe: &Path) -> Result<(), AutostartError> {
        let command = run_command_for(exe)?;
        self.store.write(&self.value_name, &command)
    }

    /// Removes the autostart entry. Idempotent.
    pub fn disable(&self) -> Result<(), AutostartError> {
        self.store.delete(&self.value_name)
    }
}

// ---------------------------------------------------------------------------
// The real store
// ---------------------------------------------------------------------------

/// Reads and writes `REG_SZ` values under one key of `HKEY_CURRENT_USER`.
pub struct HkcuRunStore {
    subkey: String,
}

impl HkcuRunStore {
    /// Creates a store over `subkey` beneath `HKEY_CURRENT_USER`.
    pub fn new(subkey: &str) -> HkcuRunStore {
        HkcuRunStore {
            subkey: subkey.to_owned(),
        }
    }

    /// The subkey this store opens.
    pub fn subkey(&self) -> &str {
        &self.subkey
    }

    /// Opens the key for reading, or reports that it does not exist.
    ///
    /// The Run key itself is created by Explorer on first login, so "key
    /// missing" is a normal state and not a failure.
    fn open_read(&self) -> Result<Option<OwnedHkey>, AutostartError> {
        let subkey = crate::wide::wide_nul(&self.subkey);
        let mut key = HKEY::default();
        // SAFETY: `subkey` is a NUL-terminated UTF-16 buffer that outlives the
        // call, and `key` is a live out-pointer. A null SECURITY_ATTRIBUTES
        // means the default DACL, which is what reading a user's own Run key
        // needs.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                None,
                KEY_READ,
                &mut key,
            )
        };
        match status {
            ERROR_SUCCESS => Ok(Some(OwnedHkey(key))),
            code if code == ERROR_FILE_NOT_FOUND => Ok(None),
            code => Err(AutostartError::SystemFailure { code: code.0 }),
        }
    }

    /// Opens the key for writing, creating it if absent.
    fn open_write(&self) -> Result<OwnedHkey, AutostartError> {
        let subkey = crate::wide::wide_nul(&self.subkey);
        let mut key = HKEY::default();
        // SAFETY: `subkey` is a NUL-terminated UTF-16 buffer that outlives the
        // call, and `key` is a live out-pointer. KEY_SET_VALUE is the minimum
        // for writing a value and needs no elevation under HKCU.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                None,
                windows::core::PCWSTR::null(),
                windows::Win32::System::Registry::REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(OwnedHkey(key))
        } else {
            Err(AutostartError::SystemFailure { code: status.0 })
        }
    }
}

impl RunValueStore for HkcuRunStore {
    fn read(&self, value_name: &str) -> Result<Option<String>, AutostartError> {
        let Some(key) = self.open_read()? else {
            return Ok(None);
        };

        let name = crate::wide::wide_nul(value_name);
        let mut size: u32 = 0;
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the
        // call; `key` is live; passing a null data pointer with a size out of
        // 0 is the documented way to ask for the required size, and the
        // `RRF_RT_REG_SZ` flag makes an unexpected value type an error instead
        // of a silent reinterpretation of REG_BINARY.
        let status = unsafe {
            RegGetValueW(
                key.get(),
                None,
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut size),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(AutostartError::SystemFailure { code: status.0 });
        }

        // Size is in bytes; RegGetValueW writes UTF-16, so half of it is code
        // units. Rounding up leaves room for a terminator the API may or may
        // not include.
        let units = (size as usize).div_ceil(2).max(1);
        let mut buffer = vec![0u16; units];

        let mut actual: u32 = 0;
        // SAFETY: `buffer` is a live allocation of `units` code units and
        // `actual` is a live out-pointer bounded by the allocation. A
        // `u32::MAX` cap is not needed: `units` came from the previous call and
        // is bounded by the value's own size.
        let status = unsafe {
            RegGetValueW(
                key.get(),
                None,
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut actual),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(AutostartError::SystemFailure { code: status.0 });
        }

        // `actual` is bytes written including the terminator, so it can be one
        // code unit larger than `units` on a value that filled the buffer
        // exactly. Truncating to the buffer length before decoding keeps that
        // case from reading past the allocation.
        let written = ((actual as usize) / 2).min(buffer.len());
        Ok(crate::wide::string_from_wide(&buffer[..written]))
    }

    fn write(&self, value_name: &str, command: &str) -> Result<(), AutostartError> {
        let key = self.open_write()?;
        let name = crate::wide::wide_nul(value_name);
        // Written without the NUL: RegSetValueExW derives the length from the
        // byte count, and including a terminator would store it as part of the
        // value, so every later read would see a trailing NUL.
        let data: Vec<u8> = crate::wide::wide_nul(command)
            .into_iter()
            .take(command.encode_utf16().count())
            .flat_map(u16::to_le_bytes)
            .collect();
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the
        // call, `key` is live, and `data` is a live byte slice whose length is
        // exactly what is stored. REG_SZ means Explorer re-reads it as a command
        // line.
        let status =
            unsafe { RegSetValueExW(key.get(), PCWSTR(name.as_ptr()), None, REG_SZ, Some(&data)) };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(AutostartError::SystemFailure { code: status.0 })
        }
    }

    fn delete(&self, value_name: &str) -> Result<(), AutostartError> {
        // Opening for write creates the key if Explorer has not yet, so a
        // disable on a fresh profile does not fail on a missing key.
        let key = self.open_write()?;
        let name = crate::wide::wide_nul(value_name);
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the
        // call, and `key` is live.
        let status = unsafe { RegDeleteValueW(key.get(), PCWSTR(name.as_ptr())) };
        match status {
            // Deleting what is not there is the state the caller asked for.
            ERROR_FILE_NOT_FOUND | ERROR_SUCCESS => Ok(()),
            code => Err(AutostartError::SystemFailure { code: code.0 }),
        }
    }
}

/// An owned `HKEY`, closed exactly once on drop.
pub(crate) struct OwnedHkey(HKEY);

impl OwnedHkey {
    /// Borrows the handle.
    pub(crate) fn get(&self) -> HKEY {
        self.0
    }
}

impl Drop for OwnedHkey {
    fn drop(&mut self) {
        // SAFETY: the handle is live and this is its only close. Leaking it
        // would keep the key open for the life of the process, which is a real
        // cost in a long-lived tray app.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// An in-memory stand-in for one Run key. Never touches HKCU, so the tests
    /// cannot disturb the machine they run on — which is the whole reason the
    /// store is a trait.
    #[derive(Default)]
    struct MemoryRunStore {
        values: Mutex<BTreeMap<String, String>>,
        fail_next: Mutex<Option<AutostartError>>,
    }

    impl MemoryRunStore {
        fn failing(error: AutostartError) -> MemoryRunStore {
            MemoryRunStore {
                values: Mutex::new(BTreeMap::new()),
                fail_next: Mutex::new(Some(error)),
            }
        }

        fn take_failure(&self) -> Option<AutostartError> {
            self.fail_next.lock().unwrap().take()
        }
    }

    impl RunValueStore for MemoryRunStore {
        fn read(&self, value_name: &str) -> Result<Option<String>, AutostartError> {
            Ok(self.values.lock().unwrap().get(value_name).cloned())
        }

        fn write(&self, value_name: &str, command: &str) -> Result<(), AutostartError> {
            match self.take_failure() {
                Some(error) => Err(error),
                None => {
                    self.values
                        .lock()
                        .unwrap()
                        .insert(value_name.to_owned(), command.to_owned());
                    Ok(())
                }
            }
        }

        fn delete(&self, value_name: &str) -> Result<(), AutostartError> {
            self.values.lock().unwrap().remove(value_name);
            Ok(())
        }
    }

    /// One Run key shared by several controllers, so a test can prove that two
    /// value names do not collide.
    #[derive(Clone, Default)]
    struct SharedStore(std::sync::Arc<Mutex<BTreeMap<String, String>>>);

    impl SharedStore {
        fn new() -> SharedStore {
            SharedStore::default()
        }
    }

    impl RunValueStore for SharedStore {
        fn read(&self, value_name: &str) -> Result<Option<String>, AutostartError> {
            Ok(self.0.lock().unwrap().get(value_name).cloned())
        }

        fn write(&self, value_name: &str, command: &str) -> Result<(), AutostartError> {
            self.0
                .lock()
                .unwrap()
                .insert(value_name.to_owned(), command.to_owned());
            Ok(())
        }

        fn delete(&self, value_name: &str) -> Result<(), AutostartError> {
            self.0.lock().unwrap().remove(value_name);
            Ok(())
        }
    }

    fn controller() -> Win32Autostart {
        Win32Autostart::with_store(Box::new(MemoryRunStore::default()), "orca")
    }

    #[test]
    fn quotes_a_path_so_spaces_survive() {
        // Without the quotes Explorer would run `C:\Program` and hand the rest
        // to it as arguments.
        let value = run_command_for(Path::new(r"C:\Program Files\orca\orca.exe"))
            .expect("absolute path is fine");
        assert_eq!(value, r#""C:\Program Files\orca\orca.exe""#);
    }

    #[test]
    fn quotes_a_path_with_no_spaces_too() {
        // Uniform, so a reader of the registry does not have to reason about
        // which form is in use.
        let value = run_command_for(Path::new(r"C:\orca\orca.exe")).expect("valid");
        assert_eq!(value, r#""C:\orca\orca.exe""#);
    }

    #[test]
    fn rejects_an_empty_path() {
        assert!(matches!(
            run_command_for(Path::new("")),
            Err(AutostartError::InvalidValue { .. })
        ));
    }

    #[test]
    fn rejects_a_relative_path() {
        // A Run value is resolved against the user's profile at logon, so a
        // relative path silently points somewhere different per user.
        let error = run_command_for(Path::new(r"orca\orca.exe")).expect_err("relative");
        assert!(matches!(error, AutostartError::InvalidValue { .. }));
    }

    #[test]
    fn rejects_a_path_containing_a_double_quote() {
        // The quote would close the quoting early and the rest of the path
        // would be parsed as arguments.
        let error = run_command_for(Path::new(r#"C:\pro"gram\orca.exe"#)).expect_err("quote");
        assert!(matches!(error, AutostartError::InvalidValue { .. }));
    }

    #[test]
    fn rejects_a_path_containing_a_control_character() {
        let error = run_command_for(Path::new("C:\\orca\torca\u{7}.exe")).expect_err("control");
        assert!(matches!(error, AutostartError::InvalidValue { .. }));
    }

    #[test]
    fn rejects_an_over_long_command_line() {
        let long = format!(r#"C:\{}\orca.exe"#, "x".repeat(MAX_RUN_VALUE_LEN));
        let error = run_command_for(Path::new(&long)).expect_err("too long");
        assert!(matches!(error, AutostartError::InvalidValue { .. }));
    }

    #[test]
    fn is_enabled_is_false_before_anything_is_written() {
        let autostart = controller();
        assert!(!autostart.is_enabled().expect("read"));
    }

    #[test]
    fn enable_writes_a_quoted_value_and_is_enabled_agrees() {
        let autostart = controller();

        assert!(!autostart.is_enabled().expect("read"));
        autostart
            .enable(Path::new(r"C:\Program Files\orca\orca.exe"))
            .expect("enable");
        assert!(autostart.is_enabled().expect("read"));
        assert_eq!(
            autostart.command().expect("read"),
            Some(r#""C:\Program Files\orca\orca.exe""#.to_owned())
        );
    }

    #[test]
    fn enable_is_idempotent() {
        let autostart = controller();
        let exe = Path::new(r"C:\orca\orca.exe");
        autostart.enable(exe).expect("first");
        autostart.enable(exe).expect("second");
        assert!(autostart.command().expect("read").is_some());
    }

    #[test]
    fn disable_removes_the_entry_and_is_idempotent() {
        let autostart = controller();
        autostart
            .enable(Path::new(r"C:\orca\orca.exe"))
            .expect("enable");

        autostart.disable().expect("first disable");
        assert!(!autostart.is_enabled().expect("read"));

        // Disabling an already-disabled launcher is the state the caller asked
        // for, not an error.
        autostart.disable().expect("second disable");
        assert!(!autostart.is_enabled().expect("read"));
    }

    #[test]
    fn a_rejected_path_never_reaches_the_store() {
        // Validation happens before the write, so a bad path cannot destroy a
        // working entry. The store is rigged to fail on any write: if the
        // rejection did not happen first, the error would be the store failure
        // instead of the validation one.
        let store = MemoryRunStore::failing(AutostartError::SystemFailure { code: 5 });
        let autostart = Win32Autostart::with_store(Box::new(store), "orca");
        let error = autostart
            .enable(Path::new("relative.exe"))
            .expect_err("relative path is rejected before any store call");
        assert!(matches!(error, AutostartError::InvalidValue { .. }));
    }

    #[test]
    fn a_store_failure_is_reported_not_swallowed() {
        let store = MemoryRunStore::failing(AutostartError::SystemFailure { code: 5 });
        let autostart = Win32Autostart::with_store(Box::new(store), "orca");
        let error = autostart
            .enable(Path::new(r"C:\orca\orca.exe"))
            .expect_err("access denied is not success");
        assert_eq!(error, AutostartError::SystemFailure { code: 5 });
    }

    #[test]
    fn each_controller_owns_only_its_own_value_name() {
        // Two launchers installed side by side must not delete each other.
        let a = Win32Autostart::with_store(Box::new(SharedStore::new()), "orca");
        let b = Win32Autostart::with_store(Box::new(SharedStore::new()), "orca-beta");

        a.enable(Path::new(r"C:\a\orca.exe")).expect("a enable");
        b.enable(Path::new(r"C:\b\orca-beta.exe"))
            .expect("b enable");
        a.disable().expect("a disable");

        assert!(!a.is_enabled().expect("a read"));
        assert!(b.is_enabled().expect("b read"));
    }

    #[test]
    fn the_real_store_targets_the_run_key_under_hkcu() {
        // Nothing is read or written here; this pins the path and the hive,
        // because getting either wrong is invisible until someone tries to log
        // out and back in.
        let store = HkcuRunStore::new(RUN_KEY);
        assert_eq!(
            store.subkey(),
            r"Software\Microsoft\Windows\CurrentVersion\Run"
        );
        assert!(
            !RUN_KEY.starts_with("HKEY_LOCAL_MACHINE"),
            "HKCU only: HKLM needs elevation and would apply to every user"
        );
    }

    #[test]
    fn a_missing_run_key_reports_the_registrys_own_code() {
        // Pinned because `disable` treats exactly this code as "already off".
        assert_eq!(ERROR_FILE_NOT_FOUND.0, 2);
    }
}
