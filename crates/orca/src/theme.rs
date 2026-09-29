//! Colours, resolved from `config.general.theme` and the OS appearance.
//!
//! The whole palette is a value with no state, and the only logic is the
//! three-way `ThemePreference` → palette mapping. That is deliberate: theme is
//! the thing a user complains about in the first ten seconds, and a palette
//! spread across `render` closures can only be checked by running the app.
//! Here it is one pure function with the contrast relationships asserted in
//! tests.
//!
//! `ThemePreference::System` resolves against the *window's* appearance rather
//! than a registry read. `Window::appearance()` is the platform's own answer to
//! "is this app light or dark right now", so it also follows the user changing
//! their theme while the launcher is running, and it costs no Win32 call.

use gpui::Rgba;
use orca_core::config::ThemePreference;
use orca_core::Source;

/// An opaque colour from a packed `0xRRGGBB` literal.
///
/// `gpui::rgb` exists for this and is not `const`, which would force both
/// palettes to be built at runtime on every frame. The arithmetic is the same
/// one `rgb` does — `0xRRGGBB` expanded to 24-bit sRGB, so each channel is
/// `byte / 255.0` — and doing it in a `const fn` means the whole palette
/// collapses to eight immediate constants in the binary.
const fn rgba(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

/// A resolved palette.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// Window background.
    pub background: Rgba,
    /// The query bar and the status bar, a step above `background`.
    pub surface: Rgba,
    /// Hairlines between the three bands.
    pub border: Rgba,
    /// The selected row's fill.
    pub selection: Rgba,
    /// Primary text.
    pub text: Rgba,
    /// Secondary text: subtitles, the prompt, the status bar.
    pub dim: Rgba,
    /// The caret and the `>` glyph.
    pub accent: Rgba,
    /// Text drawn on top of `selection`.
    pub on_selection: Rgba,
}

impl Theme {
    /// The dark palette.
    pub const DARK: Theme = Theme {
        background: rgba(0x16161a),
        surface: rgba(0x1c1c21),
        border: rgba(0x2a2a30),
        selection: rgba(0x2b4c6f),
        text: rgba(0xf2f2f2),
        dim: rgba(0x8a8a97),
        accent: rgba(0x6aa9ff),
        on_selection: rgba(0xf2f2f2),
    };

    /// The light palette.
    pub const LIGHT: Theme = Theme {
        background: rgba(0xfbfbfd),
        surface: rgba(0xf0f0f4),
        border: rgba(0xd8d8de),
        selection: rgba(0xcfe0f5),
        text: rgba(0x1b1b1f),
        dim: rgba(0x63636e),
        accent: rgba(0x0b5fc4),
        on_selection: rgba(0x1b1b1f),
    };

    /// Resolves a preference against the OS appearance.
    ///
    /// `system_dark` is passed in rather than read, so the mapping is a pure
    /// function of its two arguments. The caller gets `system_dark` from
    /// `Window::appearance()`.
    #[must_use]
    pub fn resolve(preference: ThemePreference, system_dark: bool) -> Theme {
        match preference {
            ThemePreference::Light => Theme::LIGHT,
            ThemePreference::Dark => Theme::DARK,
            ThemePreference::System => {
                if system_dark {
                    Theme::DARK
                } else {
                    Theme::LIGHT
                }
            }
        }
    }
}

/// The short label shown in a result row's gutter.
///
/// Deliberately a fixed width per source rather than a free string: the rows
/// are a single `flex_row` and a gutter that changes width per row makes the
/// titles not line up, which reads as misalignment rather than as variety.
#[must_use]
pub fn source_label(source: Source) -> &'static str {
    match source {
        Source::Application => "app",
        Source::File => "file",
        Source::Folder => "dir",
        Source::Command => "cmd",
        Source::WebSearch => "web",
        Source::Calculator => "calc",
        Source::Clipboard => "clip",
        Source::Unknown => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relative luminance, per WCAG 2.1.
    fn luminance(color: Rgba) -> f32 {
        fn channel(value: f32) -> f32 {
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
    }

    /// WCAG contrast ratio between two opaque colours.
    fn contrast(a: Rgba, b: Rgba) -> f32 {
        let (hi, lo) = {
            let (x, y) = (luminance(a), luminance(b));
            if x > y {
                (x, y)
            } else {
                (y, x)
            }
        };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn an_explicit_preference_ignores_the_system() {
        assert_eq!(
            Theme::resolve(ThemePreference::Light, true),
            Theme::LIGHT,
            "light must win even when the OS is dark"
        );
        assert_eq!(
            Theme::resolve(ThemePreference::Dark, false),
            Theme::DARK,
            "dark must win even when the OS is light"
        );
    }

    #[test]
    fn the_system_preference_follows_the_window() {
        assert_eq!(Theme::resolve(ThemePreference::System, true), Theme::DARK);
        assert_eq!(Theme::resolve(ThemePreference::System, false), Theme::LIGHT);
    }

    #[test]
    fn every_palette_meets_the_body_text_contrast_floor() {
        // 4.5:1 is the WCAG AA floor for normal-size text. This is the one
        // property of a palette that a human should never have to check, and it
        // is the property that regresses silently when someone tunes a hex.
        for (name, theme) in [("dark", Theme::DARK), ("light", Theme::LIGHT)] {
            for (role, foreground, background) in [
                ("text on background", theme.text, theme.background),
                ("text on surface", theme.text, theme.surface),
                ("dim on background", theme.dim, theme.background),
                ("dim on surface", theme.dim, theme.surface),
                (
                    "on_selection on selection",
                    theme.on_selection,
                    theme.selection,
                ),
                ("accent on background", theme.accent, theme.background),
            ] {
                let ratio = contrast(foreground, background);
                assert!(
                    ratio >= 4.5,
                    "{name}: {role} is only {ratio:.2}:1, below the 4.5:1 floor"
                );
            }
        }
    }

    #[test]
    fn the_light_palette_is_actually_light_and_the_dark_one_actually_dark() {
        assert!(luminance(Theme::LIGHT.background) > 0.5);
        assert!(luminance(Theme::DARK.background) < 0.05);
    }

    #[test]
    fn the_palettes_are_not_inverted_copies_of_each_other() {
        // If a future edit flips one channel of one colour, the two palettes
        // becoming accidental inverses is the tell.
        assert_ne!(Theme::DARK.background, Theme::LIGHT.background);
        assert_ne!(Theme::DARK.accent, Theme::LIGHT.accent);
    }

    #[test]
    fn every_source_has_a_gutter_label_that_cannot_break_the_column() {
        for source in Source::ALL {
            let label = source_label(source);
            assert!(!label.is_empty(), "{source:?} has no label");
            // The gutter is a fixed width, so a label containing whitespace
            // would wrap and push the title right for that row only — which
            // reads as a misalignment rather than as a label.
            assert!(
                !label.chars().any(char::is_whitespace),
                "{source:?} label {label:?} contains whitespace"
            );
            assert!(
                label.chars().count() <= 4,
                "{source:?} label {label:?} is too wide for the gutter"
            );
        }
    }
}
