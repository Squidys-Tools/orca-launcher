//! Case folding and word segmentation, shared by the matcher and the ranker.
//!
//! Everything here is a pure function over `&str`. It exists as its own module
//! because *how* text is normalised is the single decision that most ranking
//! bugs come from, and it needs to be inspectable in one place.
//!
//! # What normalisation does and does not do
//!
//! * **Case is folded.** `to_lowercase` is applied, which handles non-ASCII
//!   correctly. It is Rust's *simple* Unicode lowercase mapping, not full case
//!   folding, so `ß` stays `ß` and `straße` does not match `STRASSE`. That is
//!   asserted in the tests below: a user who types `strasse` and sees nothing has
//!   typed an `ss` where the name has a `ß`, and quietly guessing otherwise
//!   would be worse than the miss.
//! * **Unicode is not normalised.** Folding `é` to `e` would make `café` and
//!   `cafe` the same word, which is a guess about user intent. A user who
//!   types `cafe` and sees nothing should learn to type the accent, not have
//!   the launcher silently decide for them. Precomposed vs. decomposed
//!   spellings of the *same* character (`é` as one code point vs. `e` + U+0301)
//!   therefore do not match each other; that is the documented cost of not
//!   pulling in a normalisation table.
//!
//! Every index downstream is a `char` index, never a byte offset. A `char` is
//! the unit a user perceives, and a byte offset into a folded string is wrong
//! for any non-ASCII input — silently, because a misaligned slice simply fails
//! to match anything.
//!
//! Whitespace *inside a query* is dropped before matching. A spec copied out of
//! a settings UI (`" not pad "`) should land on the same band as the unspaced
//! equivalent. Whitespace inside a candidate is kept, because it separates
//! words and word boundaries are one of the match tiers.

/// Folds a string for comparison: trimmed, lowercased, returned as `char`s.
///
/// Returning `Vec<char>` rather than `String` is the whole point. A `char` is
/// the unit a user perceives and the unit every downstream index must be, and
/// it is the only unit that survives a multi-byte character: a byte index into a
/// lowercased string is wrong for every non-ASCII input, and wrong *silently*,
/// because a misaligned slice still compares equal to nothing and the code just
/// reports "no match".
#[cfg(test)]
pub(crate) fn fold(value: &str) -> Vec<char> {
    let mut out = Vec::new();
    fold_into(value, &mut out);
    out
}

/// [`fold`], into a buffer the caller reuses.
///
/// The ranker folds two strings per candidate per keystroke, so allocating a
/// `String` and a `Vec<char>` for each was most of what a keystroke cost. ASCII
/// takes a direct path: for ASCII input `to_ascii_lowercase` and
/// `to_lowercase` are the same mapping, so the two paths cannot disagree, and
/// most titles and paths are ASCII. Anything else goes through `to_lowercase`,
/// whose context-sensitive rules (the final sigma) a per-`char` loop would get
/// wrong.
pub(crate) fn fold_into(value: &str, out: &mut Vec<char>) {
    out.clear();
    let trimmed = value.trim();
    if trimmed.is_ascii() {
        out.extend(
            trimmed
                .bytes()
                .map(|byte| char::from(byte.to_ascii_lowercase())),
        );
    } else {
        out.extend(trimmed.to_lowercase().chars());
    }
}

/// Folds a *query*: as [`fold`], then drops every whitespace character.
///
/// A query is what the user typed one key at a time, and spaces in it are
/// almost always a slip rather than intent.
pub(crate) fn fold_query(value: &str) -> Vec<char> {
    value
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Whether `c` counts as part of a "word" for boundary detection.
///
/// Underscore counts, so `my_app_name` is one word rather than three — that is
/// what a programmer means by it. Every other non-alphanumeric character
/// (space, `-`, `.`, `/`, `\`, `:`, `+`) is a separator, which is what makes
/// `notepad.exe` searchable by `exe` and `C:\Users` by `users`.
///
/// `is_alphanumeric` rather than `is_ascii_alphanumeric`, so `42` and `café`
/// are both words — a launcher that could not find a file because its name
/// starts with an accent would be worse than useless.
pub(crate) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_trims_and_lowercases() {
        assert_eq!(
            fold("  Notepad.EXE  "),
            "notepad.exe".chars().collect::<Vec<_>>()
        );
        assert_eq!(fold(""), Vec::<char>::new());
    }

    #[test]
    fn fold_query_also_drops_interior_whitespace() {
        assert_eq!(fold_query(" ep ad "), "epad".chars().collect::<Vec<_>>());
        assert_eq!(fold_query("   "), Vec::<char>::new());
        assert_eq!(fold_query("\t\n"), Vec::<char>::new());
    }

    #[test]
    fn fold_counts_chars_not_bytes() {
        // ASCII is 1:1, and every non-ASCII case below is *not* byte-counted.
        // This is the property the whole matcher rests on: an index into a
        // folded string must be a char index, because the byte offsets of
        // anything non-ASCII are not where a reader would put a boundary.
        assert_eq!(fold("abc").len(), 3);
        assert_eq!(fold("abc").len(), "abc".len());
        assert_eq!(fold("日本語").len(), 3);
        assert_ne!(fold("日本語").len(), "日本語".len());
        assert_eq!(fold("café").len(), 4);
        assert_eq!(fold("🎉").len(), 1);
    }

    #[test]
    fn fold_is_simple_lowercasing_which_is_what_rust_gives() {
        // `to_lowercase` is the *simple* Unicode lowercase mapping, so the
        // lengths here match the input and `ß` does not expand to `ss`. The
        // matching module asserts the consequence for the user; this test
        // records the fact it rests on.
        assert_eq!(fold("ABC"), "abc".chars().collect::<Vec<_>>());
        assert_eq!(fold("ÀÉÎ"), "àéî".chars().collect::<Vec<_>>());
        // Simple, not full: a full case fold would turn `ß` into `ss`.
        assert_eq!(fold("ß"), "ß".chars().collect::<Vec<_>>());
    }

    #[test]
    fn the_ascii_fast_path_agrees_with_full_lowercasing() {
        // The fast path is only sound if it is indistinguishable from the slow
        // one. Mixed, padded, punctuated, and non-ASCII inputs, including the
        // final sigma that a per-char lowercase would get wrong.
        let mut buffer = vec!['x'; 8];
        for value in [
            "",
            "  Notepad.EXE  ",
            r"C:\Users\Chris\Downloads\Report-2026.XLSX",
            "MiXeD_case 42",
            "Café Notes",
            "ΟΔΟΣ",
            " 日本語 ",
        ] {
            fold_into(value, &mut buffer);
            let expected: Vec<char> = value.trim().to_lowercase().chars().collect();
            assert_eq!(buffer, expected, "{value:?}");
        }
    }

    #[test]
    fn is_word_char_separates_identifiers_from_delimiters() {
        for c in ['a', 'Z', '7', 'é', '9', '_'] {
            assert!(is_word_char(c), "{c:?} should be a word char");
        }
        for c in [' ', '-', '.', '/', '\\', ':', '+', '@', '\t'] {
            assert!(!is_word_char(c), "{c:?} should be a separator");
        }
    }
}
