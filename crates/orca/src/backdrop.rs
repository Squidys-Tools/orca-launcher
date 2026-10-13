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

use crate::ui::{FRAME_MARGIN, POPUP_HEIGHT, POPUP_WIDTH};

/// A blurred backdrop, ready to paint, plus what it cost to make.
pub struct Backdrop {
    /// The image, or `None` if the capture or the blur was refused.
    pub image: Option<Arc<RenderImage>>,
    /// How long capture plus blur took, logged when the background task finishes.
    pub took_ms: f64,
}

/// Captures and blurs the desktop behind a panel of the given size.
///
/// `window_bounds` is the popup *window's* bounds in GPUI logical pixels, not
/// the panel's — the panel is inset by [`FRAME_MARGIN`], and capturing the
/// window's rect and painting it inside the panel would be off by that margin
/// on three sides. `scale_factor` converts to the physical pixels Win32 uses.
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

/// Captures a hidden popup's panel using its measured physical window rectangle.
///
/// The native rectangle avoids using the cursor's current monitor to infer the
/// capture location: the pointer can move after the popup was positioned, but
/// the hidden popup's HWND still reports the exact bounds that must be sampled.
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
    let scale_x = window_width as f32 / (POPUP_WIDTH + FRAME_MARGIN * 2.0);
    let scale_y = window_height as f32 / (POPUP_HEIGHT + FRAME_MARGIN * 2.0);
    if !scale_x.is_finite() || !scale_y.is_finite() || scale_x <= 0.0 || scale_y <= 0.0 {
        return None;
    }
    let inset_x = u32::try_from((FRAME_MARGIN * scale_x).round() as i64).ok()?;
    let inset_y = u32::try_from((FRAME_MARGIN * scale_y).round() as i64).ok()?;
    let width = window_width.checked_sub(inset_x.checked_mul(2)?)?;
    let height = window_height.checked_sub(inset_y.checked_mul(2)?)?;
    let x = x.checked_add(i32::try_from(inset_x).ok()?)?;
    let y = y.checked_add(i32::try_from(inset_y).ok()?)?;
    (width > 0 && height > 0).then_some((x, y, width, height))
}

/// The screen rectangle to capture, in **physical** pixels.
///
/// Two conversions, and getting either wrong is invisible. `window_bounds` is
/// in GPUI logical pixels, and Win32 wants physical ones, so the position is
/// scaled as well as the size — scaling only the size puts the capture a third
/// of the way off at 150%. The panel is also inset by [`FRAME_MARGIN`] on every
/// side, because `window_bounds` is the *window* and the panel is what the
/// backdrop is drawn inside; capturing the window rect would show the
/// neighbouring slice of desktop along the top and left.
///
/// Extracted as its own function so the tests assert the arithmetic that
/// actually runs rather than a copy of it that can drift.
fn panel_rect(window_bounds: Bounds<Pixels>, scale_factor: f32) -> (i32, i32, i32, i32) {
    // A monitor reporting a nonsensical scale must not collapse the capture to
    // nothing: that is a hole in the middle of the panel, and a flat fill is
    // strictly better.
    let scale = if scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let inset = (FRAME_MARGIN * scale).round() as i32;
    let width = ((POPUP_WIDTH * scale).round() as i32).max(1);
    let height = ((POPUP_HEIGHT * scale).round() as i32).max(1);
    let x = (window_bounds.left().as_f32() * scale).round() as i32 + inset;
    let y = (window_bounds.top().as_f32() * scale).round() as i32 + inset;
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
        Bounds::new(point(px(x), px(y)), size(px(724.), px(494.)))
    }

    #[test]
    fn the_capture_is_the_panel_not_the_window() {
        let (x, y, w, h) = panel_rect(window_at(100., 200.), 1.0);
        assert_eq!((x, y), (132, 232), "the inset is FRAME_MARGIN on both axes");
        assert_eq!(
            (w, h),
            (660, 430),
            "the capture is the panel, not the window"
        );
    }

    #[test]
    fn position_and_size_both_scale_at_150() {
        // The failure this is here for: scaling only the *size* and adding an
        // unscaled inset looks almost right at 100% and puts the capture well
        // off at 150%, which reads as nothing more than "the blur is subtly
        // wrong". At 150%, a window at logical (100, 200) is at physical
        // (150, 300), and 32 logical px is 48 physical.
        let (x, y, w, h) = panel_rect(window_at(100., 200.), 1.5);
        assert_eq!((x, y), (198, 348), "150+48, 300+48");
        assert_eq!((w, h), (990, 645), "660*1.5, 430*1.5");
    }

    #[test]
    fn hidden_window_capture_uses_measured_physical_bounds() {
        assert_eq!(
            panel_rect_in_window((150, 300, 1086, 741)),
            Some((198, 348, 990, 645))
        );
    }

    #[test]
    fn a_too_small_window_is_not_captured() {
        assert_eq!(panel_rect_in_window((0, 0, 0, 741)), None);
    }

    #[test]
    fn the_capture_stays_inside_the_window_at_150() {
        // A stronger form of the same property: the captured rect must sit
        // within the physical window rect, or the desktop sampled under the
        // shadow is a different region from the one under the panel.
        let scale = 1.5f32;
        let window = window_at(100., 200.);
        let (x, y, w, h) = panel_rect(window, scale);
        let win_left = (window.left().as_f32() * scale).round() as i32;
        let win_top = (window.top().as_f32() * scale).round() as i32;
        let win_right = win_left + (724. * scale).round() as i32;
        let win_bottom = win_top + (494. * scale).round() as i32;
        assert!(
            x > win_left && y > win_top,
            "capture starts outside the window"
        );
        assert!(x + w < win_right, "capture overflows the right edge");
        assert!(y + h < win_bottom, "capture overflows the bottom edge");
    }

    #[test]
    fn a_nonsensical_scale_still_produces_a_capture_rather_than_a_hole() {
        // A monitor reporting scale 0 would divide the panel to nothing, and the
        // panel would have a hole in it rather than a flat fill.
        let (_, _, w, h) = panel_rect(window_at(0., 0.), 0.0);
        assert!(w >= 1 && h >= 1, "got {w}x{h}");
    }
}
