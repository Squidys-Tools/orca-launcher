//! Bringing a window to the front, and asking whether it already is.
//!
//! # Why this is not one line
//!
//! `SetForegroundWindow` is subject to the *foreground lock*: a process that
//! does not own the current foreground window may not take it, unless it was
//! started by the foreground process, received the last input, or is a
//! foreground service. A launcher satisfies none of those — it is woken by a
//! global hotkey while some other application has focus — so the plain call
//! silently fails and the window stays behind. That is not a Windows quirk to
//! work around later; it is why the probe had to reach for the same trick.
//!
//! The trick is `AttachThreadInput`: temporarily joining our thread's input
//! queue to the foreground window's, which makes the foreground process treat
//! us as its own for the duration and so drops the lock. The joins are always
//! undone, including on the failure path, because leaving them attached wedges
//! input for both threads.
//!
//! This is a no-op for a window that is already foreground, which is the common
//! case for a hotkey toggle.

use std::fmt;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::AttachThreadInput;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SetForegroundWindow, ShowWindow,
    SW_RESTORE, SW_SHOW,
};

/// An opaque Win32 window handle.
///
/// Deliberately not `windows::HWND`: that would make every consumer of this
/// crate depend on the `windows` crate and on a matching feature set, for
/// nothing but a pointer. `isize` is the shape GPUI hands out on Windows
/// (`raw_window_handle::Win32WindowHandle::hwnd`), so the conversion is free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowHandle(isize);

impl WindowHandle {
    /// Wraps a raw `HWND` as reported by the windowing layer.
    ///
    /// A null or otherwise meaningless value is the caller's problem, not this
    /// function's: nothing here can tell a stale handle from a valid one, and
    /// guessing would turn a bug into a silent no-op.
    pub fn from_raw(raw: isize) -> WindowHandle {
        WindowHandle(raw)
    }

    /// The raw `HWND`, as a pointer-sized integer.
    pub fn as_raw(self) -> isize {
        self.0
    }

    /// Whether this is the null handle, which no window has.
    pub fn is_null(self) -> bool {
        self.0 == 0
    }

    fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut core::ffi::c_void)
    }
}

impl From<WindowHandle> for HWND {
    fn from(value: WindowHandle) -> Self {
        value.to_hwnd()
    }
}

/// Why a window could not be brought to the front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundError {
    /// The handle was null.
    NullHandle,
    /// Windows refused even after the input queues were joined.
    ///
    /// Reported as a refusal rather than swallowed because the caller can see
    /// the window state and retry; a silent failure would leave the user
    /// pressing a hotkey that appears to do nothing.
    Refused,
}

impl fmt::Display for ForegroundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForegroundError::NullHandle => write!(f, "window handle is null"),
            ForegroundError::Refused => {
                write!(f, "Windows refused to give the window the foreground")
            }
        }
    }
}

impl std::error::Error for ForegroundError {}

/// Whether `window` is the foreground window.
pub fn is_foreground(window: WindowHandle) -> bool {
    if window.is_null() {
        return false;
    }
    // SAFETY: GetForegroundWindow takes no arguments and cannot fail. The
    // comparison is a pointer equality test on a handle we were handed; a stale
    // handle simply will not match, which is the correct answer for "is this
    // window in front right now".
    unsafe { GetForegroundWindow() == window.to_hwnd() }
}

/// Brings `window` to the front and gives it the keyboard focus.
///
/// A minimized window is restored first: `SetForegroundWindow` on a minimized
/// window leaves it minimized, so the call would appear to succeed while the
/// user still sees their old window.
///
/// Returns [`ForegroundError::NullHandle`] for a null handle and
/// [`ForegroundError::Refused`] when Windows declines, rather than reporting
/// success it cannot confirm.
pub fn activate(window: WindowHandle) -> Result<(), ForegroundError> {
    if window.is_null() {
        return Err(ForegroundError::NullHandle);
    }
    let hwnd = window.to_hwnd();

    // SAFETY: GetForegroundWindow takes no arguments and cannot fail.
    if unsafe { GetForegroundWindow() } == hwnd {
        return Ok(());
    }

    // SAFETY: GetWindowThreadProcessId writes nothing when the out-pointer is
    // null, and returns 0 for a handle that has no thread. A 0 here is not an
    // error; it just means there is no input queue to join.
    let target_thread = unsafe { GetWindowThreadProcessId(hwnd, None) };

    // SAFETY: as above, for the current foreground window.
    let current_foreground = unsafe { GetForegroundWindow() };
    // SAFETY: as above. Null is the documented "give me no id" argument.
    let foreground_thread = unsafe { GetWindowThreadProcessId(current_foreground, None) };

    // SAFETY: takes no arguments and cannot fail; the returned id is this
    // thread's own input queue.
    let our_thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };

    // Attach only to real, distinct threads: attaching a queue to itself is
    // documented as a no-op that still has to be undone, and a 0 thread id is
    // not a thread at all.
    let attached_foreground = foreground_thread != 0
        && foreground_thread != our_thread
        && attach(our_thread, foreground_thread);
    let attached_target =
        target_thread != 0 && target_thread != our_thread && attach(our_thread, target_thread);

    // A minimized window must be restored or `SetForegroundWindow` leaves it
    // minimized and the call looks like it worked.
    // SAFETY: `hwnd` is non-null. The return value reports the *previous*
    // visibility and is deliberately ignored.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let _ = SetForegroundWindow(hwnd);
    };

    // Detach unconditionally, before looking at the result: leaving the queues
    // joined would corrupt input for this thread and the foreground one, and
    // that is far worse than a failed activation.
    if attached_target {
        detach(our_thread, target_thread);
    }
    if attached_foreground {
        detach(our_thread, foreground_thread);
    }

    // SAFETY: as above; a no-argument call that cannot fail.
    if unsafe { GetForegroundWindow() } == hwnd {
        Ok(())
    } else {
        Err(ForegroundError::Refused)
    }
}

/// Joins two threads' input queues, reporting whether the join took.
fn attach(from: u32, to: u32) -> bool {
    // SAFETY: both ids come from `GetWindowThreadProcessId` or
    // `GetCurrentThreadId`, so they name real threads. Attaching is scoped to
    // this process pair and is undone by the matching `detach`.
    unsafe { AttachThreadInput(from, to, true).as_bool() }
}

/// Undoes an [`attach`]. Must be called for every successful attach.
fn detach(from: u32, to: u32) {
    // SAFETY: the caller only reaches this after a matching successful
    // `attach` with the same ids, which is the required pairing.
    unsafe {
        let _ = AttachThreadInput(from, to, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_handle_is_not_the_foreground_window() {
        assert!(!is_foreground(WindowHandle::from_raw(0)));
    }

    #[test]
    fn activating_a_null_handle_is_an_error_not_a_no_op() {
        // Reporting success here would let a caller believe a window came
        // forward when it did not exist.
        assert_eq!(
            activate(WindowHandle::from_raw(0)),
            Err(ForegroundError::NullHandle)
        );
    }

    #[test]
    fn window_handles_round_trip_through_raw() {
        let handle = WindowHandle::from_raw(0x1234_5678);
        assert_eq!(handle.as_raw(), 0x1234_5678);
        assert!(!handle.is_null());
        assert!(WindowHandle::from_raw(0).is_null());
    }

    #[test]
    fn a_stale_handle_is_simply_never_foreground() {
        // Nothing owns this value, so no window can match it. The answer is
        // `false`, not a panic and not an error: "is this in front" has a
        // well-defined answer for a window that no longer exists.
        let bogus = WindowHandle::from_raw(0x7FFF_FF00);
        assert!(!is_foreground(bogus));
    }

    #[test]
    fn errors_explain_themselves() {
        assert!(ForegroundError::NullHandle.to_string().contains("null"));
        assert!(ForegroundError::Refused.to_string().contains("foreground"));
    }
}
