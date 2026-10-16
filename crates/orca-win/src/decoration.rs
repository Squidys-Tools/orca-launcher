//! Removing the accent border GPUI enables for transparent windows.

use std::sync::OnceLock;

use windows::core::PCSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

use crate::WindowHandle;

const WCA_ACCENT_POLICY: u32 = 0x13;
const ACCENT_DISABLED: u32 = 0;

#[repr(C)]
struct AccentPolicy {
    accent_state: u32,
    accent_flags: u32,
    gradient_color: u32,
    animation_id: u32,
}

#[repr(C)]
struct WindowCompositionAttribData {
    attribute: u32,
    data: *mut std::ffi::c_void,
    data_size: usize,
}

type SetWindowCompositionAttributeFn =
    unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> i32;

/// Turns off the accent policy that GPUI enables for transparent windows.
///
/// This removes the accent-policy border without changing the Direct
/// Composition swap chain's per-pixel alpha. DWM's separate frame and shadow
/// still need to be handled by the caller.
pub fn clear_accent_border(window: WindowHandle) -> bool {
    if window.is_null() {
        return false;
    }

    static SET_COMPOSITION_ATTRIBUTE: OnceLock<Option<SetWindowCompositionAttributeFn>> =
        OnceLock::new();
    let Some(setter) = SET_COMPOSITION_ATTRIBUTE
        .get_or_init(|| {
            let user32 =
                unsafe { GetModuleHandleA(PCSTR::from_raw(c"user32.dll".as_ptr().cast::<u8>())) }
                    .ok()?;
            let address = unsafe {
                GetProcAddress(
                    user32,
                    PCSTR::from_raw(c"SetWindowCompositionAttribute".as_ptr().cast::<u8>()),
                )
            }?;
            Some(unsafe {
                std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    SetWindowCompositionAttributeFn,
                >(address)
            })
        })
        .as_ref()
    else {
        return false;
    };

    let policy = AccentPolicy {
        accent_state: ACCENT_DISABLED,
        accent_flags: 0,
        gradient_color: 0,
        animation_id: 0,
    };
    let mut data = WindowCompositionAttribData {
        attribute: WCA_ACCENT_POLICY,
        data: &policy as *const AccentPolicy as *mut std::ffi::c_void,
        data_size: std::mem::size_of::<AccentPolicy>(),
    };

    let hwnd: HWND = window.into();
    unsafe { setter(hwnd, &mut data) != 0 }
}
