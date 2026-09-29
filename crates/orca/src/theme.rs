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
//!
//! # The panel is translucent, and that changes what "contrast" means
//!
//! The popup is a rounded panel floating over the desktop, so the background
//! is an alpha-blended fill rather than an opaque colour. A contrast check
//! against the fill's own RGB is then meaningless: the colour the user actually
//! sees is that fill composited over whatever is behind the window, and a
//! `rgba(10, 10, 13, 0.88)` panel over a white wallpaper is a mid grey, not
//! black.
//!
//! So the tests composite: they carry their own source-over, which is the same
//! maths the renderer's blend state does, and every contrast assertion is made
//! against the *worst* of the two extreme backdrops — pure white and pure
//! black — because the desktop behind the launcher is not under our control and
//! could be either. That is what sets the panel's alpha: raise it and
//! translucency is lost, lower it and dim text stops clearing 4.5:1 over a
//! light wallpaper.

use gpui::{hsla, px, BoxShadow, Rgba};
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
    rgba_alpha(hex, 1.0)
}

/// A translucent colour from a packed `0xRRGGBB` literal and an alpha.
///
/// Kept next to [`rgba`] rather than inlined at each call site so that every
/// translucent entry in the palette visibly carries its alpha, which is the
/// number that decides whether text on it stays readable.
const fn rgba_alpha(hex: u32, alpha: f32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: alpha,
    }
}

/// A resolved palette.
///
/// Every field except the text colours is translucent, because all of them are
/// painted over the desktop rather than over another palette entry. The text
/// colours are opaque because a translucent glyph over a translucent panel is
/// two roundings away from invisible.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// The panel fill, alpha-blended over the desktop.
    pub background: Rgba,
    /// The footer pills, a step above `background`.
    pub surface: Rgba,
    /// The hairline around the panel and under the footer.
    pub border: Rgba,
    /// The selected row's fill.
    pub selection: Rgba,
    /// Primary text.
    pub text: Rgba,
    /// Secondary text: the per-row source name, the search placeholder, the
    /// status line.
    pub dim: Rgba,
    /// The caret and the search glyph.
    pub accent: Rgba,
    /// Text drawn on top of `selection`.
    pub on_selection: Rgba,
}

impl Theme {
    /// The dark palette.
    ///
    /// The panel is nearly opaque on purpose. See the module docs: the alpha is
    /// the number that keeps `dim` above 4.5:1 when the launcher is over a
    /// white wallpaper.
    pub const DARK: Theme = Theme {
        background: rgba_alpha(0x0a0a0d, 0.88),
        surface: rgba_alpha(0xffffff, 0.07),
        border: rgba_alpha(0xffffff, 0.11),
        selection: rgba_alpha(0xffffff, 0.10),
        text: rgba(0xf6f6f8),
        dim: rgba(0xb0b0ba),
        accent: rgba(0x5b9dff),
        on_selection: rgba(0xffffff),
    };

    /// The light palette.
    pub const LIGHT: Theme = Theme {
        background: rgba_alpha(0xf4f4f7, 0.90),
        surface: rgba_alpha(0x000000, 0.05),
        border: rgba_alpha(0x000000, 0.11),
        selection: rgba_alpha(0x000000, 0.07),
        text: rgba(0x16161a),
        dim: rgba(0x494952),
        accent: rgba(0x0a4fa8),
        on_selection: rgba(0x16161a),
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

    /// The drop shadow under the panel.
    ///
    /// Two shadows rather than one, because a single large blur reads as a grey
    /// halo: the tight one is the contact shadow that says the panel is above
    /// the desktop, and the wide one is the ambient occlusion that says it is
    /// floating. This is the reason the window is larger than the panel — the
    /// ambient shadow needs somewhere to go.
    #[must_use]
    pub fn panel_shadows(self) -> Vec<BoxShadow> {
        vec![
            BoxShadow::new(px(0.), px(2.), hsla(0.0, 0.0, 0.0, 0.44)).blur_radius(px(6.)),
            BoxShadow::new(px(0.), px(24.), hsla(0.0, 0.0, 0.0, 0.30)).blur_radius(px(56.)),
        ]
    }
}

/// The name shown at the right of a result row.
///
/// Long, human, and right-aligned, rather than the short gutter tag this used
/// to be. Two reasons: a fixed-width left gutter exists to keep titles aligned
/// across rows, and a right-aligned column needs no such reservation, so the
/// space the gutter was holding can go to the title; and the right-hand
/// position is where the reference puts it, so a row reads as
/// "what it is" then "what kind of thing it is".
#[must_use]
pub fn source_name(source: Source) -> &'static str {
    match source {
        Source::Application => "Application",
        Source::File => "File",
        Source::Folder => "Folder",
        Source::Command => "Command",
        Source::WebSearch => "Web search",
        Source::Calculator => "Calculator",
        Source::Clipboard => "Clipboard",
        Source::Unknown => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Source-over compositing: `top` drawn on `bottom`.
    ///
    /// The renderer's blend is `SRC_ALPHA` / `INV_SRC_ALPHA` with a separate
    /// `ONE` / `ONE` alpha channel, which is exactly this. It lives here rather
    /// than in the palette because its only callers are the contrast tests, and
    /// a `pub fn` in a binary crate that nothing calls is dead code the gate
    /// (correctly) refuses.
    fn composite_over(top: Rgba, bottom: Rgba) -> Rgba {
        let inverse = 1.0 - top.a;
        let alpha = top.a + bottom.a * inverse;
        if alpha <= 0.0 {
            return Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            };
        }
        let channel = |t: f32, b: f32| (t * top.a + b * bottom.a * inverse) / alpha;
        Rgba {
            r: channel(top.r, bottom.r),
            g: channel(top.g, bottom.g),
            b: channel(top.b, bottom.b),
            a: alpha,
        }
    }

    /// Relative luminance, per WCAG 2.1. Ignores alpha: every colour reaching
    /// this function has already been composited to opaque.
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

    /// The two backdrops a translucent panel can land on that matter most: a
    /// pure white wallpaper and a pure black one. Anything between them is less
    /// hostile to one palette or the other, so the worst case is always one of
    /// these two.
    const BACKDROPS: [(&str, Rgba); 2] = [("white", rgba(0xffffff)), ("black", rgba(0x000000))];

    /// The lowest contrast `foreground` reaches against the surface it is
    /// actually drawn on.
    ///
    /// `fill` is composited *on top of* the panel, which is itself composited
    /// over each extreme backdrop. The panel cannot be skipped: a selection
    /// highlight does not sit on the desktop, it sits on the panel, and a
    /// 10%-white highlight over a white wallpaper is only readable because the
    /// near-black panel is underneath it. Compositing it over the wallpaper
    /// directly scores it at 1.00:1, which is a true statement about nothing.
    fn worst_contrast(theme: Theme, foreground: Rgba, fill: Rgba) -> f32 {
        BACKDROPS
            .iter()
            .map(|(_, backdrop)| {
                let panel = composite_over(theme.background, *backdrop);
                contrast(foreground, composite_over(fill, panel))
            })
            .fold(f32::INFINITY, f32::min)
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
        //
        // Checked against the composited panel, over both extreme backdrops,
        // because the panel is translucent and the desktop behind it is not
        // ours. A palette that only passes over black is not a dark palette,
        // it is a palette that has not been over a light wallpaper yet.
        for (name, theme) in [("dark", Theme::DARK), ("light", Theme::LIGHT)] {
            for (role, foreground, fill) in [
                ("text on panel", theme.text, theme.background),
                ("dim on panel", theme.dim, theme.background),
                ("accent on panel", theme.accent, theme.background),
                ("text on selection", theme.on_selection, theme.selection),
                ("dim on selection", theme.dim, theme.selection),
            ] {
                let ratio = worst_contrast(theme, foreground, fill);
                assert!(
                    ratio >= 4.5,
                    "{name}: {role} is only {ratio:.2}:1 over the worst backdrop, \
                     below the 4.5:1 floor"
                );
            }
        }
    }

    #[test]
    fn the_panel_is_actually_translucent() {
        // The whole look depends on this. An alpha of 1.0 would still pass the
        // contrast tests and would look like the launcher did before, so the
        // property is asserted rather than assumed.
        for (name, theme) in [("dark", Theme::DARK), ("light", Theme::LIGHT)] {
            assert!(
                theme.background.a < 0.95,
                "{name}: the panel fill is opaque (alpha {})",
                theme.background.a
            );
            assert!(
                theme.background.a > 0.5,
                "{name}: the panel is too transparent to keep text readable \
                 (alpha {})",
                theme.background.a
            );
        }
    }

    #[test]
    fn the_panel_tint_still_follows_the_system_appearance() {
        // Translucency must not have flattened the palettes into each other: a
        // dark launcher over a dark desktop and a light one over a light
        // desktop are different products, and the tint is what tells them
        // apart before any text is read.
        assert!(luminance(Theme::LIGHT.background) > luminance(Theme::DARK.background));
    }

    #[test]
    fn compositing_a_translucent_colour_moves_it_toward_the_backdrop() {
        // A half-transparent black over white lands halfway to white, and over
        // black it lands on black. Both directions are the whole reason the
        // panel's alpha has to be a considered number rather than 1.0.
        //
        // Asserted on channel values rather than luminance: "halfway" is a
        // statement about the 0.5 *sRGB value*, and the WCAG luminance of a 50%
        // grey is 0.21, not 0.5. Comparing the two would be the kind of
        // plausible-looking arithmetic that makes a test pass for the wrong
        // reason.
        let half_black = rgba_alpha(0x000000, 0.5);
        let over_white = composite_over(half_black, rgba(0xffffff));
        let over_black = composite_over(half_black, rgba(0x000000));
        assert!(
            (over_white.r - 0.5).abs() < 0.001,
            "half black over white should be 0.5 grey, got {}",
            over_white.r
        );
        assert!(
            over_black.r < 0.001,
            "half black over black should be black, got {}",
            over_black.r
        );
        // Fully transparent on top leaves the backdrop untouched, which is what
        // makes the shadow margin around the panel see-through.
        let clear = composite_over(rgba_alpha(0xff0000, 0.0), rgba(0x123456));
        assert_eq!(clear.r, rgba(0x123456).r);
        assert_eq!(clear.a, 1.0, "an opaque backdrop stays opaque");
    }

    #[test]
    fn every_source_has_a_right_hand_label() {
        for source in Source::ALL {
            let name = source_name(source);
            assert!(!name.is_empty(), "{source:?} has no name");
            // It sits at the end of a fixed-height row, so a name that wraps
            // would push the row taller than its neighbours.
            assert!(
                !name.contains('\n') && !name.contains('\r'),
                "{source:?} name {name:?} contains a line break"
            );
            assert!(
                name.chars().count() <= 14,
                "{source:?} name {name:?} is too wide for one row"
            );
        }
    }

    #[test]
    fn the_shadow_has_a_tight_layer_and_a_wide_one() {
        // One blur cannot do both jobs: a wide one alone reads as a grey halo
        // rather than as a panel above a surface, and a tight one alone reads
        // as a sticker. The wide layer is also why the window is larger than
        // the panel, so the ordering of the two is the property that matters.
        let shadows = Theme::DARK.panel_shadows();
        assert_eq!(shadows.len(), 2);
        let (tight, wide) = (&shadows[0], &shadows[1]);
        assert!(
            tight.blur_radius < wide.blur_radius,
            "the contact shadow must be the tighter of the two"
        );
        assert!(wide.offset.y > tight.offset.y, "the wide shadow sits lower");
    }
}
