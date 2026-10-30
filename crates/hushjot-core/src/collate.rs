//! Natural, case- and accent-insensitive ordering of titles, tags and folder
//! names.
//!
//! The app uses `Intl.Collator(undefined, { numeric: true, sensitivity:
//! 'base' })`: "apple" sits with "Apple", "Café" equals "cafe", and "note 2"
//! comes before "note 10". Pulling in a full ICU collator for that would be the
//! largest dependency in the crate, so this is a deliberately small
//! approximation of the root collation:
//!
//! - letters compare by their base form (decomposed, combining marks dropped,
//!   lowercased);
//! - runs of ASCII digits compare as numbers;
//! - whitespace and punctuation sort before digits, digits before letters, as
//!   in ICU.
//!
//! It agrees with ICU on everything a note list is likely to hold. Where it
//! might not — ligatures, scripts with contractions — the only effect is the
//! order of a list on this device; nothing is stored, synced or compared across
//! clients through it. Equality ([`equal`]) is what note links use to resolve a
//! title, and for that the base-form rule is the one that matters.

use std::cmp::Ordering;

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Unit {
    /// Whitespace and punctuation, by code point.
    Other(char),
    /// A run of digits: (length without leading zeros, digits without them).
    Number(usize, String),
    /// A letter (or any other alphanumeric), in base form.
    Letter(char),
}

fn units(s: &str) -> Vec<Unit> {
    let base: Vec<char> = s.nfd().filter(|c| !is_combining_mark(*c)).flat_map(char::to_lowercase).collect();
    let mut out = Vec::with_capacity(base.len());
    let mut i = 0;
    while i < base.len() {
        let c = base[i];
        if c.is_ascii_digit() {
            let start = i;
            while i < base.len() && base[i].is_ascii_digit() {
                i += 1;
            }
            let digits: String = base[start..i].iter().collect();
            let trimmed = digits.trim_start_matches('0').to_string();
            out.push(Unit::Number(trimmed.len(), trimmed));
            continue;
        }
        out.push(if c.is_alphanumeric() { Unit::Letter(c) } else { Unit::Other(c) });
        i += 1;
    }
    out
}

/// Compare two strings the way the app's collator does (see the module notes).
pub fn compare(a: &str, b: &str) -> Ordering {
    units(a).cmp(&units(b))
}

/// Whether two strings are equal at base strength: case and accents ignored,
/// `"007"` equal to `"7"`.
pub fn equal(a: &str, b: &str) -> bool {
    compare(a, b) == Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_and_accents_are_ignored() {
        assert!(equal("Café", "cafe"));
        assert!(equal("APPLE", "apple"));
        assert!(!equal("apple", "apples"));
    }

    #[test]
    fn numbers_sort_numerically() {
        assert_eq!(compare("note 2", "note 10"), Ordering::Less);
        assert_eq!(compare("t10", "t2"), Ordering::Greater);
        assert!(equal("v007", "v7"));
    }

    #[test]
    fn punctuation_before_digits_before_letters() {
        assert_eq!(compare("-a", "1a"), Ordering::Less);
        assert_eq!(compare("9", "a"), Ordering::Less);
        assert_eq!(compare("work", "work/clients"), Ordering::Less);
    }
}
