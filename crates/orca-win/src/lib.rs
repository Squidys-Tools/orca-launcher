//! `orca-win` — Windows platform integration for orca.
//!
//! Everything in this crate talks to the operating system: registering a
//! global hotkey, deciding whether this process is the primary instance,
//! toggling autostart, the tray icon, enumerating windows. The GPUI binary
//! talks to this crate; this crate never talks to GPUI.
//!
//! # Status: seams only
//!
//! The traits below are the real API surface the rest of the app will be
//! written against, and each has a Win32 implementation that **honestly
//! reports that it is not implemented**. None of them fakes success. The
//! registry calls are not written yet.
//!
//! Splitting the seams out before the behaviour means the shape of the
//! platform layer is reviewable on its own, and the UI can be developed
//! against a fake without a `RegisterHotKey` in the way.

use std::error::Error;
use std::fmt;

// ---------------------------------------------------------------------------
// Hotkey value types
// ---------------------------------------------------------------------------

/// A set of held modifier keys.
///
/// A bitset rather than four `bool` fields so that `Modifiers` stays one word,
/// stays `const`-constructible, and gains a total `union` without the caller
/// having to remember which fields exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    /// No modifiers.
    pub const NONE: Modifiers = Modifiers(0);
    /// <kbd>Alt</kbd> / <kbd>Option</kbd>.
    pub const ALT: Modifiers = Modifiers(1 << 0);
    /// <kbd>Ctrl</kbd> / <kbd>Control</kbd>.
    pub const CTRL: Modifiers = Modifiers(1 << 1);
    /// <kbd>Shift</kbd>.
    pub const SHIFT: Modifiers = Modifiers(1 << 2);
    /// The <kbd>Win</kbd> / <kbd>Super</kbd> / <kbd>Meta</kbd> key.
    pub const WIN: Modifiers = Modifiers(1 << 3);

    /// The set of modifiers with no modifiers held.
    pub const fn new() -> Modifiers {
        Modifiers::NONE
    }

    /// The union of two modifier sets.
    pub const fn union(self, other: Modifiers) -> Modifiers {
        Modifiers(self.0 | other.0)
    }

    /// Whether every modifier in `other` is held in `self`.
    ///
    /// Note this is "contains all of", not "equals".
    pub const fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no modifier is held.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// A non-modifier key that can appear in a global hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A single printable character, `'a'..='z'`, `'0'..='9'`, or punctuation.
    Char(char),
    /// A function key, <kbd>F1</kbd> through <kbd>F24</kbd>.
    Function(u8),
    /// <kbd>Space</kbd>.
    Space,
    /// <kbd>Enter</kbd> / <kbd>Return</kbd>.
    Enter,
    /// <kbd>Tab</kbd>.
    Tab,
    /// <kbd>Esc</kbd>.
    Escape,
    /// <kbd>Backspace</kbd>.
    Backspace,
    /// <kbd>Del</kbd>.
    Delete,
}

/// A modifier-key combination plus one non-modifier key.
///
/// This is a value, not a registration: constructing one is free and
/// side-effect free. Registering it is [`GlobalHotkey::register`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hotkey {
    /// Modifiers that must be held.
    pub modifiers: Modifiers,
    /// The non-modifier key that must be pressed.
    pub key: Key,
}

impl Hotkey {
    /// Parses a human-written hotkey spec such as `"Ctrl+Shift+Space"`,
    /// `"Alt+F4"`, or `"Win+K"`.
    ///
    /// Modifier names are case-insensitive and matched on `+` boundaries, so
    /// order does not matter. Fails when the spec names no key, names more
    /// than one key, contains an empty segment, or asks for a combination
    /// Windows cannot bind globally.
    pub fn parse(spec: &str) -> Result<Hotkey, HotkeyParseError> {
        let mut modifiers = Modifiers::NONE;
        let mut key: Option<Key> = None;

        for segment in spec.split('+') {
            let token = segment.trim();
            if token.is_empty() {
                return Err(HotkeyParseError::EmptySegment);
            }
            match token.to_ascii_lowercase().as_str() {
                "alt" => modifiers = modifiers.union(Modifiers::ALT),
                "ctrl" | "control" => modifiers = modifiers.union(Modifiers::CTRL),
                "shift" => modifiers = modifiers.union(Modifiers::SHIFT),
                "win" | "meta" | "super" => modifiers = modifiers.union(Modifiers::WIN),
                _ => {
                    if key.is_some() {
                        return Err(HotkeyParseError::MultipleKeys);
                    }
                    key = Some(parse_key(&token.to_ascii_lowercase())?);
                }
            }
        }

        let key = key.ok_or(HotkeyParseError::MissingKey)?;
        // Windows cannot bind Alt+Ctrl globally; several combinations are also
        // reserved by the system. Rejecting known-unsupported pairs here keeps
        // the failure at config-parse time instead of at registration time.
        let unbound = Modifiers::ALT.union(Modifiers::CTRL);
        if modifiers.contains(unbound) {
            return Err(HotkeyParseError::UnsupportedCombination);
        }
        if modifiers.is_empty() && matches!(key, Key::Char(c) if c.is_alphanumeric()) {
            return Err(HotkeyParseError::UnsupportedCombination);
        }

        Ok(Hotkey { modifiers, key })
    }
}

/// A hotkey spec string could not be turned into a [`Hotkey`].
///
/// Raised at config-parse time, so a bad hotkey in a config file surfaces when
/// the file is read rather than minutes later when registration silently fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyParseError {
    /// The spec had no non-modifier key, e.g. `"Ctrl"`.
    MissingKey,
    /// The spec named more than one non-modifier key, e.g. `"Ctrl+K+L"`.
    MultipleKeys,
    /// The spec had an empty `+`-delimited segment, e.g. `"Ctrl+"`.
    EmptySegment,
    /// The spec named something that is not a key, e.g. `"Ctrl+Nonsense"`.
    UnknownKey(String),
    /// The spec parsed, but Windows cannot bind it system-wide.
    UnsupportedCombination,
}

impl fmt::Display for HotkeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotkeyParseError::MissingKey => write!(f, "hotkey spec names no key"),
            HotkeyParseError::MultipleKeys => write!(f, "hotkey spec names more than one key"),
            HotkeyParseError::EmptySegment => write!(f, "hotkey spec has an empty segment"),
            HotkeyParseError::UnknownKey(key) => write!(f, "{key:?} is not a known key"),
            HotkeyParseError::UnsupportedCombination => {
                write!(f, "this key combination cannot be bound system-wide")
            }
        }
    }
}

impl Error for HotkeyParseError {}

/// Maps one already-lowercased token to a [`Key`].
fn parse_key(token: &str) -> Result<Key, HotkeyParseError> {
    match token {
        "space" => return Ok(Key::Space),
        "enter" | "return" => return Ok(Key::Enter),
        "tab" => return Ok(Key::Tab),
        "esc" | "escape" => return Ok(Key::Escape),
        "backspace" => return Ok(Key::Backspace),
        "del" | "delete" => return Ok(Key::Delete),
        _ => {}
    }

    if let Some(digits) = token.strip_prefix('f') {
        let index: u8 = digits
            .parse()
            .map_err(|_| HotkeyParseError::UnknownKey(token.to_owned()))?;
        if (1..=24).contains(&index) {
            return Ok(Key::Function(index));
        }
        return Err(HotkeyParseError::UnknownKey(token.to_owned()));
    }

    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        (Some(character), None) => Ok(Key::Char(character)),
        _ => Err(HotkeyParseError::UnknownKey(token.to_owned())),
    }
}

// ---------------------------------------------------------------------------
// Global hotkey
// ---------------------------------------------------------------------------

/// Something went wrong binding or unbinding a system-wide hotkey.
///
/// Failures are reported, never swallowed. A [`HotkeyError::NotImplemented`]
/// means the seam exists and the work has not been done — it is not a
/// placeholder for a code path that should have succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyError {
    /// The Win32 implementation has not been written yet.
    NotImplemented {
        /// The operation that was attempted, for the log line.
        operation: &'static str,
    },
    /// Windows refused the hotkey; another application already owns it.
    AlreadyTaken,
    /// [`GlobalHotkey::unregister`] was called with nothing registered.
    NotRegistered,
    /// Windows reported a failure for a reason we do not model.
    SystemFailure {
        /// The raw `GetLastError()` value, or `0` if unavailable.
        code: u32,
    },
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotkeyError::NotImplemented { operation } => write!(
                f,
                "{operation} is not implemented yet (orca-win is a stub; no Win32 call was made)"
            ),
            HotkeyError::AlreadyTaken => {
                write!(f, "hotkey is already owned by another application")
            }
            HotkeyError::NotRegistered => write!(f, "no hotkey is registered"),
            HotkeyError::SystemFailure { code } => {
                write!(f, "hotkey call failed (Win32 error {code})")
            }
        }
    }
}

impl Error for HotkeyError {}

/// A system-wide hotkey: pressed while *any* application has focus.
///
/// `Send` because a real implementation owns a dedicated thread running a
/// message loop, and the owner of the handle is not the UI thread.
pub trait GlobalHotkey: Send {
    /// Binds `hotkey` system-wide. On success, the handler is invoked on every
    /// press until [`GlobalHotkey::unregister`].
    fn register(&mut self, hotkey: &Hotkey) -> Result<(), HotkeyError>;

    /// Releases the hotkey, if one is bound. Idempotent in spirit: calling it
    /// without a registration is reported as [`HotkeyError::NotRegistered`]
    /// rather than treated as a no-op success.
    fn unregister(&mut self) -> Result<(), HotkeyError>;

    /// Whether a hotkey is currently bound.
    fn is_registered(&self) -> bool;
}

/// The real Win32 implementation. **Stub: performs no Win32 call.**
///
/// Constructing it is fine and side-effect free. Every method that would need
/// the Win32 API returns [`HotkeyError::NotImplemented`], so a caller that
/// ignores the error still ends up with `is_registered() == false` — it cannot
/// mistake this for a working hotkey.
#[derive(Debug, Default)]
pub struct Win32GlobalHotkey {
    registered: Option<Hotkey>,
}

impl Win32GlobalHotkey {
    /// Creates an unbound hotkey registrar.
    pub fn new() -> Win32GlobalHotkey {
        Win32GlobalHotkey { registered: None }
    }
}

impl GlobalHotkey for Win32GlobalHotkey {
    fn register(&mut self, _hotkey: &Hotkey) -> Result<(), HotkeyError> {
        Err(HotkeyError::NotImplemented {
            operation: "RegisterHotKey",
        })
    }

    fn unregister(&mut self) -> Result<(), HotkeyError> {
        Err(HotkeyError::NotImplemented {
            operation: "UnregisterHotKey",
        })
    }

    fn is_registered(&self) -> bool {
        // Always false: this stub never records a registration, so a caller
        // that discards errors still sees an accurate answer.
        self.registered.is_some()
    }
}

// ---------------------------------------------------------------------------
// Single instance
// ---------------------------------------------------------------------------

/// Something went wrong establishing whether this process is the primary one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingleInstanceError {
    /// The Win32 implementation has not been written yet.
    NotImplemented {
        /// The operation that was attempted, for the log line.
        operation: &'static str,
    },
    /// Another process holds the lock and did not answer us.
    AlreadyRunning,
    /// Windows reported a failure for a reason we do not model.
    SystemFailure {
        /// The raw `GetLastError()` value, or `0` if unavailable.
        code: u32,
    },
}

impl fmt::Display for SingleInstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SingleInstanceError::NotImplemented { operation } => write!(
                f,
                "{operation} is not implemented yet (orca-win is a stub; no Win32 call was made)"
            ),
            SingleInstanceError::AlreadyRunning => write!(f, "another orca instance is running"),
            SingleInstanceError::SystemFailure { code } => {
                write!(f, "single-instance call failed (Win32 error {code})")
            }
        }
    }
}

impl Error for SingleInstanceError {}

/// Guards against a second copy of orca fighting over the same global hotkey,
/// tray icon, and index database.
///
/// The launcher should show its window and exit rather than run twice, so the
/// primary instance has to be decided before any global resource is claimed.
pub trait SingleInstance: Send {
    /// Attempts to become the primary instance.
    ///
    /// `Ok(true)` means this process is primary and should continue starting
    /// up. `Ok(false)` means another instance is primary and this process
    /// should signal it and exit.
    fn try_acquire(&mut self) -> Result<bool, SingleInstanceError>;

    /// Whether this process currently holds the primary role.
    fn is_primary(&self) -> bool;

    /// Gives up the primary role, if held.
    fn release(&mut self) -> Result<(), SingleInstanceError>;
}

/// The real Win32 implementation. **Stub: performs no Win32 call.**
///
/// A real implementation will use a named mutex, or a window class plus
/// `FindWindow`/atomically-registered message broadcast so the second
/// instance can forward its arguments to the first.
#[derive(Debug, Default)]
pub struct Win32SingleInstance {
    primary: bool,
}

impl Win32SingleInstance {
    /// Creates a single-instance guard that has not yet claimed the role.
    pub fn new() -> Win32SingleInstance {
        Win32SingleInstance { primary: false }
    }
}

impl SingleInstance for Win32SingleInstance {
    fn try_acquire(&mut self) -> Result<bool, SingleInstanceError> {
        Err(SingleInstanceError::NotImplemented {
            operation: "CreateMutexW / FindWindowW",
        })
    }

    fn is_primary(&self) -> bool {
        self.primary
    }

    fn release(&mut self) -> Result<(), SingleInstanceError> {
        Err(SingleInstanceError::NotImplemented {
            operation: "ReleaseMutex / CloseHandle",
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn err(result: Result<Hotkey, HotkeyParseError>) -> HotkeyParseError {
        result.expect_err("expected a parse error")
    }

    #[test]
    fn parses_a_typical_launcher_hotkey() {
        let hotkey = Hotkey::parse("Ctrl+Shift+Space").expect("should parse");
        assert_eq!(hotkey.key, Key::Space);
        assert!(hotkey.modifiers.contains(Modifiers::CTRL));
        assert!(hotkey.modifiers.contains(Modifiers::SHIFT));
        assert!(!hotkey.modifiers.contains(Modifiers::ALT));
    }

    #[test]
    fn parse_is_case_and_order_insensitive() {
        // Ctrl+Shift+K, not Ctrl+Alt+K: Alt+Ctrl is rejected as unbindable.
        let expected = Hotkey::parse("Ctrl+Shift+K").expect("should parse");
        assert_eq!(
            Hotkey::parse("ctrl+shift+k").expect("should parse"),
            expected
        );
        assert_eq!(
            Hotkey::parse("SHIFT+CTRL+k").expect("should parse"),
            expected
        );
        assert_eq!(
            Hotkey::parse("  Ctrl + Shift + K  ").expect("should parse"),
            expected
        );
    }

    #[test]
    fn parses_function_keys_and_aliases() {
        assert_eq!(
            Hotkey::parse("Win+F5").expect("should parse").key,
            Key::Function(5)
        );
        assert_eq!(
            Hotkey::parse("Ctrl+Escape").expect("should parse").key,
            Key::Escape
        );
        assert_eq!(
            Hotkey::parse("Alt+Del").expect("should parse").key,
            Key::Delete
        );
    }

    #[test]
    fn rejects_malformed_specs() {
        assert_eq!(err(Hotkey::parse("Ctrl")), HotkeyParseError::MissingKey);
        assert_eq!(err(Hotkey::parse("")), HotkeyParseError::EmptySegment);
        assert_eq!(err(Hotkey::parse("Ctrl+")), HotkeyParseError::EmptySegment);
        assert_eq!(
            err(Hotkey::parse("Ctrl+K+L")),
            HotkeyParseError::MultipleKeys
        );
        assert_eq!(
            err(Hotkey::parse("Ctrl+F99")),
            HotkeyParseError::UnknownKey("f99".into())
        );
        assert_eq!(
            err(Hotkey::parse("Ctrl+Nonsense")),
            HotkeyParseError::UnknownKey("nonsense".into())
        );
    }

    #[test]
    fn rejects_combinations_windows_cannot_bind_globally() {
        assert_eq!(
            err(Hotkey::parse("Ctrl+Alt+K")),
            HotkeyParseError::UnsupportedCombination
        );
        // A bare alphanumeric key would swallow normal typing globally.
        assert_eq!(
            err(Hotkey::parse("K")),
            HotkeyParseError::UnsupportedCombination
        );
    }

    #[test]
    fn modifiers_union_contains_and_emptiness() {
        let combo = Modifiers::CTRL.union(Modifiers::SHIFT);

        assert!(Modifiers::NONE.is_empty());
        assert!(!combo.is_empty());
        assert!(combo.contains(Modifiers::CTRL));
        assert!(combo.contains(Modifiers::CTRL.union(Modifiers::SHIFT)));
        assert!(!combo.contains(Modifiers::WIN));
        assert!(!combo.contains(Modifiers::ALT));
    }

    #[test]
    fn the_win32_hotkey_stub_reports_failure_rather_than_faking_success() {
        let mut hotkey = Win32GlobalHotkey::new();
        let spec = Hotkey::parse("Ctrl+Shift+Space").expect("should parse");

        assert!(!hotkey.is_registered());
        assert_eq!(
            hotkey.register(&spec),
            Err(HotkeyError::NotImplemented {
                operation: "RegisterHotKey"
            })
        );
        // Ignoring the error must still leave us knowing nothing is bound.
        assert!(!hotkey.is_registered());
        assert_eq!(
            hotkey.unregister(),
            Err(HotkeyError::NotImplemented {
                operation: "UnregisterHotKey"
            })
        );
    }

    #[test]
    fn the_win32_single_instance_stub_reports_failure() {
        let mut guard = Win32SingleInstance::new();

        assert!(!guard.is_primary());
        assert_eq!(
            guard.try_acquire(),
            Err(SingleInstanceError::NotImplemented {
                operation: "CreateMutexW / FindWindowW"
            })
        );
        assert!(!guard.is_primary());
    }

    #[test]
    fn stub_errors_explain_themselves() {
        let message = HotkeyError::NotImplemented {
            operation: "RegisterHotKey",
        }
        .to_string();
        assert!(message.contains("RegisterHotKey"), "{message}");
        assert!(message.contains("not implemented"), "{message}");
    }
}
