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
//! here: the window is `SW_HIDE` between toggles, so the show path captures
//! first and shows second, and the very first frame the user sees already has
//! the blur on it.
//!
//! That is also why this is synchronous. The alternative is to capture on the
//! background executor and swap the result in when it lands, which would show
//! an unblurred panel for one frame on the first show and a *stale* blur
//! thereafter. The whole capture is a few hundred thousand pixels through a
//! box filter, and `show` logs how long it took so the number is measured
//! rather than assumed.

use std::sync::Arc;
use std::time::Instant;

use gpui::{Bounds, Pixels, RenderImage};
use image::{Delay, Frame, RgbaImage};

use crate::ui::{FRAME_MARGIN, POPUP_HEIGHT, POPUP_WIDTH};

/// A blurred backdrop, ready to paint, plus what it cost to make.
pub struct Backdrop {
    /// The image, or `None` if the capture or the blur was refused.
    pub image: Option<Arc<RenderImage>>,
    /// How long capture plus blur took. Logged on every show, because the
    /// whole reason this runs on the keystroke path is that it is cheap, and
    /// "cheap" is a claim that needs a number attached.
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

    let image = orca_win::capture_screen_region(x, y, width as u32, height as u32)
        .ok()
        .and_then(|region| orca_core::backdrop::soften(&region.bgra, region.width, region.height))
        .filter(orca_core::backdrop::Softened::is_paintable)
        .map(|softened| Arc::new(RenderImage::new([frame_of(&softened)])));

    Backdrop {
        image,
        took_ms: started.elapsed().as_secs_f64() * 1000.0,
    }
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

    #[test]
    fn the_backdrop_is_optional_and_never_fatal() {
        // The contract: a launcher with no backdrop still opens. `capture`
        // returns a struct rather than a Result precisely so the show path has
        // nothing to branch on but `image.is_some()`.
        let backdrop = capture(window_at(0., 0.), 1.0);
        assert!(backdrop.took_ms.is_finite());
        // Whether `image` is `Some` depends on there being a desktop session, so
        // it is deliberately not asserted. This test is about the *shape* of the
        // return, which is the part the show path depends on.
    }
}
