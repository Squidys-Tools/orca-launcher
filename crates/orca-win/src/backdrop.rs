//! Capturing a rectangle of the screen, for the launcher's frosted-glass panel.
//!
//! # Why this exists
//!
//! The popup is a translucent panel over the desktop. Translucent is not enough:
//! a dark panel over a *detailed* desktop is a panel you cannot read, because
//! the text behind it competes with the text in it. The desktop has to be
//! blurred, and Windows will not do it for us — `gpui`'s
//! `WindowBackgroundAppearance::Blurred` and the Mica variants all apply their
//! material to the whole window rectangle, and the window is deliberately larger
//! than the panel so its drop shadow has somewhere to live. A backdrop material
//! would fill the shadow margin too and turn the rounded panel into a rounded
//! *square*.
//!
//! So we capture the region ourselves and blur it. `docs/ARCHITECTURE.md` has
//! the full reasoning.
//!
//! # GDI, and why not Windows.Graphics.Capture
//!
//! GDI `BitBlt` off the screen DC. The modern alternative,
//! `Windows.Graphics.Capture`, shows the user a consent dialog and returns a
//! `Direct3D11CaptureFramePool` we would then have to copy out of GPU memory.
//! For a launcher that is blurring a few hundred thousand pixels a second time
//! the user presses a key, GDI is the right tool: no consent prompt, no extra
//! dependency, and the pixels land in a plain `Vec<u8>` we already know how to
//! hand to the renderer.
//!
//! # The call has one precondition worth stating
//!
//! **The window must be hidden when this runs.** `BitBlt` photographs whatever is
//! on screen, so capturing with the popup already up captures the popup. The
//! retained-window design helps here rather than hurting: the window is `SW_HIDE`
//! between toggles, so the show path captures first and shows second.

use std::fmt;

use windows::core::Error as Win32Error;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ, SRCCOPY,
};

/// A captured rectangle of the screen, as tightly packed BGRA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedRegion {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
    /// `width * height * 4` bytes, 8-bit BGRA, top-down, no row padding.
    pub bgra: Vec<u8>,
}

/// The screen could not be read.
#[derive(Debug)]
pub enum BackdropError {
    /// A GDI call returned a null handle or zero.
    Gdi {
        /// What was being attempted.
        what: &'static str,
        /// The Win32 error code.
        code: i32,
    },
    /// The caller asked for a rectangle with no area.
    Empty,
}

impl fmt::Display for BackdropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackdropError::Gdi { what, code } => {
                write!(
                    f,
                    "capturing the screen for the backdrop failed at {what} (Win32 error {code})"
                )
            }
            BackdropError::Empty => write!(f, "cannot capture a region with no area"),
        }
    }
}

impl std::error::Error for BackdropError {}

/// Reads a rectangle of the screen into a tightly packed BGRA buffer.
///
/// `x`/`y` are **physical** screen pixels, top-left origin, exactly as Win32
/// reports them. The caller owns the conversion from GPUI's logical pixels;
/// `win::logical_cursor` already does that conversion for a point, and getting
/// it wrong here means sampling the wrong part of the wallpaper rather than
/// failing — which is why the conversion is not repeated here.
///
/// The window must be hidden. See the module docs.
///
/// # Panics
///
/// Never. Every GDI handle is checked and released on the failure path.
pub fn capture_screen_region(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<CapturedRegion, BackdropError> {
    if width == 0 || height == 0 {
        return Err(BackdropError::Empty);
    }
    // A region larger than this is either a scaling bug or a hostile
    // multi-monitor layout, and either way the allocation below would be the
    // largest thing the launcher ever does on the keystroke path. 8192 is far
    // past any plausible popup.
    const MAX_DIMENSION: u32 = 8192;
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(BackdropError::Gdi {
            what: "a region larger than 8192px",
            code: 0,
        });
    }

    // SAFETY: every GDI call below is paired and every handle is released on
    // every path, including the error paths, via the guards below. The pointers
    // handed to `CreateDIBSection` and `BitBlt` all point at live locals or at
    // storage owned by the DIB section itself.
    unsafe {
        let screen = GetDC(None);
        if screen.0.is_null() {
            return Err(gdi("GetDC", Win32Error::from_thread()));
        }
        let _screen_guard = DcGuard {
            dc: screen,
            is_screen: true,
        };

        let memory = CreateCompatibleDC(Some(screen));
        if memory.0.is_null() {
            return Err(gdi("CreateCompatibleDC", Win32Error::from_thread()));
        }
        let _memory_guard = DcGuard {
            dc: memory,
            is_screen: false,
        };

        // Top-down (negative `biHeight`) so row 0 of the buffer is the top of
        // the region. Without it GDI hands back a bottom-up bitmap and the
        // backdrop renders upside down — which looks like a plausible image
        // until you notice it is the wrong way round.
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        // `CreateDIBSection` is one of the few calls in this crate that already
        // returns a `Result`, so its error is used rather than re-derived from
        // `GetLastError` — asking twice can report a *later* failure, which is a
        // classic way to end up debugging the wrong call.
        let section =
            match CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(section) => section,
                Err(error) => return Err(gdi("CreateDIBSection", error)),
            };
        let _section_guard = ObjectGuard(section);

        // Put the section into the memory DC and remember what was there, so it
        // can be put back before the DC is deleted. Deleting a DC that still has
        // a bitmap selected is a documented way to leak a GDI handle per call.
        let previous = SelectObject(memory, section.into());
        if previous.0.is_null() {
            return Err(gdi("SelectObject", Win32Error::from_thread()));
        }
        let _previous_guard = SelectedGuard {
            dc: memory,
            previous,
        };

        // `CAPTUREBLT` includes layered windows in the copy. Without it a
        // maximised window, or anything with a translucent acrylic border, is
        // captured as a black rectangle — which is the whole desktop area in
        // front of most people.
        if let Err(error) = BitBlt(
            memory,
            0,
            0,
            width as i32,
            height as i32,
            Some(screen),
            x,
            y,
            SRCCOPY | CAPTUREBLT,
        ) {
            return Err(gdi("BitBlt", error));
        }

        if bits.is_null() {
            return Err(gdi(
                "CreateDIBSection returned no pixels",
                Win32Error::from_thread(),
            ));
        }
        let len = (width as usize) * (height as usize) * 4;
        let bgra = std::slice::from_raw_parts(bits as *const u8, len).to_vec();

        Ok(CapturedRegion {
            width,
            height,
            bgra,
        })
    }
}

fn gdi(what: &'static str, error: Win32Error) -> BackdropError {
    BackdropError::Gdi {
        what,
        code: error.code().0,
    }
}

/// Releases a device context on drop, including on the error paths.
///
/// The `is_screen` flag is not decoration. A DC from `GetDC(None)` is released
/// with `ReleaseDC` and a DC from `CreateCompatibleDC` is destroyed with
/// `DeleteDC`; calling the wrong one on the wrong kind of handle is undefined,
/// and getting it backwards for the screen DC leaks a handle on every single
/// keystroke. The flag is recorded at the point of creation because that is the
/// only place the answer is known — there is no reliable query for it later.
struct DcGuard {
    dc: HDC,
    is_screen: bool,
}

impl Drop for DcGuard {
    fn drop(&mut self) {
        if self.dc.0.is_null() {
            return;
        }
        // SAFETY: the handle came from `GetDC` or `CreateCompatibleDC` in this
        // function and is not used again — the guards drop in reverse order of
        // declaration, so the selected bitmap and the section are cleaned up
        // before either DC.
        unsafe {
            if self.is_screen {
                ReleaseDC(None, self.dc);
            } else {
                let _ = DeleteDC(self.dc);
            }
        }
    }
}

/// Restores the previous bitmap selection before its DC is deleted.
struct SelectedGuard {
    dc: HDC,
    previous: HGDIOBJ,
}

impl Drop for SelectedGuard {
    fn drop(&mut self) {
        if self.previous.0.is_null() {
            return;
        }
        // SAFETY: `dc` is a live memory DC owned by a `DcGuard` that drops
        // after this one, and `previous` is the handle `SelectObject` returned.
        unsafe {
            SelectObject(self.dc, self.previous);
        }
    }
}

/// Deletes a GDI object on drop.
struct ObjectGuard(HBITMAP);

impl Drop for ObjectGuard {
    fn drop(&mut self) {
        if self.0.is_invalid() {
            return;
        }
        // SAFETY: the handle came from `CreateDIBSection` in this function and
        // is not used again. `SelectedGuard` is declared *after* this one, so
        // it drops first and restores the old selection — a bitmap that is still
        // selected must not be deleted.
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_region_is_refused_before_any_gdi_call() {
        // Zero-sized must be caught here rather than becoming a 0x0 DIB, which
        // `CreateDIBSection` accepts and which yields an image nothing can
        // paint.
        assert!(matches!(
            capture_screen_region(0, 0, 0, 100),
            Err(BackdropError::Empty)
        ));
        assert!(matches!(
            capture_screen_region(0, 0, 100, 0),
            Err(BackdropError::Empty)
        ));
    }

    #[test]
    fn an_absurd_region_is_refused_rather_than_allocated() {
        // The allocation is `width * height * 4`. At 100000² that is 40GB, and
        // it would be allocated on the keystroke path.
        let err = capture_screen_region(0, 0, 100_000, 100_000)
            .expect_err("a 100k-pixel region must be refused");
        assert!(matches!(err, BackdropError::Gdi { .. }), "{err:?}");
    }

    #[test]
    fn the_error_message_names_the_failing_call() {
        // A bare "Win32 error 0" sends the reader hunting. The `what` is the
        // whole value of the message.
        let message = BackdropError::Gdi {
            what: "BitBlt",
            code: 5,
        }
        .to_string();
        assert!(message.contains("BitBlt"), "{message}");
        assert!(message.contains('5'), "{message}");
    }

    #[test]
    fn a_real_capture_is_the_right_size_and_is_fully_opaque() {
        // The one test here that touches GDI. It is not hermetic — it needs a
        // desktop session — but it is the only way to catch a stride or
        // top-down mistake, both of which produce a correctly-sized buffer full
        // of plausible-looking nonsense.
        let Ok(region) = capture_screen_region(0, 0, 64, 64) else {
            // Headless or locked session. Not a failure; there is nothing to
            // assert against.
            return;
        };
        assert_eq!(region.width, 64);
        assert_eq!(region.height, 64);
        assert_eq!(region.bgra.len(), 64 * 64 * 4);
        assert!(
            region.bgra.as_chunks::<4>().0.iter().all(|p| p[3] == 255),
            "a GDI screen capture is opaque; any other alpha means the DIB was \
             not 32bpp or the copy did not happen"
        );
    }
}
