//! The shell's icon for a file, a file type, or a folder, as plain pixels.
//!
//! # Why `SHGetFileInfoW`
//!
//! It is the one call that answers "what icon does Explorer show for this?" for
//! every kind of result the launcher has: an `.exe`'s embedded icon, a
//! shortcut's target icon, and the icon registered for an extension. With
//! `SHGFI_USEFILEATTRIBUTES` it answers the extension and folder cases from the
//! registry alone, without touching the disk, which is what makes a per-type
//! icon cheap enough to share across every file of that type.
//!
//! `SHGFI_LARGEICON` is the system large-icon size: 32px at 100% and scaled
//! with the system DPI, so it stays sharp at the 24 logical pixels a row draws
//! it at.
//!
//! # The pixels are straight alpha, and sometimes there is no alpha
//!
//! A 32-bit icon's colour bitmap carries non-premultiplied BGRA, which is what
//! `gpui::RenderImage` stores, so it passes through untouched. An older icon
//! has an all-zero alpha channel and keeps its transparency in a separate 1-bit
//! mask; [`apply_mask`] rebuilds the alpha from it. Skipping that step paints
//! every legacy icon as an invisible square.
//!
//! # One call at a time
//!
//! Concurrent `SHGetFileInfoW` calls from several threads fail: with two or
//! three in flight, extension and `.exe` lookups came back with no icon while a
//! lone call for the same thing succeeded. The gate's parallel test runner is
//! what showed it. So every load holds [`SHELL_LOCK`]; the loads are a few
//! milliseconds each, and serialising them costs less than a blank row.

use std::fmt;
use std::path::Path;
use std::sync::Mutex;

use windows::core::{Error as Win32Error, PCWSTR};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

use crate::apps::ComApartment;
use crate::wide::{wide_nul, wide_path_nul};

/// What to ask the shell for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellIcon<'a> {
    /// The icon of this specific file. Reads the file.
    File(&'a Path),
    /// The icon registered for an extension, given without its dot. An empty
    /// extension asks for the icon of a file with none. Does not touch the disk.
    Extension(&'a str),
    /// The generic folder icon. Does not touch the disk.
    Folder,
}

/// An icon as tightly packed BGRA, top-down, straight (not premultiplied) alpha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconImage {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub bgra: Vec<u8>,
}

/// The icon could not be produced.
#[derive(Debug)]
pub enum IconError {
    /// The shell has no icon for this request, e.g. a file that no longer exists.
    NoIcon,
    /// The icon is monochrome, or a size no row would draw.
    Unsupported {
        /// What was wrong with it.
        what: &'static str,
    },
    /// A Win32 call failed.
    Call {
        /// The failing call.
        what: &'static str,
        /// The Win32 error code.
        code: i32,
    },
}

impl fmt::Display for IconError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IconError::NoIcon => write!(f, "the shell has no icon for this item"),
            IconError::Unsupported { what } => write!(f, "unsupported icon: {what}"),
            IconError::Call { what, code } => {
                write!(f, "reading an icon failed at {what} (Win32 error {code})")
            }
        }
    }
}

impl std::error::Error for IconError {}

/// The largest icon edge accepted. `SHGFI_LARGEICON` is 32px at 100% and 64px
/// at 200%; anything past this is a broken bitmap, not an icon.
const MAX_EDGE: i32 = 256;

/// Serialises shell icon lookups. See the module docs.
static SHELL_LOCK: Mutex<()> = Mutex::new(());

/// Reads the shell's large icon for `request`.
///
/// Blocking — it may load a shell extension or read an `.exe` — so it belongs
/// on a background thread, never on the UI thread.
pub fn shell_icon(request: ShellIcon<'_>) -> Result<IconImage, IconError> {
    // Guards no data, so a panic while it was held leaves nothing inconsistent.
    let _serial = SHELL_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Shell icon handlers are COM objects; without an apartment some of them
    // decline and the shell falls back to a generic icon.
    let _com = ComApartment::enter();

    let (name, attributes, flags) = match request {
        ShellIcon::File(path) => (
            wide_path_nul(path),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            SHGFI_ICON | SHGFI_LARGEICON,
        ),
        ShellIcon::Extension(extension) => (
            wide_nul(&if extension.is_empty() {
                "file".to_owned()
            } else {
                format!("file.{extension}")
            }),
            FILE_ATTRIBUTE_NORMAL,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        ),
        ShellIcon::Folder => (
            wide_nul("folder"),
            FILE_ATTRIBUTE_DIRECTORY,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        ),
    };

    let mut info = SHFILEINFOW::default();
    // SAFETY: `name` is NUL-terminated and outlives the call; `info` is a live
    // local whose size is passed alongside it.
    let found = unsafe {
        SHGetFileInfoW(
            PCWSTR(name.as_ptr()),
            attributes,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    if found == 0 || info.hIcon.is_invalid() {
        return Err(IconError::NoIcon);
    }
    let icon = IconGuard(info.hIcon);
    icon_pixels(icon.0)
}

fn icon_pixels(icon: HICON) -> Result<IconImage, IconError> {
    let mut parts = ICONINFO::default();
    // SAFETY: `icon` is a live handle owned by the caller's guard; `parts` is a
    // live local.
    unsafe { GetIconInfo(icon, &mut parts) }.map_err(|error| call("GetIconInfo", error))?;
    // `GetIconInfo` hands back copies of both bitmaps, which the caller owns.
    let _mask = BitmapGuard(parts.hbmMask);
    let _color = BitmapGuard(parts.hbmColor);
    if parts.hbmColor.is_invalid() {
        return Err(IconError::Unsupported {
            what: "monochrome icon",
        });
    }

    let mut bitmap = BITMAP::default();
    // SAFETY: `bitmap` is a live local and its size is passed with it.
    let written = unsafe {
        GetObjectW(
            parts.hbmColor.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bitmap as *mut BITMAP).cast()),
        )
    };
    if written == 0 {
        return Err(call("GetObjectW", Win32Error::from_thread()));
    }
    let (width, height) = (bitmap.bmWidth, bitmap.bmHeight);
    if !(1..=MAX_EDGE).contains(&width) || !(1..=MAX_EDGE).contains(&height) {
        return Err(IconError::Unsupported {
            what: "icon size outside 1..=256px",
        });
    }

    // SAFETY: a memory DC compatible with the screen; released by the guard.
    let dc = DcGuard(unsafe { CreateCompatibleDC(None) });
    if dc.0.is_invalid() {
        return Err(call("CreateCompatibleDC", Win32Error::from_thread()));
    }

    let mut bgra = read_bits(dc.0, parts.hbmColor, width, height, "GetDIBits (colour)")?;
    let mask = read_bits(dc.0, parts.hbmMask, width, height, "GetDIBits (mask)")?;
    apply_mask(&mut bgra, &mask);

    Ok(IconImage {
        width: width as u32,
        height: height as u32,
        bgra,
    })
}

/// Reads `bitmap` as 32-bit top-down BGRA, converting from whatever depth it is
/// stored at. A 1-bit mask comes back black where opaque and white where not.
fn read_bits(
    dc: HDC,
    bitmap: HBITMAP,
    width: i32,
    height: i32,
    what: &'static str,
) -> Result<Vec<u8>, IconError> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative for top-down, so row 0 is the top of the icon.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    // SAFETY: `pixels` holds exactly `height` rows of `width` 32-bit pixels, the
    // format `info` describes, and `bitmap` is not selected into any DC.
    let lines = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if lines != height {
        return Err(call(what, Win32Error::from_thread()));
    }
    Ok(pixels)
}

/// Rebuilds alpha from the 1-bit mask for an icon whose colour bitmap has none.
///
/// An icon with any non-zero alpha already carries real transparency and is
/// left alone; its mask is only a fallback for old renderers.
fn apply_mask(bgra: &mut [u8], mask: &[u8]) {
    let (pixels, _) = bgra.as_chunks_mut::<4>();
    if pixels.iter().any(|pixel| pixel[3] != 0) {
        return;
    }
    for (pixel, bit) in pixels.iter_mut().zip(mask.as_chunks::<4>().0) {
        // Mask black (0) is opaque, mask white is transparent.
        pixel[3] = if bit[0] == 0 { 255 } else { 0 };
    }
}

fn call(what: &'static str, error: Win32Error) -> IconError {
    IconError::Call {
        what,
        code: error.code().0,
    }
}

/// Destroys the icon `SHGetFileInfoW` created. Without it every row leaks a
/// `USER` handle per load, and the process has 10,000 of those.
struct IconGuard(HICON);

impl Drop for IconGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from `SHGetFileInfoW` and is not used again.
        unsafe {
            let _ = DestroyIcon(self.0);
        }
    }
}

struct BitmapGuard(HBITMAP);

impl Drop for BitmapGuard {
    fn drop(&mut self) {
        if self.0.is_invalid() {
            return;
        }
        // SAFETY: the handle came from `GetIconInfo`, which transfers ownership.
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}

struct DcGuard(HDC);

impl Drop for DcGuard {
    fn drop(&mut self) {
        if self.0.is_invalid() {
            return;
        }
        // SAFETY: the handle came from `CreateCompatibleDC` and has nothing
        // selected into it.
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPAQUE_MASK: [u8; 4] = [0, 0, 0, 0];
    const CLEAR_MASK: [u8; 4] = [255, 255, 255, 0];

    #[test]
    fn an_icon_without_alpha_takes_its_transparency_from_the_mask() {
        let mut pixels = [10, 20, 30, 0, 40, 50, 60, 0];
        let mask = [OPAQUE_MASK, CLEAR_MASK].concat();
        apply_mask(&mut pixels, &mask);
        assert_eq!(pixels, [10, 20, 30, 255, 40, 50, 60, 0]);
    }

    #[test]
    fn an_icon_with_alpha_ignores_the_mask() {
        // A soft edge at alpha 128 must not be flattened to the mask's 1 bit.
        let mut pixels = [10, 20, 30, 128, 40, 50, 60, 0];
        let mask = [CLEAR_MASK, OPAQUE_MASK].concat();
        apply_mask(&mut pixels, &mask);
        assert_eq!(pixels, [10, 20, 30, 128, 40, 50, 60, 0]);
    }

    fn assert_paintable(icon: &IconImage) {
        assert!(icon.width >= 16 && icon.height >= 16, "{icon:?}");
        assert_eq!(icon.bgra.len(), (icon.width * icon.height * 4) as usize);
        assert!(
            icon.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] > 0),
            "every pixel is transparent"
        );
    }

    // These ask the real shell. They need no file on disk and no window, and
    // they are the only check that the HICON-to-pixels path returns an image
    // at all rather than an empty square.
    #[test]
    fn the_folder_icon_is_readable() {
        assert_paintable(&shell_icon(ShellIcon::Folder).expect("folder icon"));
    }

    #[test]
    fn an_extension_icon_is_readable() {
        assert_paintable(&shell_icon(ShellIcon::Extension("txt")).expect("txt icon"));
        assert_paintable(&shell_icon(ShellIcon::Extension("")).expect("no-extension icon"));
    }

    #[test]
    fn concurrent_loads_all_succeed() {
        // The regression: unserialised, parallel lookups returned `NoIcon`.
        let exe = std::env::current_exe().expect("test binary path");
        std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|n| {
                    let exe = &exe;
                    scope.spawn(move || match n % 3 {
                        0 => shell_icon(ShellIcon::Extension("txt")),
                        1 => shell_icon(ShellIcon::File(exe)),
                        _ => shell_icon(ShellIcon::Folder),
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                assert_paintable(&handle.join().expect("no panic").expect("icon"));
            }
        });
    }

    #[test]
    fn an_executables_icon_is_readable() {
        let exe = std::env::current_exe().expect("test binary path");
        assert_paintable(&shell_icon(ShellIcon::File(&exe)).expect("exe icon"));
    }
}
