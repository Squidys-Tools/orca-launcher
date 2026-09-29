//! The `hotkey = "..."` value in `config.toml`.
//!
//! # What this validates, and what it deliberately does not
//!
//! This type checks *syntax* and nothing else: exactly one non-modifier key,
//! known modifier names, no empty `+` segments. It does **not** check whether
//! Windows can bind the combination, because that is a Win32 fact and
//! `orca-win` owns the Win32 facts. `orca-win::Hotkey::parse` already enforces
//! `Ctrl+Alt` being unbindable and bare alphanumeric keys being unacceptable,
//! and duplicating those rules here would be a second copy that drifts.
//!
//! So the flow is: config parses and canonicalises the *string*, then
//! `orca-win` decides whether it can be registered. A bad-but-well-formed spec
//! therefore fails at registration, with `orca-win`'s error, rather than at
//! config load.
//!
//! ```
//! use orca_core::config::HotkeySpec;
//!
//! // Canonicalised: modifiers in a fixed order, keys uppercased.
//! assert_eq!(
//!     HotkeySpec::parse("shift+ctrl+k").expect("parses").as_str(),
//!     "Ctrl+Shift+K"
//! );
//! assert_eq!(
//!     HotkeySpec::parse("Ctrl+Alt+Space").expect("parses").as_str(),
//!     "Ctrl+Alt+Space"
//! );
//! // Syntax errors are values with an explanation, not panics.
//! assert!(HotkeySpec::parse("Ctrl+").is_err());
//! assert!(HotkeySpec::parse("Ctrl+A+B").is_err());
//! ```

use std::fmt;

use serde::{Deserialize, Serialize};

/// The compiled-in default, matching what the launcher has always used.
pub const DEFAULT_HOTKEY_SPEC: &str = "Ctrl+Shift+Space";

/// A validated hotkey spec string, stored canonically.
///
/// Canonical form is modifiers in the order `Ctrl`, `Alt`, `Shift`, `Win`,
/// followed by the key. Two specs that mean the same thing therefore compare
/// and serialise identically, which is what makes a config file stable when it
/// is rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct HotkeySpec(String);

impl HotkeySpec {
    /// The spec used when the config file does not say.
    pub const DEFAULT_SPEC: &'static str = DEFAULT_HOTKEY_SPEC;

    /// Parses and canonicalises a spec.
    ///
    /// Accepts `"Ctrl+Shift+Space"`, `"alt+f4"`, `"Win+K"`, and any ordering
    /// or casing of those. Rejects a spec that names no key, more than one
    /// key, has an empty segment, or uses a name this crate does not
    /// recognise.
    pub fn parse(spec: &str) -> Result<HotkeySpec, HotkeySpecError> {
        if spec.trim().is_empty() {
            return Err(HotkeySpecError::Empty);
        }

        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut win = false;
        let mut key: Option<String> = None;

        for segment in spec.split('+') {
            let token = segment.trim();
            if token.is_empty() {
                return Err(HotkeySpecError::EmptySegment);
            }
            match token.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "ctl" => ctrl = true,
                "alt" | "option" => alt = true,
                "shift" => shift = true,
                "win" | "meta" | "super" | "cmd" => win = true,
                _ => {
                    if key.is_some() {
                        return Err(HotkeySpecError::MultipleKeys);
                    }
                    key = Some(canonical_key(token)?);
                }
            }
        }

        let key = key.ok_or(HotkeySpecError::MissingKey)?;
        let mut canonical = String::new();
        for (present, name) in [(ctrl, "Ctrl"), (alt, "Alt"), (shift, "Shift"), (win, "Win")] {
            if present {
                if !canonical.is_empty() {
                    canonical.push('+');
                }
                canonical.push_str(name);
            }
        }
        if !canonical.is_empty() {
            canonical.push('+');
        }
        canonical.push_str(&key);
        Ok(HotkeySpec(canonical))
    }

    /// The canonical spec, ready to hand to `orca_win::Hotkey::parse`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether any modifier is present. A bare key is syntactically valid here
    /// and rejected by `orca-win`; see the module docs.
    #[must_use]
    pub fn has_modifier(&self) -> bool {
        self.0.contains('+')
    }
}

impl fmt::Display for HotkeySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for HotkeySpec {
    type Err = HotkeySpecError;

    fn from_str(value: &str) -> Result<HotkeySpec, HotkeySpecError> {
        HotkeySpec::parse(value)
    }
}

impl TryFrom<String> for HotkeySpec {
    type Error = HotkeySpecError;

    fn try_from(value: String) -> Result<HotkeySpec, HotkeySpecError> {
        HotkeySpec::parse(&value)
    }
}

/// A hotkey spec could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeySpecError {
    /// The spec was empty or all whitespace.
    Empty,
    /// A `+`-delimited segment was empty, e.g. `"Ctrl+"`.
    EmptySegment,
    /// More than one non-modifier key, e.g. `"Ctrl+K+L"`.
    MultipleKeys,
    /// No non-modifier key at all, e.g. `"Ctrl+Shift"`.
    MissingKey,
    /// A key name this crate does not recognise, e.g. `"Ctrl+Nonsense"`.
    UnknownKey(String),
}

impl HotkeySpecError {
    /// A message that names the field, says what is wrong, and shows a fix.
    ///
    /// Written to be pasted into a log and be enough on its own, because the
    /// caller usually only has a config file and this string. Every variant
    /// leads with the word `hotkey` so the message is greppable and so a user
    /// scanning a list of config errors knows which key to look at.
    #[must_use]
    pub fn message(&self) -> String {
        let fix = format!("write it as \"{DEFAULT_HOTKEY_SPEC}\"");
        match self {
            HotkeySpecError::Empty => {
                format!("hotkey is empty; {fix}")
            }
            HotkeySpecError::EmptySegment => {
                format!("hotkey has an empty \"+\" segment; {fix}")
            }
            HotkeySpecError::MultipleKeys => {
                format!(
                    "hotkey names more than one key; a global hotkey is one key plus modifiers; {fix}"
                )
            }
            HotkeySpecError::MissingKey => {
                format!("hotkey has modifiers but no key; {fix}")
            }
            HotkeySpecError::UnknownKey(key) => {
                format!("hotkey key {key:?} is not one this launcher knows; {fix}")
            }
        }
    }
}

impl fmt::Display for HotkeySpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for HotkeySpecError {}

/// Canonicalises one non-modifier token.
///
/// Named keys get a fixed spelling; a single character is uppercased. Anything
/// longer is a typo, not a key name — accepting it would let `Nonsense` become
/// a "hotkey" that fails silently at registration time instead of here.
fn canonical_key(token: &str) -> Result<String, HotkeySpecError> {
    let lowercase = token.to_ascii_lowercase();
    // Named keys and punctuation keys, canonicalised to the spelling orca-win
    // understands. Arrow names are listed too, so `Up` does not fall through to
    // the single-character path and become "U" + "P".
    let named = match lowercase.as_str() {
        "space" | "spacebar" => Some("Space"),
        "enter" | "return" => Some("Enter"),
        "tab" => Some("Tab"),
        "esc" | "escape" => Some("Escape"),
        "backspace" | "bksp" => Some("Backspace"),
        "del" | "delete" => Some("Delete"),
        "up" => Some("Up"),
        "down" => Some("Down"),
        "left" => Some("Left"),
        "right" => Some("Right"),
        _ => None,
    };
    if let Some(named) = named {
        return Ok(named.to_owned());
    }

    // Function keys: F1..=F24, matching what orca-win accepts.
    if lowercase.starts_with('f') {
        return match lowercase
            .strip_prefix('f')
            .and_then(|d| d.parse::<u8>().ok())
        {
            Some(index) if (1..=24).contains(&index) => Ok(format!("F{index}")),
            _ => Err(HotkeySpecError::UnknownKey(lowercase)),
        };
    }

    let mut chars = lowercase.chars();
    match (chars.next(), chars.next()) {
        (Some(character), None) => Ok(character.to_ascii_uppercase().to_string()),
        _ => Err(HotkeySpecError::UnknownKey(lowercase)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(spec: &str) -> String {
        HotkeySpec::parse(spec)
            .unwrap_or_else(|error| panic!("{spec:?} should parse: {error}"))
            .as_str()
            .to_owned()
    }

    fn error(spec: &str) -> HotkeySpecError {
        HotkeySpec::parse(spec).expect_err("expected a parse error")
    }

    #[test]
    fn canonicalises_modifier_order_case_and_spacing() {
        assert_eq!(canonical("Ctrl+Shift+Space"), "Ctrl+Shift+Space");
        assert_eq!(canonical("shift+ctrl+k"), "Ctrl+Shift+K");
        assert_eq!(canonical("  SHIFT  +  CTRL  +  k  "), "Ctrl+Shift+K");
        assert_eq!(canonical("Ctrl+Alt+Space"), "Ctrl+Alt+Space");
        assert_eq!(canonical("Win+K"), "Win+K");
        assert_eq!(canonical("Meta+K"), "Win+K");
        assert_eq!(canonical("Super+K"), "Win+K");
        assert_eq!(canonical("Cmd+K"), "Win+K");
        assert_eq!(canonical("Control+Alt+Delete"), "Ctrl+Alt+Delete");
        assert_eq!(canonical("Option+Q"), "Alt+Q");
    }

    #[test]
    fn repeated_modifiers_collapse() {
        // A user typing Ctrl twice means Ctrl, not an error.
        assert_eq!(canonical("Ctrl+Ctrl+K"), "Ctrl+K");
    }

    #[test]
    fn named_keys_get_a_fixed_spelling() {
        assert_eq!(canonical("Ctrl+Space"), "Ctrl+Space");
        assert_eq!(canonical("Ctrl+Spacebar"), "Ctrl+Space");
        assert_eq!(canonical("Ctrl+Return"), "Ctrl+Enter");
        assert_eq!(canonical("Ctrl+Esc"), "Ctrl+Escape");
        assert_eq!(canonical("Ctrl+Del"), "Ctrl+Delete");
        assert_eq!(canonical("Ctrl+Bksp"), "Ctrl+Backspace");
        assert_eq!(canonical("Ctrl+Up"), "Ctrl+Up");
        assert_eq!(canonical("Ctrl+Right"), "Ctrl+Right");
    }

    #[test]
    fn function_keys_are_range_checked() {
        assert_eq!(canonical("Alt+F1"), "Alt+F1");
        assert_eq!(canonical("Alt+F24"), "Alt+F24");
        assert_eq!(canonical("Alt+f5"), "Alt+F5");
        assert_eq!(error("Alt+F0"), HotkeySpecError::UnknownKey("f0".into()));
        assert_eq!(error("Alt+F25"), HotkeySpecError::UnknownKey("f25".into()));
        assert_eq!(error("Alt+Fx"), HotkeySpecError::UnknownKey("fx".into()));
    }

    #[test]
    fn a_bare_key_is_syntactically_valid_even_though_windows_will_refuse_it() {
        // Documented division of labour: syntax here, bindability in orca-win.
        // `Ctrl+Alt+K` is also accepted here and refused there.
        let spec = HotkeySpec::parse("K").expect("syntactically fine");
        assert_eq!(spec.as_str(), "K");
        assert!(!spec.has_modifier());
        assert!(HotkeySpec::parse("Ctrl+Alt+K").is_ok());
    }

    #[test]
    fn rejects_malformed_specs_with_specific_variants() {
        assert_eq!(error(""), HotkeySpecError::Empty);
        assert_eq!(error("   "), HotkeySpecError::Empty);
        assert_eq!(error("Ctrl+"), HotkeySpecError::EmptySegment);
        assert_eq!(error("+K"), HotkeySpecError::EmptySegment);
        assert_eq!(error("Ctrl++K"), HotkeySpecError::EmptySegment);
        assert_eq!(error("Ctrl+A+B"), HotkeySpecError::MultipleKeys);
        assert_eq!(error("Ctrl+Shift"), HotkeySpecError::MissingKey);
        assert_eq!(
            error("Nonsense"),
            HotkeySpecError::UnknownKey("nonsense".into())
        );
    }

    #[test]
    fn error_messages_name_the_field_and_show_a_fix() {
        for spec in ["", "Ctrl+", "Ctrl+A+B", "Ctrl", "Ctrl+Nonsense"] {
            let message = error(spec).to_string();
            assert!(message.contains("hotkey"), "{spec:?}: {message}");
            assert!(message.contains(DEFAULT_HOTKEY_SPEC), "{spec:?}: {message}");
        }
    }

    #[test]
    fn canonicalisation_is_idempotent() {
        for spec in ["ctrl+shift+space", "win+k", "alt+enter", "f7"] {
            let once = canonical(spec);
            let twice = canonical(&once);
            assert_eq!(once, twice, "{spec:?} did not settle");
        }
    }

    /// A bare newtype deserialises as a *table* named `hotkey`, so serde
    /// round-trips have to go through a struct that owns the key. That is also
    /// how it is used in practice: `Config::general.hotkey`.
    #[derive(Debug, serde::Deserialize, serde::Serialize)]
    struct Holder {
        hotkey: HotkeySpec,
    }

    #[test]
    fn display_and_serde_round_trip_the_canonical_form() {
        let spec = HotkeySpec::parse("shift+ctrl+space").expect("parses");
        assert_eq!(spec.to_string(), "Ctrl+Shift+Space");

        let parsed: Holder =
            toml::from_str("hotkey = \"alt+space\"\n").expect("should deserialise");
        assert_eq!(parsed.hotkey.as_str(), "Alt+Space");

        let rendered = toml::to_string(&parsed).expect("should serialise");
        assert!(rendered.contains("Alt+Space"), "{rendered}");
        // And what comes back out is what went in.
        let round_tripped: Holder = toml::from_str(&rendered).expect("should re-deserialise");
        assert_eq!(round_tripped.hotkey.as_str(), "Alt+Space");
    }

    #[test]
    fn a_bad_serde_value_is_an_error_rather_than_a_panic() {
        let error = toml::from_str::<Holder>("hotkey = \"Ctrl+\"\n").expect_err("must fail");
        assert!(error.to_string().contains("hotkey"), "{error}");
        assert!(error.to_string().contains(DEFAULT_HOTKEY_SPEC), "{error}");
    }

    #[test]
    fn the_compiled_in_default_parses() {
        assert_eq!(canonical(HotkeySpec::DEFAULT_SPEC), DEFAULT_HOTKEY_SPEC);
    }
}
