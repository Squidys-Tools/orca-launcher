//! UTF-16 conversion. Every Win32 string API in this crate is the `W` variant,
//! so this is the single place where the encoding boundary is crossed.
//!
//! Two rules, both learned the hard way elsewhere:
//!
//! * A NUL-terminated buffer must actually contain the terminator. `Vec<u16>`
//!   from a `&str` does not, and passing it to a `W` API is a read past the end
//!   of the vector until it happens to find a zero.
//! * Converting back is *not* symmetric. `String::from_utf16_lossy` will
//!   silently substitute U+FFFD for an unpaired surrogate, and Windows file
//!   names and registry values are not required to be valid UTF-16 text. For
//!   anything that came off disk or out of the registry this module returns
//!   `None` instead of inventing characters, because a launcher showing a
//!   mangled path is worse than one that skips the entry.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// Encodes `text` as a NUL-terminated UTF-16 buffer.
///
/// An interior NUL is not representable in a `&str` path to this function in
/// any way that survives the API, so it is rejected by the caller rather than
/// silently truncating the string.
pub(crate) fn wide_nul(text: &str) -> Vec<u16> {
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    buf.push(0);
    buf
}

/// Encodes a filesystem path as a NUL-terminated UTF-16 buffer.
///
/// Goes through `OsStr` rather than `str` so a path that is not valid Unicode —
/// which Windows permits, and which `to_string_lossy` would corrupt — reaches
/// the API intact.
pub(crate) fn wide_path_nul(path: &Path) -> Vec<u16> {
    let mut buf: Vec<u16> = path.as_os_str().encode_wide().collect();
    buf.push(0);
    buf
}

/// Decodes a NUL-terminated UTF-16 buffer into an `OsString`, losslessly.
///
/// Stops at the first NUL, which is what the Win32 convention means by
/// "NUL-terminated". The result is an `OsString` rather than a `String` because
/// Windows paths are UTF-16 and not necessarily Unicode scalar sequences; this
/// conversion never has to invent a replacement character.
pub(crate) fn os_from_wide(buffer: &[u16]) -> OsString {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    OsString::from_wide(&buffer[..end])
}

/// Decodes a NUL-terminated UTF-16 buffer into a `PathBuf`, losslessly.
pub(crate) fn path_from_wide(buffer: &[u16]) -> PathBuf {
    PathBuf::from(os_from_wide(buffer))
}

/// Decodes a NUL-terminated UTF-16 buffer into a `String`.
///
/// Returns `None` when the buffer is not valid UTF-16 text. Callers that are
/// reading a *display name* — a shortcut's title, an App Paths key — skip the
/// entry on `None` rather than showing replacement characters, because a
/// launcher row that reads `��` is a bug the user has to notice.
///
/// Kept separate from [`os_from_wide`] so the choice is explicit at the call
/// site: a path is always decoded losslessly, a label is not.
pub(crate) fn string_from_wide(buffer: &[u16]) -> Option<String> {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16(&buffer[..end]).ok()
}

/// Copies a NUL-terminated UTF-16 C string into an `OsString`, losslessly.
///
/// For shell APIs that return a bare pointer rather than a slice or a length.
/// The scan is bounded by the terminator the API contract guarantees, so there
/// is no unbounded read; there is simply no way to know the length any other
/// way.
///
/// # Safety
/// `ptr` must be non-null and point at a NUL-terminated UTF-16 buffer that
/// stays valid and unmodified for the duration of the call.
pub(crate) unsafe fn os_from_c_wide(ptr: *const u16) -> OsString {
    if ptr.is_null() {
        return OsString::new();
    }
    // SAFETY: the caller guarantees a NUL terminator, and each probe reads one
    // element at a time starting from `cursor`, so the scan cannot run past it.
    unsafe {
        let mut length = 0usize;
        while *ptr.add(length) != 0 {
            length += 1;
        }
        os_from_wide(std::slice::from_raw_parts(ptr, length))
    }
}

/// Copies `text` into a fixed-size NUL-terminated UTF-16 field of `capacity`
/// code units, truncating on a code-unit boundary.
///
/// The `NOTIFYICONDATA` tip and info fields are fixed arrays, so truncation is
/// not optional. Truncating a surrogate pair in half would produce a lone
/// surrogate in the buffer, which is exactly the malformed UTF-16 the rest of
/// this module refuses to produce — hence the check.
pub(crate) fn copy_wide_fixed(text: &str, out: &mut [u16]) {
    let capacity = out.len();
    debug_assert!(capacity > 0, "fixed UTF-16 field needs room for the NUL");

    let units: Vec<u16> = text.encode_utf16().collect();
    let mut written = 0usize;
    let mut index = 0usize;

    while index < units.len() {
        let unit = units[index];
        // A high surrogate is meaningless without the low surrogate that
        // follows it, so the pair has to be admitted or refused together.
        // `encode_utf16` never emits an unpaired surrogate, so a high surrogate
        // at `index` always has its partner at `index + 1`.
        let is_pair_start = (0xD800..0xDC00).contains(&unit);
        let units_needed = if is_pair_start { 3 } else { 2 };

        // One of those is the NUL terminator, which is not optional.
        if written + units_needed > capacity {
            break;
        }

        out[written] = unit;
        written += 1;
        if is_pair_start {
            if let Some(low) = units.get(index + 1) {
                out[written] = *low;
                written += 1;
                index += 1;
            }
        }
        index += 1;
    }

    out[written] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_nul_appends_exactly_one_terminator() {
        let buf = wide_nul("orca");
        assert_eq!(
            buf,
            vec![b'o' as u16, b'r' as u16, b'c' as u16, b'a' as u16, 0]
        );
    }

    #[test]
    fn wide_nul_of_empty_is_just_the_terminator() {
        assert_eq!(wide_nul(""), vec![0]);
    }

    #[test]
    fn wide_nul_carries_non_ascii_as_utf16() {
        assert_eq!(wide_nul("\u{00e9}"), vec![0xE9, 0]);
    }

    #[test]
    fn os_from_wide_stops_at_the_first_nul() {
        // This is the shape every Win32 string comes back in: the caller's
        // buffer is longer than the string, with the tail zeroed.
        let buffer = [0x6F, 0x72, 0x63, 0x61, 0, 0, 0];
        assert_eq!(os_from_wide(&buffer), OsString::from("orca"));
    }

    #[test]
    fn os_from_wide_accepts_an_unbuffered_slice() {
        // No terminator at all: must not read past the end of the slice.
        assert_eq!(os_from_wide(&[0x6F, 0x6B]), OsString::from("ok"));
    }

    #[test]
    fn os_from_wide_preserves_an_unpaired_surrogate() {
        // 0xD800 with no low surrogate. Path decoding must not lose this, or the
        // path we hand to CreateProcessW is a different path from the one the
        // caller asked for.
        let buffer = [0xD800u16, 0x2E, 0x65, 0x78, 0x65, 0];
        let decoded = os_from_wide(&buffer);
        assert_eq!(
            decoded.encode_wide().collect::<Vec<u16>>()[..2],
            [0xD800, 0x2E]
        );
    }

    #[test]
    fn string_from_wide_rejects_malformed_text_instead_of_replacing_it() {
        assert_eq!(string_from_wide(&[0x6F, 0x6B, 0]), Some("ok".into()));
        assert_eq!(string_from_wide(&[0xD800, 0]), None);
    }

    #[test]
    fn copy_wide_fixed_terminates_and_fits() {
        let mut field = [0xFFFFu16; 8];
        copy_wide_fixed("orca", &mut field);
        assert_eq!(&field[..5], &[0x6F, 0x72, 0x63, 0x61, 0]);
        assert_eq!(&field[5..], &[0xFFFF; 3], "must not write past the NUL");
    }

    #[test]
    fn copy_wide_fixed_truncates_rather_than_overflowing() {
        let mut field = [0xFFFFu16; 5];
        copy_wide_fixed("abcdefgh", &mut field);
        assert_eq!(field[4], 0, "last slot is the NUL");
        assert_eq!(&field[..4], &[0x61, 0x62, 0x63, 0x64]);
    }

    #[test]
    fn copy_wide_fixed_never_splits_a_surrogate_pair() {
        // U+1F600 is a surrogate pair (D83D DE00). A buffer with room for the
        // pair but not for the pair *and* the NUL must refuse both halves:
        // emitting the high surrogate alone would put malformed UTF-16 into the
        // NOTIFYICONDATA buffer.
        let mut cramped = [0xFFFFu16; 2];
        copy_wide_fixed("\u{1F600}", &mut cramped);
        assert_eq!(cramped[0], 0, "only the NUL is written");
        assert_eq!(
            &cramped[1..],
            &[0xFFFF],
            "nothing beyond the NUL is touched"
        );

        // One more unit of room and the pair fits intact, NUL included.
        let mut roomy = [0xFFFFu16; 3];
        copy_wide_fixed("\u{1F600}", &mut roomy);
        assert_eq!(roomy, [0xD83D, 0xDE00, 0]);
    }

    #[test]
    fn wide_path_nul_round_trips() {
        let path = Path::new(r"C:\Program Files\orca\orca.exe");
        let decoded = path_from_wide(&wide_path_nul(path));
        assert_eq!(decoded, path);
    }
}
