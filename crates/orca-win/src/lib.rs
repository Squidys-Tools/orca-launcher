//! `orca-win` — Windows platform integration for orca.
//!
//! Everything in this crate talks to the operating system: binding a global
//! hotkey, deciding whether this process is the primary instance, toggling
//! autostart, the tray icon, enumerating installed applications, raising a
//! window to the front, and putting text on the clipboard. The GPUI binary
//! talks to this crate; this crate never talks to GPUI.
//!
//! # The seams
//!
//! The two operations that claim exclusive global state — a system-wide
//! hotkey and a `HKCU` registry value — sit behind traits ([`HotkeyBackend`]
//! and [`RunValueStore`]). Everything above them is a state machine that can be
//! driven by a fake, which is what makes the interesting behaviour testable:
//! the tests assert that a refused hotkey leaves `is_registered()` false, that
//! a rejected autostart path leaves the previous entry intact, and that a
//! command really does travel from a second launch to the primary over a named
//! pipe.
//!
//! What a fake cannot prove is that Windows delivers a keypress or that Explorer
//! runs a Run value. Those are recorded in `docs/ARCHITECTURE.md` from the
//! probe; this crate does not pretend to re-establish them.
//!
//! # Mapping onto `orca-core`
//!
//! `orca-core` is pure, so none of its types appear here. The adapter is
//! mechanical and lives in the binary:
//!
//! | `orca-win` | `orca-core` |
//! |---|---|
//! | [`Hotkey`] | the config value, parsed from a string via [`Hotkey::parse`] |
//! | [`Win32GlobalHotkey`] | the `GlobalHotkey` the app is written against |
//! | [`Win32SingleInstance`] + [`SECONDARY_INSTANCE_EXIT_CODE`] | `main`'s early exit |
//! | [`Win32Autostart`] | the autostart preference |
//! | [`TrayIcon`] | the tray menu and its actions |
//! | [`installed_apps`] | a `ResultProvider` returning `ResultItem`s with `Source::Application` |
//! | [`WindowHandle`] | whatever the UI layer produces for a window |

#![forbid(unsafe_op_in_unsafe_fn)]

use std::error::Error;
use std::fmt;

mod apps;
mod autostart;
mod clipboard;
mod foreground;
mod hotkey;
mod single_instance;
mod tray;
mod wide;

pub use apps::{
    installed_apps, start_menu_apps_in, user_file_search_roots, InstalledApp, InstalledAppSource,
    ShortcutResolver,
};
pub use autostart::{
    run_command_for, AutostartError, HkcuRunStore, RunValueStore, Win32Autostart,
    MAX_RUN_VALUE_LEN, RUN_KEY,
};
pub use clipboard::{set_clipboard_text, ClipboardError};
pub use foreground::{activate, is_foreground, ForegroundError, WindowHandle};
pub use hotkey::{
    hotkey_code, modifier_flags, virtual_key, GlobalHotkey, HotkeyBackend, HotkeyError,
    Win32GlobalHotkey, Win32HotkeyBackend,
};
pub use single_instance::{
    InstanceCommand, SingleInstanceError, Win32SingleInstance, DEFAULT_HANDSHAKE_TIMEOUT,
    DEFAULT_MUTEX_NAME, DEFAULT_PIPE_NAME, SECONDARY_INSTANCE_EXIT_CODE,
};
pub use tray::{TrayError, TrayEvent, TrayIcon, TrayIconSource, TrayMenuItem, TraySpec};

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
        // There is deliberately NO rule rejecting `Ctrl+Alt` here. An earlier
        // version of this parser refused every `Alt+Ctrl` combination on the
        // claim that Windows cannot bind one globally. That claim is false and
        // it was expensive: the probe registered `Ctrl+Alt+Space` through the
        // real `RegisterHotKey` and drove it across ten open/hide cycles, so
        // `Ctrl+Alt+Space` is the launcher's shipped default and this parser
        // used to reject its own default. `orca` then grew a second, laxer
        // parser to work around it — two parsers that could drift apart.
        //
        // Windows does reserve a small set of combinations (Ctrl+Alt+Del,
        // Win+L, Alt+Tab, F12). Those are refused by `RegisterHotKey` itself,
        // and letting the platform answer is more honest than guessing at its
        // reserved set from here.
        //
        // A bare unmodified alphanumeric key IS rejected below, because binding
        // one globally would swallow that character in every other application.
        if modifiers.is_empty() && matches!(key, Key::Char(c) if c.is_alphanumeric()) {
            return Err(HotkeyParseError::UnsupportedCombination);
        }
        // Punctuation has no layout-independent virtual-key code, so a global
        // binding for it cannot be expressed. Rejecting it here means the
        // failure is a config error, not a silent no-op at registration.
        if virtual_key(key).is_none() {
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
    fn rejects_a_bare_alphanumeric_key_but_not_ctrl_alt() {
        // A bare alphanumeric key would swallow normal typing globally.
        assert_eq!(
            err(Hotkey::parse("K")),
            HotkeyParseError::UnsupportedCombination
        );
        assert_eq!(
            err(Hotkey::parse("7")),
            HotkeyParseError::UnsupportedCombination
        );

        // `Ctrl+Alt` used to be rejected here on the claim that Windows cannot
        // bind it globally. That claim is false: the probe registered
        // `Ctrl+Alt+Space` via the real `RegisterHotKey` and drove ten
        // open/hide cycles with it, and this is the launcher's shipped default.
        // Rejecting it here made `orca` carry a second, laxer hotkey parser as a
        // workaround. Windows does reserve some combinations (Ctrl+Alt+Del,
        // Win+L, Alt+Tab, F12), but `RegisterHotKey` refuses those itself, and
        // guessing at the reserved set from the parser is what caused the bug.
        for spec in ["Ctrl+Alt+K", "Ctrl+Alt+Space", "Alt+Ctrl+K"] {
            let hotkey = Hotkey::parse(spec)
                .unwrap_or_else(|e| panic!("{spec} must be bindable, got {e:?}"));
            assert!(hotkey.modifiers.contains(Modifiers::CTRL), "{spec}");
            assert!(hotkey.modifiers.contains(Modifiers::ALT), "{spec}");
        }
    }

    #[test]
    fn rejects_a_key_with_no_virtual_key_code() {
        // `/` has no layout-independent virtual-key code, so no global binding
        // for it can be expressed. This is a fact about keyboard layouts, not
        // about the API, so it stays rejected.
        assert_eq!(
            err(Hotkey::parse("Ctrl+/")),
            HotkeyParseError::UnsupportedCombination
        );
    }

    #[test]
    fn rejects_keys_with_no_virtual_key_code() {
        // `/` means different physical keys on different layouts, so there is
        // no single VK to bind. Saying so at parse time beats a hotkey that
        // silently never fires.
        assert_eq!(
            err(Hotkey::parse("Ctrl+/")),
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
    fn parse_errors_explain_themselves() {
        let message = HotkeyParseError::MissingKey.to_string();
        assert!(message.contains("no key"), "{message}");
    }
}
