//! The frosted-glass backdrop behind the popup panel.
//!
//! The panel is translucent, and a translucent panel over a *detailed* desktop
//! is just a panel you cannot read: the text behind it competes with the text in
//! it. So the desktop has to be blurred, and Windows will not do it for us —
//! `docs/ARCHITECTURE.md` has the full account. This module is the third of the
//! three pieces:
//!
//! | piece | where | why there |
//! |---|---|---|
//! | reading the screen | [`orca_win::capture_screen_region`] | it is the only crate allowed to call Win32 |
//! | blurring it | [`orca_core::backdrop::soften`] | it is a total function over a byte buffer, so it is testable without a desktop |
//! | painting it | here | it is wiring between those two and the renderer |
//!
//! # It has to happen while the window is hidden
//!
//! `BitBlt` photographs the screen. Capture with the popup already up and you
//! photograph the popup. The retained-window design helps rather than hurts
//! here: the window is `SW_HIDE` between toggles, so a hide can capture the
//! desktop for the next show without sampling the popup itself.
//!
//! Captures run on `BackgroundExecutor`: the initial sample is taken before a
//! popup exists, and later samples run just after hiding it. The result is
//! installed asynchronously, so none of the capture or blur cost is paid by
//! the show path. The image is intentionally from hide time; for this launcher,
//! a slightly stale backdrop is preferable to a delayed hotkey response.

use std::sync::Arc;
use std::time::Instant;

use gpui::{Bounds, Pixels, RenderImage};
use image::{Delay, Frame, RgbaImage};

use crate::ui::{POPUP_HEIGHT, POPUP_WIDTH};

/// A blurred backdrop, ready to paint, plus what it cost to make.
pub struct Backdrop {
    /// The image, or `None` if the capture or the blur was refused.
    pub image: Option<Arc<RenderImage>>,
    /// How long capture plus blur took, logged when the background task finishes.
    pub took_ms: f64,
}

/// Captures and blurs the desktop behind the full panel.
///
/// `window_bounds` is the popup's bounds in GPUI logical pixels. It may be the
/// compact search window, but its top-left matches the expanded panel; capture
/// dimensions come from the full panel constants. `scale_factor` converts to
/// the physical pixels Win32 uses.
///
/// Returns a `Backdrop` whose `image` is `None` rather than an error: a missing
/// backdrop degrades the launcher to a flat translucent panel, which is a worse
/// look and nothing more. It must never be the reason the launcher does not
/// open.
#[must_use]
pub fn capture(window_bounds: Bounds<Pixels>, scale_factor: f32) -> Backdrop {
    let started = Instant::now();
    let (x, y, width, height) = panel_rect(window_bounds, scale_factor);

    let image = capture_region(x, y, width as u32, height as u32);

    Backdrop {
        image,
        took_ms: started.elapsed().as_secs_f64() * 1000.0,
    }
}

/// Captures a hidden popup's full panel using its physical window position and scale.
///
/// The compact search window is shorter than the expanded panel, so its measured
/// height cannot size the capture. The width identifies the monitor scale; the
/// capture then covers the full panel from the window's top-left anchor. This
/// also avoids using the cursor's current monitor to infer the capture location.
#[must_use]
pub fn capture_hidden_window(window_rect: (i32, i32, u32, u32)) -> Backdrop {
    let started = Instant::now();
    let image = panel_rect_in_window(window_rect)
        .and_then(|(x, y, width, height)| capture_region(x, y, width, height));

    Backdrop {
        image,
        took_ms: started.elapsed().as_secs_f64() * 1000.0,
    }
}

fn capture_region(x: i32, y: i32, width: u32, height: u32) -> Option<Arc<RenderImage>> {
    orca_win::capture_screen_region(x, y, width, height)
        .ok()
        .and_then(|region| orca_core::backdrop::soften(&region.bgra, region.width, region.height))
        .filter(orca_core::backdrop::Softened::is_paintable)
        .map(|softened| Arc::new(RenderImage::new([frame_of(&softened)])))
}

fn panel_rect_in_window(
    (x, y, window_width, window_height): (i32, i32, u32, u32),
) -> Option<(i32, i32, u32, u32)> {
    if window_width == 0 || window_height == 0 {
        return None;
    }

    let scale = window_width as f32 / POPUP_WIDTH;
    let height = (POPUP_HEIGHT * scale).round();
    if !scale.is_finite() || scale <= 0.0 || !height.is_finite() || height < 1.0 {
        return None;
    }

    Some((x, y, window_width, height as u32))
}

/// The screen rectangle to capture, in **physical** pixels.
///
/// Two conversions, and getting either wrong is invisible. `window_bounds` is
/// in GPUI logical pixels, and Win32 wants physical ones, so the position is
/// scaled as well as the size — scaling only the size puts the capture a third
/// of the way off at 150%. The window is panel-sized, so no inset is needed.
///
/// Extracted as its own function so the tests assert the arithmetic that
/// actually runs rather than a copy of it that can drift.
fn panel_rect(window_bounds: Bounds<Pixels>, scale_factor: f32) -> (i32, i32, i32, i32) {
    // A monitor reporting a nonsensical scale must not collapse the capture to
    // nothing: that is a hole in the middle of the panel, and a flat fill is
    // strictly better.
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let width = ((POPUP_WIDTH * scale).round() as i32).max(1);
    let height = ((POPUP_HEIGHT * scale).round() as i32).max(1);
    let x = (window_bounds.left().as_f32() * scale).round() as i32;
    let y = (window_bounds.top().as_f32() * scale).round() as i32;
    (x, y, width, height)
}

/// Wraps a softened BGRA buffer in the one type `gpui` accepts.
///
/// The buffer is BGRA and the type is called `RgbaImage`, which reads like a
/// contradiction and is not one. `gpui`'s own decoder does the same thing: it
/// decodes to RGBA and then swaps R and B before storing, so a `RenderImage`'s
/// backing store has always been BGRA regardless of what the type is called.
/// Passing our GDI bytes straight through is therefore correct, and swapping
/// them "to match the type name" would render the panel with its red and blue
/// channels exchanged — a green desktop and a magenta tint.
fn frame_of(softened: &orca_core::backdrop::Softened) -> Frame {
    let buffer = RgbaImage::from_raw(softened.width, softened.height, softened.bgra.clone())
        .expect("soften() already checked the buffer length matches the dimensions");
    Frame::from_parts(buffer, 0, 0, Delay::from_numer_denom_ms(100, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn window_at(x: f32, y: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(660.), px(430.)))
    }

    #[test]
    fn the_capture_matches_the_window() {
        let (x, y, w, h) = panel_rect(window_at(100., 200.), 1.0);
        assert_eq!((x, y, w, h), (100, 200, 660, 430));
    }

    #[test]
    fn position_and_size_both_scale_at_150() {
        // The failure this is here for: scaling only the *size* and adding an
        // unscaled inset looks almost right at 100% and puts the capture well
        // off at 150%, which reads as nothing more than "the blur is subtly
        // wrong". At 150%, a window at logical (100, 200) is at physical
        // (150, 300).
        let (x, y, w, h) = panel_rect(window_at(100., 200.), 1.5);
        assert_eq!((x, y), (150, 300));
        assert_eq!((w, h), (990, 645), "660*1.5, 430*1.5");
    }

    #[test]
    fn hidden_window_capture_uses_measured_physical_bounds() {
        assert_eq!(
            panel_rect_in_window((150, 300, 990, 645)),
            Some((150, 300, 990, 645))
        );
    }

    #[test]
    fn hidden_compact_window_capture_covers_the_expanded_panel() {
        assert_eq!(
            panel_rect_in_window((150, 300, 990, 96)),
            Some((150, 300, 990, 645))
        );
    }

    #[test]
    fn capture_offsets_remain_correct_on_monitors_above_and_left() {
        let predicted = panel_rect(window_at(-800., -200.), 1.5);
        let hidden = panel_rect_in_window((-1200, -300, 990, 645));
        assert_eq!(predicted, (-1200, -300, 990, 645));
        assert_eq!(
            hidden,
            Some((
                predicted.0,
                predicted.1,
                predicted.2 as u32,
                predicted.3 as u32
            ))
        );
    }

    #[test]
    fn a_too_small_window_is_not_captured() {
        assert_eq!(panel_rect_in_window((0, 0, 0, 741)), None);
    }

    #[test]
    fn the_capture_uses_the_full_window_at_150() {
        // The captured desktop and painted panel share the same bounds, so a
        // scaled window must not retain an old inset from the former shadow.
        let scale = 1.5f32;
        let window = window_at(100., 200.);
        let (x, y, w, h) = panel_rect(window, scale);
        let win_left = (window.left().as_f32() * scale).round() as i32;
        let win_top = (window.top().as_f32() * scale).round() as i32;
        let win_right = win_left + (660. * scale).round() as i32;
        let win_bottom = win_top + (430. * scale).round() as i32;
        assert_eq!((x, y), (win_left, win_top));
        assert_eq!((x + w, y + h), (win_right, win_bottom));
    }

    #[test]
    fn a_nonsensical_scale_uses_a_safe_fallback() {
        let expected = panel_rect(window_at(0., 0.), 1.0);
        for scale in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                panel_rect(window_at(0., 0.), scale),
                expected,
                "scale {scale}"
            );
        }
    }
}
