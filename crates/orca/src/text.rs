//! Caret arithmetic across the UTF-16 / UTF-8 boundary.
//!
//! # Why this module exists
//!
//! `Window::handle_input` and every method on [`gpui::EntityInputHandler`]
//! speak **UTF-16 code units**, because that is what the Win32 text services
//! and the IME speak. The model — the `query` string and its `caret` — is a
//! Rust `String` and a `usize`, so its indices are **UTF-8 byte offsets**.
//!
//! For ASCII the two coincide and everything works, which is exactly why the
//! bug is cheap to ship and expensive to notice. For anything else they
//! diverge immediately: `é` is 1 UTF-16 unit but 2 UTF-8 bytes, and `😀` is
//! 2 UTF-16 units but 4 UTF-8 bytes. A caret index carried across that
//! boundary without conversion lands mid-character, which is not a rendering
//! glitch — `str::replace_range` on a non-boundary index **panics**.
//!
//!
//! So the conversion happens here, at the boundary, and everywhere else in the
//! crate `caret` unambiguously means a UTF-8 byte offset.
//!
//! # The one rule
//!
//! **Every range that crosses into [`gpui::EntityInputHandler`] is UTF-16.
//! Every range that stays inside the model is UTF-8 bytes.** There is no third
//! space and no flag saying which is which, because a flag would eventually be
//! set wrong.
//!
//! # Why the edits are free functions
//!
//! [`apply`] and [`apply_and_mark`] used to be `EntityInputHandler` methods on
//! the widget. They are here instead because that is where they can be
//! *tested*: every method on that trait takes a `&mut Window` and a
//! `&mut Context<_>`, and GPUI only hands those out through its `test-support`
//! feature, which pulls in the Wayland and X11 backends. These are pure
//! transitions over three fields, and a pure transition does not need a window
//! to be trustworthy — it needs to be callable.
//!
//! For the same reason the worked examples below are written as prose: `orca` is
//! a binary crate, so `cargo test` never compiles this file's doc examples, and
//! a `use orca::text::…` in one would look checked without ever being checked.
//! Every one of those values is asserted in the tests at the bottom of the file.
//!
//! # Rounding
//!
//! A UTF-16 index can land *inside* a character — that is what the middle of a
//! surrogate pair is. There is no correct answer, so both directions snap to
//! the start of the character containing the index. Snapping forward would put
//! the caret after a character the user cannot see half of; snapping back puts
//! it before one, which is at least a position the user typed towards.

use std::ops::Range;

/// Converts a UTF-16 index into a UTF-8 byte offset in `text`.
///
/// Returns `0` for index `0`, `text.len()` for an index at or past the end,
/// and otherwise the byte offset of the character that *contains or starts at*
/// `u16_index`.
///
/// `u16_to_byte("note", 2) == 2`; `u16_to_byte("café", 3) == 3` and
/// `u16_to_byte("café", 4) == 5`, because "é" is one UTF-16 unit and two bytes;
/// `u16_to_byte("a😀b", 1) == 1` and `u16_to_byte("a😀b", 3) == 5`, because
/// "😀" is two UTF-16 units and four bytes, so index 1 is inside it.
#[must_use]
pub fn u16_to_byte(text: &str, u16_index: usize) -> usize {
    if u16_index == 0 {
        return 0;
    }
    let mut units = 0usize;
    for (byte, ch) in text.char_indices() {
        // The target is at or after this character starts.
        if units >= u16_index {
            return byte;
        }
        units += ch.len_utf16();
        // The target fell strictly inside this character — a split surrogate
        // pair, or an index that simply overshot a short string. Snap back to
        // this character's start rather than past it.
        if units > u16_index {
            return byte;
        }
    }
    text.len()
}

/// Converts a UTF-8 byte offset in `text` into a UTF-16 index.
///
/// A byte offset that is not a character boundary snaps back to the enclosing
/// character's start; an offset past the end clamps to the end.
///
/// `byte_to_u16("note", 2) == 2`; `byte_to_u16("café", 4) == 3`;
/// `byte_to_u16("a😀b", 5) == 3`.
#[must_use]
pub fn byte_to_u16(text: &str, byte_index: usize) -> usize {
    let mut byte = byte_index.min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    text[..byte].chars().map(char::len_utf16).sum()
}

/// Moves the caret back one character, or to `0` at the start of the text.
///
/// A `byte` that is not a character boundary is first snapped back to the
/// enclosing character's start, and the step is taken from there — so this can
/// neither panic nor land mid-character, and it never moves *forward`.
///
/// `prev_boundary("café", 5) == 3`; `prev_boundary("café", 4) == 2`, because 4
/// is inside "é" so it normalises to 3 and then steps back over "f";
/// `prev_boundary("abc", 0) == 0`.
#[must_use]
pub fn prev_boundary(text: &str, byte: usize) -> usize {
    let byte = clamp_boundary(text, byte);
    text[..byte]
        .char_indices()
        .next_back()
        .map_or(0, |(start, _)| start)
}

/// The start of the character after `byte`, or `text.len()` at the end.
///
/// A `byte` that is not a character boundary snaps back to the enclosing
/// character's start, so this cannot panic on a mid-character index.
///
/// `next_boundary("café", 3) == 5` and `next_boundary("café", 4) == 5`, because
/// "é" is two bytes, so the boundary after it is at 5 and not 4;
/// `next_boundary("abc", 3) == 3`.
#[must_use]
pub fn next_boundary(text: &str, byte: usize) -> usize {
    let byte = clamp_boundary(text, byte);
    match text[byte..].chars().next() {
        Some(ch) => byte + ch.len_utf8(),
        None => text.len(),
    }
}

/// Clamps `byte` into `text` and onto a character boundary.
///
/// Every stored index goes through this on the way in, so no later code has to
/// re-check it before calling a `&str` range method.
#[must_use]
pub fn clamp_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while byte > 0 && !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

/// The three fields an incoming edit can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The new text.
    pub query: String,
    /// The new caret, a UTF-8 byte offset.
    pub caret: usize,
    /// The new in-progress composition, a UTF-8 byte range. `None` when there
    /// is no composition.
    pub composing: Option<Range<usize>>,
}

/// The byte range an incoming edit applies to.
///
/// An explicit `range` wins, then the IME's marked range, then a collapsed caret.
/// The IME replaces what it marked and only falls back to the caret when it has
/// marked nothing, so the order is not a preference.
///
/// `range` is UTF-16 because it came from `EntityInputHandler`. An IME does not
/// send a range that splits a surrogate pair, but a stale one from a previous
/// composition can, and [`u16_to_byte`] snaps that to a character start rather
/// than to a byte offset that would panic the `replace_range` behind it.
#[must_use]
pub fn target(
    query: &str,
    caret: usize,
    composing: Option<&Range<usize>>,
    range: Option<Range<usize>>,
) -> Range<usize> {
    match range {
        Some(range) => {
            let start = u16_to_byte(query, range.start);
            let end = u16_to_byte(query, range.end);
            start..end.max(start)
        }
        None => composing.map_or(caret..caret, Range::clone),
    }
}

/// Replaces the targeted span with `new_text` and ends any composition.
///
/// The caret lands after the inserted text, clamped onto a boundary. Clamping
/// rather than trusting the arithmetic is what keeps a shorter replacement — a
/// backspace arriving as an IME commit, say — from leaving a caret past the end.
#[must_use]
pub fn apply(
    query: &str,
    caret: usize,
    composing: Option<&Range<usize>>,
    range: Option<Range<usize>>,
    new_text: &str,
) -> Edit {
    let target = target(query, caret, composing, range);
    let mut next = String::with_capacity(query.len() + new_text.len());
    next.push_str(&query[..target.start]);
    next.push_str(new_text);
    next.push_str(&query[target.end..]);

    Edit {
        caret: clamp_boundary(&next, target.start + new_text.len()),
        query: next,
        composing: None,
    }
}

/// Replaces the targeted span with `new_text` and marks it as composing.
///
/// `selection` is the IME's replacement cursor for the marked text, in UTF-16,
/// and is measured against the *new* text. It is applied after the marked range
/// is recorded because an IME that supplies one has already positioned itself.
#[must_use]
pub fn apply_and_mark(
    query: &str,
    caret: usize,
    composing: Option<&Range<usize>>,
    range: Option<Range<usize>>,
    new_text: &str,
    selection: Option<Range<usize>>,
) -> Edit {
    let target = target(query, caret, composing, range);
    let mut next = String::with_capacity(query.len() + new_text.len());
    next.push_str(&query[..target.start]);
    next.push_str(new_text);
    next.push_str(&query[target.end..]);

    let marked = if new_text.is_empty() {
        None
    } else {
        Some(target.start..target.start + new_text.len())
    };
    let caret = match selection {
        Some(selection) => u16_to_byte(&next, selection.start),
        None => clamp_boundary(&next, target.start + new_text.len()),
    };

    Edit {
        query: next,
        caret,
        composing: marked,
    }
}

/// Deletes the character before `caret`, leaving the string and caret in step.
///
/// A no-op at the start of the text, and a no-op while `composing` is a
/// non-empty range: the IME owns that text and will replace it through
/// [`apply`], so deleting here as well would remove two characters for one
/// keypress.
pub fn backspace(query: &mut String, caret: &mut usize, composing: Option<&Range<usize>>) {
    let ime_owns_the_text = composing.is_some_and(|range| !range.is_empty());
    if *caret == 0 || ime_owns_the_text {
        return;
    }
    let start = prev_boundary(query, *caret);
    query.replace_range(start..*caret, "");
    *caret = start;
}

/// Deletes the character after `caret`, leaving the string and caret in step.
pub fn delete_forward(query: &mut String, caret: &mut usize) {
    if *caret >= query.len() {
        return;
    }
    let end = next_boundary(query, *caret);
    query.replace_range(*caret..end, "");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every string a user can plausibly type, with the byte lengths and UTF-16
    /// lengths that make the two spaces differ.
    const SAMPLES: [&str; 6] = ["", "note", "café", "日本語", "a😀b", "mixed é 日本 😀 end"];

    #[test]
    fn ascii_is_the_same_in_both_spaces_so_the_casual_reading_holds() {
        assert_eq!(u16_to_byte("note", 0), 0);
        assert_eq!(u16_to_byte("note", 3), 3);
        assert_eq!(u16_to_byte("note", 4), 4);
        assert_eq!(byte_to_u16("note", 3), 3);
    }

    #[test]
    fn a_multi_byte_character_shifts_the_two_index_spaces_apart() {
        // "é" is 1 UTF-16 unit at bytes 3..5.
        assert_eq!("café".find('é'), Some(3));
        assert_eq!(u16_to_byte("café", 3), 3);
        assert_eq!(u16_to_byte("café", 4), 5);
        assert_eq!(byte_to_u16("café", 3), 3);
        assert_eq!(byte_to_u16("café", 5), 4);
    }

    #[test]
    fn a_surrogate_pair_snaps_to_the_character_start_not_past_it() {
        // "😀" occupies bytes 1..5 and UTF-16 units 1..3. Index 2 is inside it.
        assert_eq!(u16_to_byte("a😀b", 2), 1, "must snap back, not forward");
        assert_eq!(u16_to_byte("a😀b", 1), 1);
        assert_eq!(u16_to_byte("a😀b", 3), 5);
    }

    #[test]
    fn round_tripping_through_both_spaces_lands_where_it_started() {
        for text in SAMPLES {
            let units = text.chars().map(char::len_utf16).sum::<usize>();
            for index in 0..=units {
                // Walking forward: every UTF-16 index is either a real caret
                // position or inside a surrogate pair, and both must map to a
                // valid byte offset.
                let byte = u16_to_byte(text, index);
                assert!(
                    text.is_char_boundary(byte),
                    "{text:?} index {index} produced non-boundary byte {byte}"
                );
            }
            // Walking backward from every real byte boundary.
            for byte in (0..=text.len()).filter(|b| text.is_char_boundary(*b)) {
                let index = byte_to_u16(text, byte);
                assert!(
                    index <= units,
                    "{text:?} byte {byte} produced out-of-range unit {index}"
                );
            }
        }
    }

    #[test]
    fn a_real_caret_position_survives_both_directions() {
        for text in SAMPLES {
            for (byte, _) in text.char_indices() {
                let index = byte_to_u16(text, byte);
                assert_eq!(
                    u16_to_byte(text, index),
                    byte,
                    "{text:?} lost byte offset {byte} on the round trip"
                );
            }
            let end = byte_to_u16(text, text.len());
            assert_eq!(u16_to_byte(text, end), text.len());
        }
    }

    #[test]
    fn out_of_range_indices_clamp_rather_than_panic() {
        for text in SAMPLES {
            let units = text.chars().map(char::len_utf16).sum::<usize>();
            assert_eq!(u16_to_byte(text, units + 50), text.len());
            assert_eq!(byte_to_u16(text, text.len() + 50), units);
        }
    }

    #[test]
    fn boundary_walkers_step_one_character_at_a_time() {
        // "café" is c@0 a@1 f@2 é@3..5, so the boundaries are 0 1 2 3 5.
        assert_eq!(prev_boundary("café", 5), 3);
        assert_eq!(prev_boundary("café", 3), 2);
        assert_eq!(
            prev_boundary("café", 4),
            2,
            "a mid-character index normalises first"
        );
        assert_eq!(prev_boundary("abc", 1), 0);
        assert_eq!(prev_boundary("abc", 0), 0);
        assert_eq!(next_boundary("café", 0), 1);
        assert_eq!(next_boundary("café", 3), 5);
        assert_eq!(next_boundary("abc", 3), 3);
    }

    #[test]
    fn walking_backwards_over_an_emoji_deletes_the_whole_thing() {
        // The bug this module exists to prevent: a backspace that removes one
        // byte of a four-byte emoji, leaving an unprintable fragment. The caret
        // starts *inside* the emoji rather than at the end of the string, so
        // the emoji really is the character under the cursor.
        let mut text = String::from("a😀b");
        let mut caret = 5;
        let start = prev_boundary(&text, caret);
        text.replace_range(start..caret, "");
        caret = start;
        assert_eq!(text, "ab");
        assert_eq!(byte_to_u16(&text, caret), 1);
    }

    #[test]
    fn every_caret_step_over_every_sample_lands_on_a_boundary() {
        // Walk each sample forwards and backwards one step at a time, checking
        // the invariant after every step rather than only at the ends. This is
        // the property the UI relies on: these are called on every keypress,
        // and a `str` range method panics on a mid-character index.
        for text in SAMPLES {
            let mut caret = 0usize;
            while next_boundary(text, caret) != caret {
                caret = next_boundary(text, caret);
                assert!(text.is_char_boundary(caret), "{text:?} at {caret}");
            }
            while prev_boundary(text, caret) != caret {
                caret = prev_boundary(text, caret);
                assert!(text.is_char_boundary(caret), "{text:?} at {caret}");
            }
            assert_eq!(caret, 0, "walking back to the start must arrive at 0");
        }
    }

    #[test]
    fn clamp_boundary_never_returns_a_mid_character_offset() {
        for text in SAMPLES {
            for byte in 0..=text.len() {
                let clamped = clamp_boundary(text, byte);
                assert!(text.is_char_boundary(clamped), "{text:?} at {byte}");
                assert!(clamped <= byte || byte == text.len());
            }
        }
    }

    // ------------------------------------------------------------- the edits
    //
    // Everything above proves the two conversions agree. What follows proves
    // they are actually *used*: these are the exact transitions the popup runs
    // on every keystroke and every IME commit, and each one ends in a
    // `String::replace_range` that panics on a non-boundary index.

    fn units(text: &str) -> usize {
        text.chars().map(char::len_utf16).sum()
    }

    /// Replaces the whole of `query` with `new_text`, the way select-all does.
    #[test]
    fn select_all_by_utf16_range_clears_multibyte_text_whole() {
        for text in SAMPLES {
            let edit = apply(text, text.len(), None, Some(0..units(text)), "");
            assert_eq!(edit.query, "", "for {text:?}");
            assert_eq!(edit.caret, 0);
        }
    }

    /// The emoji is bytes 1..5 and UTF-16 units 1..3, so `replace_range(1..3, "")`
    /// is the exact index pair that panicked before the conversion existed.
    #[test]
    fn a_range_covering_a_surrogate_pair_removes_the_whole_emoji() {
        let edit = apply("a😀b", 5, None, Some(1..3), "");
        assert_eq!(edit.query, "ab");
        assert_eq!(edit.caret, 1);
    }

    #[test]
    fn a_range_that_splits_a_surrogate_pair_deletes_nothing_rather_than_half() {
        // UTF-16 1..2 is the two halves of one emoji. There is no correct answer,
        // and the documented one is to snap back to the character's start, which
        // makes the range empty. Deleting half a character would leave a
        // replacement character on screen.
        let edit = apply("a😀b", 5, None, Some(1..2), "");
        assert_eq!(edit.query, "a😀b");
        assert_eq!(edit.caret, 1);
    }

    /// The property the UI depends on: whatever the IME asks for, the edit does
    /// not panic, the caret comes back as a real caret position, and only the
    /// requested span changes.
    #[test]
    fn no_combination_of_text_index_and_replacement_can_panic_or_misplace_the_caret() {
        for text in SAMPLES {
            for start in 0..=units(text) {
                for end in start..=units(text) {
                    for replacement in ["", "x", "é", "😀", "日本"] {
                        let edit = apply(text, text.len(), None, Some(start..end), replacement);
                        let message = format!(
                            "replacing utf16 {start}..{end} of {text:?} with {replacement:?}"
                        );

                        assert!(edit.query.is_char_boundary(edit.caret), "{message}");
                        assert!(
                            edit.caret <= edit.query.len(),
                            "{message}: the caret ran past the end of {:?}",
                            edit.query
                        );
                        assert_eq!(edit.composing, None, "{message}: a commit ends it");

                        // The span the caller named, in bytes, is the span that
                        // goes. The text on either side of it is untouched, which
                        // is the part a mis-snapped surrogate pair would break.
                        let mut expected = String::from(&text[..u16_to_byte(text, start)]);
                        expected.push_str(replacement);
                        expected.push_str(&text[u16_to_byte(text, end)..]);
                        assert_eq!(edit.query, expected, "{message}");
                    }
                }
            }
        }
    }

    /// A marked range is the fallback target when the IME sends no explicit one,
    /// which is how a commit works.
    #[test]
    fn an_unmarked_commit_replaces_exactly_the_composed_text() {
        let marked = 2..8;
        let edit = apply("ab日本x", 8, Some(&marked), None, "語");
        assert_eq!(edit.query, "ab語x");
        assert_eq!(edit.caret, 5);
        assert_eq!(edit.composing, None, "a commit ends the composition");
    }

    #[test]
    fn an_explicit_range_beats_the_marked_one() {
        // The IME is replacing something it marked *and* naming a range, which
        // happens when it takes over a selection. The named range wins.
        let marked = 0..1;
        let edit = apply("abc", 3, Some(&marked), Some(1..2), "Z");
        assert_eq!(edit.query, "aZc");
    }

    #[test]
    fn a_non_ascii_composition_is_marked_and_commits_at_the_right_offset() {
        // The two-step an IME actually performs, in order.
        let marked = apply_and_mark("ab", 2, None, None, "日本", None);
        assert_eq!(marked.query, "ab日本");
        assert_eq!(marked.caret, 8, "two CJK characters are six bytes");
        assert_eq!(marked.composing, Some(2..8));

        let committed = apply(
            "ab日本",
            marked.caret,
            marked.composing.as_ref(),
            None,
            "日本",
        );
        assert_eq!(committed.query, "ab日本");
        assert_eq!(committed.caret, 8);
        assert_eq!(committed.composing, None);
    }

    #[test]
    fn a_composition_after_existing_multibyte_text_keeps_the_offsets_right() {
        // "éx" is three bytes and two UTF-16 units, so the mark belongs at byte
        // 3. Computing it in UTF-16 would put it at 2, inside the "é", and the
        // commit would then eat the "x" as well — which is the failure this
        // asserts against by naming the whole expected string.
        let marked = apply_and_mark("éx", 3, None, None, "日本", None);
        assert_eq!(marked.query, "éx日本");
        assert_eq!(marked.composing, Some(3..9));

        let committed = apply(
            &marked.query,
            marked.caret,
            marked.composing.as_ref(),
            None,
            "語",
        );
        assert_eq!(committed.query, "éx語");
        assert_eq!(committed.caret, 6);
    }

    #[test]
    fn an_empty_composition_deletes_what_was_marked() {
        // The IME backed out of what it had marked and sent the empty string
        // over the same span, so the span goes.
        let edit = apply_and_mark("ab", 2, Some(&(1..2)), None, "", None);
        assert_eq!(edit.query, "a");
        assert_eq!(edit.caret, 1);
        assert_eq!(
            edit.composing, None,
            "nothing typed means nothing to commit"
        );
    }

    #[test]
    fn an_ime_supplied_selection_is_honoured_in_utf16() {
        // The IME has already placed its cursor; it tells us where in UTF-16 and
        // expects the same space back.
        let edit = apply_and_mark("ab", 2, None, None, "日本", Some(0..2));
        assert_eq!(edit.query, "ab日本");
        assert_eq!(edit.caret, 0, "the IME asked for the very start");
        assert_eq!(edit.composing, Some(2..8));
    }

    // ------------------------------------------------------------- deleting

    #[test]
    fn backspace_removes_a_whole_multibyte_character() {
        for (text, caret, expected) in [
            ("a😀", 5usize, "a"),
            ("café", 5, "caf"),
            ("日本", 6, "日"),
            ("é", 2, ""),
        ] {
            let mut query = text.to_owned();
            let mut caret = caret;
            backspace(&mut query, &mut caret, None);
            assert_eq!(query, expected, "backspace in {text:?}");
            assert!(query.is_char_boundary(caret), "{query:?} at {caret}");
        }
    }

    #[test]
    fn delete_forward_removes_a_whole_multibyte_character() {
        for (text, caret, expected) in [("é日", 0usize, "日"), ("😀ab", 0, "ab"), ("a😀", 1, "a")]
        {
            let mut query = text.to_owned();
            let mut caret = caret;
            delete_forward(&mut query, &mut caret);
            assert_eq!(query, expected, "delete in {text:?} at {caret}");
        }
    }

    #[test]
    fn deleting_at_either_end_does_nothing() {
        let mut query = String::from("日本");
        let mut caret = 0;
        backspace(&mut query, &mut caret, None);
        assert_eq!(query, "日本");
        assert_eq!(caret, 0);

        caret = 6;
        delete_forward(&mut query, &mut caret);
        assert_eq!(query, "日本");
        assert_eq!(caret, 6);
    }

    #[test]
    fn backspace_leaves_text_the_ime_owns_alone() {
        // The IME will replace what it marked. Deleting here as well removes two
        // characters for one keypress, which reads as dropped input.
        let mut query = String::from("ab日本");
        let mut caret = 8;
        backspace(&mut query, &mut caret, Some(&(2..8)));
        assert_eq!(query, "ab日本");
        assert_eq!(caret, 8);
    }

    #[test]
    fn backspace_still_works_when_the_ime_marked_an_empty_range() {
        // "日" starts at byte 2, so the caret lands at 2, not 3.
        let mut query = String::from("ab日");
        let mut caret = 5;
        backspace(&mut query, &mut caret, Some(&(5..5)));
        assert_eq!(query, "ab");
        assert_eq!(caret, 2);
    }

    /// Walking the caret across every sample and deleting at every position
    /// either way must never panic and never leave a fragment behind.
    #[test]
    fn deleting_from_every_position_of_every_sample_stays_on_a_boundary() {
        for text in SAMPLES {
            for caret in (0..=text.len()).filter(|b| text.is_char_boundary(*b)) {
                let mut query = text.to_owned();
                let mut at = caret;
                backspace(&mut query, &mut at, None);
                assert!(
                    query.is_char_boundary(at),
                    "backspace in {text:?} at {caret}"
                );

                let mut query = text.to_owned();
                let mut at = caret;
                delete_forward(&mut query, &mut at);
                assert!(query.is_char_boundary(at), "delete in {text:?} at {caret}");
                // A partial character is the specific thing that must not
                // survive: every remaining character is one the user typed.
                assert!(
                    query.chars().all(|ch| ch != '\u{fffd}'),
                    "delete in {text:?} at {caret} left a replacement character"
                );
            }
        }
    }
}
