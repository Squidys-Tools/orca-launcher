//! Clipboard: put text on the Windows clipboard.
//!
//! # Why the sequence is spelled out
//!
//! There is no "copy this string" call. `SetClipboardData` accepts a handle to
//! a `GMEM_MOVEABLE` block that the *system* then owns, so everything before it
//! — open, empty, encode, allocate, lock, copy, unlock — exists only to build
//! that one handle. The order is not stylistic: `EmptyClipboard` after
//! `SetClipboardData` would erase what was just placed, and `SetClipboardData`
//! before `EmptyClipboard` would hand the system memory it then empties.
//!
//! # Ownership: the part that is invisible when wrong
//!
//! `SetClipboardData` succeeding means the system took the memory and the caller
//! must not free it; failing means the system did not take it and the caller
//! must. Both halves are required, and only one of them is observable: a missed
//! free leaks a handle, a wrong free corrupts memory the system is using. A
//! calculator that copies its result a handful of times a day discovers the
//! first of those in a week, and nothing fails in the meantime.
//!
//! # Verification status
//!
//! The Win32 path in this module is **statically unverified**. Nothing in the
//! test suite opens the clipboard, on purpose: a test that wrote to it would
//! clobber whatever the person at the machine just copied. What is verified
//! here is the arithmetic — how many bytes `GlobalAlloc` is asked for — because
//! that part is pure and is where the off-by-one lives. The rest is a manual
//! check: activate a computed result, paste somewhere, and confirm the text
//! arrives full-length.

use std::fmt;
use std::thread::sleep;
use std::time::Duration;

use windows::Win32::Foundation::{GetLastError, GlobalFree, SetLastError, HANDLE, WIN32_ERROR};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

use crate::single_instance::last_error_code;

/// How many times to try `OpenClipboard` before giving up.
///
/// Bounded, because a clipboard nobody closes must not hang the caller: this
/// runs on whichever thread asked, and for a launcher result that is the UI
/// thread. Eight attempts is enough to outlive the common holders and cannot
/// cost more than a fraction of a second.
const OPEN_ATTEMPTS: u32 = 8;

/// Pause between [`OPEN_ATTEMPTS`]. Eight attempts of fifteen milliseconds is
/// about a tenth of a second in total — past the point a person notices, and
/// far more than any legitimate clipboard hold lasts.
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(15);

/// `ERROR_SUCCESS`. Named rather than written inline so the retry's "nothing has
/// failed yet" state cannot be confused with a code Windows really reported.
const ERROR_SUCCESS: u32 = 0;

/// Error code `OpenClipboard` returns while another process holds the
/// clipboard. Common and transient, so it is the only code that is retried.
const ERROR_ACCESS_DENIED: u32 = 5;

/// Puts `text` on the Windows clipboard as Unicode text.
///
/// Empty text is a valid request: it stores an empty string, so a paste yields
/// nothing. It is deliberately *not* special-cased into a silent no-op, because
/// a no-op would leave whatever the user copied beforehand on the clipboard
/// while reporting success — exactly the surprise this function exists to
/// remove.
pub fn set_clipboard_text(text: &str) -> Result<(), ClipboardError> {
    // The trailing NUL is not decoration. Every `CF_UNICODETEXT` consumer,
    // including the ones nobody tested, finds the end of the string by scanning
    // for it, and a `Vec<u16>` built straight from a `&str` carries no
    // terminator at all.
    let units = crate::wide::wide_nul(text);
    with_clipboard_open(|| place_unicode_text(&units))
}

/// Runs `place` with the clipboard held open, and closes it on every path.
///
/// A function rather than an inline sequence because "close on the failure
/// paths too" is the requirement that is easiest to state and easiest to get
/// wrong by hand. A `CloseClipboard` that is missed leaves the clipboard owned
/// by a thread that has already returned, after which every `OpenClipboard` in
/// this process fails with `ERROR_ACCESS_DENIED` until that thread exits —
/// which for the UI thread is not until the process does.
fn with_clipboard_open(
    place: impl FnOnce() -> Result<(), ClipboardError>,
) -> Result<(), ClipboardError> {
    open_clipboard()?;

    let placed = place();

    // SAFETY: `open_clipboard` returned `Ok`, which is the documented
    // precondition, and the clipboard belongs to this thread until closed here.
    //
    // The result is deliberately ignored. By now the data is the system's, so a
    // failure here means a copy that landed: reporting it would make the caller
    // retry a copy it already got, or worse, tell the user it failed.
    unsafe {
        let _ = CloseClipboard();
    }

    placed
}

/// Opens the clipboard, retrying while another process holds it.
///
/// Any process mid-copy — a clipboard manager, Explorer, a remote desktop
/// session — makes `OpenClipboard` fail with `ERROR_ACCESS_DENIED`, and the
/// failure lasts only as long as that hold does. A single attempt therefore
/// turns a working copy into a coin flip, which is what this loop removes.
fn open_clipboard() -> Result<(), ClipboardError> {
    let mut code = ERROR_SUCCESS;

    for attempt in 0..OPEN_ATTEMPTS {
        // SAFETY: takes a plain value, has no preconditions, and touches only
        // this thread's error slot. The *before* matters: placed after the
        // call, it would wipe the real error and the reported code would be
        // whatever the previous call left behind.
        unsafe { SetLastError(WIN32_ERROR(ERROR_SUCCESS)) };

        match unsafe { OpenClipboard(None) } {
            Ok(()) => return Ok(()),
            Err(error) => {
                code = last_error_code(&error);
                // Only a busy clipboard is worth another attempt. Anything else
                // — a null owner, a handle that is not a window — is
                // deterministic and would fail identically on the next try.
                if code != ERROR_ACCESS_DENIED {
                    return Err(ClipboardError::Failed {
                        at: "OpenClipboard",
                        code,
                    });
                }
            }
        }

        // No sleep after the last attempt: the loop is over, and the caller
        // should not pay for a retry that never happened.
        if attempt + 1 < OPEN_ATTEMPTS {
            sleep(OPEN_RETRY_DELAY);
        }
    }

    Err(ClipboardError::Failed {
        at: "OpenClipboard",
        code,
    })
}

/// Clears the clipboard and installs `units` as Unicode text.
///
/// Runs with the clipboard already open, which is the precondition for every
/// call in it. On return the memory is either the system's (success) or freed
/// (failure); it is never left owned-and-leaked and never freed twice.
///
/// # Statically unverified
///
/// This is the Win32 path, and no test exercises it: a test would write to the
/// real clipboard and destroy whatever the user copied last. It is a manual
/// check — activate a computed result, paste it, confirm the full text
/// arrives. Do not add a test that calls it.
fn place_unicode_text(units: &[u16]) -> Result<(), ClipboardError> {
    // The clipboard keeps every format it held before this call unless they
    // are explicitly dropped, so a clipboard that was carrying a screenshot
    // would still carry it afterwards — and the paste would give the user an
    // image rather than the number they just activated. The call also releases
    // the *previous* owner's memory, which is the system's bookkeeping and not
    // this function's.
    //
    // SAFETY: the clipboard is open on this thread, which is the whole
    // documented precondition.
    unsafe { SetLastError(WIN32_ERROR(ERROR_SUCCESS)) };
    match unsafe { EmptyClipboard() } {
        Ok(()) => {}
        Err(error) => return Err(failed_at("EmptyClipboard", &error)),
    }

    let bytes = buffer_bytes(units);
    // SAFETY: same as above.
    unsafe { SetLastError(WIN32_ERROR(ERROR_SUCCESS)) };
    let handle = match unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) } {
        Ok(handle) => handle,
        // Nothing was allocated, so there is no handle to release. The
        // clipboard stays open; the caller closes it.
        Err(error) => return Err(failed_at("GlobalAlloc", &error)),
    };

    // SAFETY: same as above.
    unsafe { SetLastError(WIN32_ERROR(ERROR_SUCCESS)) };
    // SAFETY: `handle` is a live `GMEM_MOVEABLE` block this process allocated
    // and has not handed to anyone.
    let locked = unsafe { GlobalLock(handle) };
    if locked.is_null() {
        let code = unsafe { GetLastError() }.0;
        // `GlobalLock` failed, so it did not increment the lock count and
        // there is nothing below to balance — unlocking a block that was never
        // locked corrupts the count for whoever locks it next.
        //
        // SAFETY: `handle` is live, unlocked, and still owned by this process,
        // because nothing has taken it.
        unsafe {
            let _ = GlobalFree(Some(handle));
        }
        return Err(ClipboardError::Failed {
            at: "GlobalLock",
            code,
        });
    }

    // SAFETY: `locked` is exactly `bytes` writable bytes, because that is the
    // size requested from `GlobalAlloc`, and `units` is a live slice of exactly
    // `bytes` bytes. The two cannot overlap — `units` lives on this stack
    // frame, `locked` is heap memory the allocator returned — so a
    // non-overlapping copy is the sound one.
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr().cast(), locked, bytes);
    }

    // The lock count went up one in `GlobalLock` and has to come back down
    // here, on this path, before the block is handed away. Leaving it
    // incremented attaches a count to a block the system now owns, and the
    // clipboard's future holders are not promised to read it correctly.
    //
    // SAFETY: `handle` is live and locked exactly once by the call above.
    unsafe {
        let _ = GlobalUnlock(handle);
    }

    // SAFETY: same as above.
    unsafe { SetLastError(WIN32_ERROR(ERROR_SUCCESS)) };
    // SAFETY: the clipboard is open on this thread; `CF_UNICODETEXT` matches
    // how `units` was encoded; and `handle` is an unlocked `GMEM_MOVEABLE`
    // block holding a NUL-terminated UTF-16 string of exactly `bytes` bytes.
    match unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(handle.0))) } {
        // The system owns the memory now. Freeing it here would free the
        // clipboard's own buffer out from under it, and the fault would land in
        // whichever process pastes next — not in this one.
        Ok(_) => Ok(()),
        // The system did not take it, so this process still does. A missed
        // `GlobalFree` leaks a handle per failed copy, and nothing in this
        // process ever touches that handle again, which is exactly why the leak
        // is invisible until the process runs out of handles.
        Err(error) => {
            // SAFETY: `handle` is live, unlocked, and still owned here.
            unsafe {
                let _ = GlobalFree(Some(handle));
            }
            Err(failed_at("SetClipboardData", &error))
        }
    }
}

/// The `GlobalAlloc` size for a NUL-terminated UTF-16 buffer.
///
/// `units` already carries its terminator, so the terminator's own two bytes
/// are inside the count. Asking for one code unit less than the content needs
/// truncates whatever the clipboard hands to the next process; asking for more
/// hands it a block with uninitialised bytes after the NUL, which nobody is
/// promised to skip.
fn buffer_bytes(units: &[u16]) -> usize {
    std::mem::size_of_val(units)
}

/// Wraps a failed Win32 call in the variant that names it.
///
/// The unpacking lives in [`crate::single_instance::last_error_code`] and is
/// shared on purpose: a Win32 code reaches a `windows::core::Error` as
/// `HRESULT_FROM_WIN32(x)`, and reporting that packed value would print
/// 0x80070005 where the reader is expecting to look up 5.
fn failed_at(at: &'static str, error: &windows::core::Error) -> ClipboardError {
    ClipboardError::Failed {
        at,
        code: last_error_code(error),
    }
}

/// Something went wrong writing to the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardError {
    /// A Win32 call in the clipboard sequence failed.
    ///
    /// `at` names the *call* that failed. This field exists because five
    /// different calls make up one "copy to clipboard" from the caller's point
    /// of view, and collapsing them into a single variant made the tray's log
    /// actively misleading for days: it reported a window-creation failure it
    /// had always reported while the real cause was `LoadImageW`, and "tray
    /// message window could not be created" was believed. Three of the five
    /// calls in that log line had nothing to do with creating a window. A wrong
    /// error message is worse than none, because it is believed — see
    /// `docs/ARCHITECTURE.md`, "One error message must name one failure".
    Failed {
        /// Which call failed, e.g. `"OpenClipboard"` or `"SetClipboardData"`.
        at: &'static str,
        /// The raw Win32 error code, as `GetLastError()` reported it.
        code: u32,
    },
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClipboardError::Failed { at, code } => {
                write!(f, "clipboard {at} failed (Win32 error {code})")
            }
        }
    }
}

impl std::error::Error for ClipboardError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `GlobalAlloc` size for `text`, computed the way the Win32 path
    /// computes it: encode, then size.
    fn alloc_len(text: &str) -> usize {
        buffer_bytes(&crate::wide::wide_nul(text))
    }

    #[test]
    fn the_buffer_size_counts_ascii_code_units_and_the_terminator() {
        // A calculator result is ASCII in the common case, so counting the
        // terminator is what stops the classic off-by-one: one unit short and
        // the string the next process reads is not NUL-terminated at all.
        assert_eq!(alloc_len("1234.5"), 14, "6 units of digits plus the NUL");
        assert_eq!(alloc_len("42"), 6);
    }

    #[test]
    fn the_buffer_size_counts_a_bmp_character_as_one_unit() {
        // U+20AC is one UTF-16 unit. Counting characters instead of units gives
        // the same answer here and is wrong for the next test, so both
        // directions are asserted.
        assert_eq!(alloc_len("\u{20AC}9.99"), 12, "5 units plus the NUL");
        assert_eq!(
            alloc_len("\u{00e9}"),
            4,
            "2 units: one character, one terminator"
        );
    }

    #[test]
    fn the_buffer_size_counts_an_astral_character_as_two_units() {
        // U+1F600 is a surrogate pair: two units, one character. Allocating one
        // unit per character would cut the pair to a lone high surrogate, which
        // is malformed UTF-16 and not a payload `CF_UNICODETEXT` can carry.
        // U+10437 is in the same supplementary plane, so it is the same shape.
        assert_eq!(alloc_len("\u{1F600}"), 6, "2 units of pair plus the NUL");
        assert_eq!(alloc_len("\u{10437}"), 6);
        assert_eq!(
            alloc_len("\u{1F600}7+8"),
            12,
            "4 units of pair and digits plus the NUL"
        );
    }

    #[test]
    fn empty_text_still_needs_a_terminated_buffer() {
        // Empty text is not an error and not a no-op, so it still has to
        // produce a buffer the clipboard accepts — the NUL alone. Asking for
        // zero bytes would hand `GlobalAlloc` a block with no string in it.
        assert_eq!(alloc_len(""), 2, "the terminator alone");
        assert_eq!(crate::wide::wide_nul(""), vec![0u16]);
    }

    #[test]
    fn the_encoded_units_are_the_slice_the_copy_reads() {
        // Guards the arithmetic's input, not a Win32 behaviour: the copy in the
        // Win32 path moves exactly as many bytes as `buffer_bytes` reports, so
        // if the encoding ever stopped NUL-terminating the copy and the
        // allocation would still agree and only the pasted string would break.
        let units = crate::wide::wide_nul("a\nb");
        assert_eq!(units, [b'a' as u16, b'\n' as u16, b'b' as u16, 0]);
        assert_eq!(buffer_bytes(&units), 8);
    }

    #[test]
    fn errors_explain_themselves() {
        // One variant, one call name, one code. If this ever stops matching
        // the tray's format, the log stops being greppable across the crate.
        let error = ClipboardError::Failed {
            at: "OpenClipboard",
            code: 5,
        };
        let message = error.to_string();
        assert!(message.contains("OpenClipboard"), "{message}");
        assert!(message.contains('5'), "{message}");
        assert!(message.contains("clipboard"), "{message}");
    }
}
