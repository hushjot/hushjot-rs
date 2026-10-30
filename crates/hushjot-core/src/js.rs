//! JavaScript string semantics, for the derivations that must match the app.
//!
//! The app is TypeScript, so `content.length`, `slice`, `trim` and the regex
//! `\s` all mean what JavaScript says they mean: lengths in **UTF-16 code
//! units**, and a whitespace set that includes U+FEFF and leaves out U+0085.
//! A title cut at 120 `char`s instead of 120 UTF-16 units would differ for any
//! note whose first line holds an emoji, so the ports go through these helpers
//! rather than through `str::len` or `str::trim`.
//!
//! One divergence is unavoidable and deliberate. JavaScript will happily cut a
//! surrogate pair in half and keep the lone high surrogate; a Rust `String`
//! cannot hold one. [`slice_utf16`] therefore drops a pair the cut would split,
//! which makes the result one code unit shorter than the app's in that case.
//! The app then seals that lone surrogate as U+FFFD, so the two clients already
//! disagree about such a title by the time it is stored; nothing here makes that
//! worse.

/// Whether `c` is whitespace by JavaScript's definition (`WhiteSpace` and
/// `LineTerminator` in ECMA-262), which is what `trim()` and `\s` use.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// The JavaScript whitespace set as a regex character-class body, for patterns
/// ported from the app that use `\s`.
pub const JS_WS_CLASS: &str =
    r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// `String.prototype.trim`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `String.prototype.trimStart`.
pub fn trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_whitespace)
}

/// `String.prototype.length`: UTF-16 code units.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s.slice(0, end)` where `end` counts UTF-16 code units. A surrogate pair the
/// cut would split is dropped entirely (see the module notes).
pub fn slice_utf16(s: &str, end: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        let next = units + c.len_utf16();
        if next > end {
            return &s[..i];
        }
        units = next;
    }
    s
}

/// `String.prototype.toLowerCase`, which applies Unicode's default case
/// mapping exactly as Rust's `to_lowercase` does.
pub fn to_lower(s: &str) -> String {
    s.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_lengths_count_surrogate_pairs_twice() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("é"), 1);
        assert_eq!(utf16_len("🔐"), 2);
        assert_eq!(utf16_len("日本語"), 3);
    }

    #[test]
    fn slicing_counts_utf16_units_and_never_splits_a_pair() {
        assert_eq!(slice_utf16("hello", 3), "hel");
        assert_eq!(slice_utf16("hello", 10), "hello");
        assert_eq!(slice_utf16("a🔐b", 3), "a🔐");
        assert_eq!(slice_utf16("a🔐b", 2), "a");
        assert_eq!(slice_utf16("", 0), "");
    }

    #[test]
    fn trim_uses_the_javascript_whitespace_set() {
        assert_eq!(trim("\u{FEFF} x \u{3000}"), "x");
        // U+0085 is whitespace to Rust but not to JavaScript.
        assert_eq!(trim("\u{0085}x"), "\u{0085}x");
        assert_eq!(trim_start("  x  "), "x  ");
    }
}
