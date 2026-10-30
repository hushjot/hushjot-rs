//! The search query grammar (the app's `src/core/search/query.ts`).
//!
//! Shared with the app and `@hushjot/cli`, so a query pasted from one means the
//! same thing in the other:
//!
//! - words are ANDed prefix matches;
//! - `#tag` filters by tag prefix (and its nested tags);
//! - `in:Work/DB` scopes to a folder and its subtree (`in:/` is unfiled);
//! - `"quoted"` is a literal phrase, verified against note bodies;
//! - `-` negates any of them.
//!
//! **Quoted runs are lifted first**, which is what makes quotes the escape for
//! a literal `#` or `-`. `-` and `in:` immediately before a quote still bind to
//! it, so `in:"My Work"` is a folder. **An unterminated quote is plain words**,
//! because a query is re-parsed on every keystroke. `in:` is not resolved here:
//! folder names are sealed, so only the caller can turn one into ids.

use std::sync::OnceLock;

use regex::Regex;

use crate::js::{self, is_js_whitespace};

const IN_PREFIX: &str = "in:";

/// A parsed query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    /// Prefix-matched words that must all be present.
    pub terms: Vec<String>,
    /// Words whose prefix-matches exclude a note.
    pub not_terms: Vec<String>,
    /// Lowercased, whitespace-collapsed literal runs.
    pub phrases: Vec<String>,
    /// Phrases that exclude a note.
    pub not_phrases: Vec<String>,
    /// Tag prefixes, lowercased, without `#`.
    pub tags: Vec<String>,
    /// Excluded tag prefixes.
    pub not_tags: Vec<String>,
    /// Raw `in:` values, lowercased.
    pub folders: Vec<String>,
    /// Excluded `in:` values.
    pub not_folders: Vec<String>,
}

pub(crate) fn split_regex() -> &'static Regex {
    static SPLIT: OnceLock<Regex> = OnceLock::new();
    SPLIT.get_or_init(|| Regex::new(r"[^\p{L}\p{N}_]+").unwrap())
}

/// Lowercased, with whitespace runs collapsed to one space and trimmed, so a
/// phrase can span a line break.
pub fn normalise_phrase(text: &str) -> String {
    let lower = js::to_lower(text);
    let mut out = String::with_capacity(lower.len());
    let mut in_space = false;
    for c in lower.chars() {
        if is_js_whitespace(c) {
            in_space = true;
        } else {
            if in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = false;
            out.push(c);
        }
    }
    out
}

/// The whitespace-delimited token ending at byte `end`, lowercased, and its start.
fn token_before(query: &str, end: usize) -> (String, usize) {
    let start = query[..end]
        .char_indices()
        .rev()
        .find(|(_, c)| is_js_whitespace(*c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    (js::to_lower(&query[start..end]), start)
}

struct Lifted {
    rest: String,
    phrases: Vec<String>,
    not_phrases: Vec<String>,
    folders: Vec<String>,
    not_folders: Vec<String>,
}

fn lift_phrases(query: &str) -> Lifted {
    let mut out =
        Lifted { rest: String::new(), phrases: vec![], not_phrases: vec![], folders: vec![], not_folders: vec![] };
    let mut i = 0;
    while i < query.len() {
        let Some(open) = query[i..].find('"').map(|o| o + i) else { break };
        let Some(close) = query[open + 1..].find('"').map(|c| c + open + 1) else { break };

        let (before, start) = token_before(query, open);
        let negated = before == "-" || before == "-in:";
        let is_folder = before == IN_PREFIX || before == "-in:";
        let cut = if negated || is_folder { start } else { open };

        out.rest.push_str(&query[i..cut]);
        out.rest.push(' ');

        let body = normalise_phrase(&query[open + 1..close]);
        if !body.is_empty() {
            match (is_folder, negated) {
                (true, true) => out.not_folders.push(body),
                (true, false) => out.folders.push(body),
                (false, true) => out.not_phrases.push(body),
                (false, false) => out.phrases.push(body),
            }
        }
        i = close + 1;
    }
    out.rest.push_str(&query[i.min(query.len())..]);
    out
}

/// Read a query into its operators. Everything is lowercased.
pub fn parse_query(query: &str) -> ParsedQuery {
    let lifted = lift_phrases(query);
    let mut out = ParsedQuery {
        phrases: lifted.phrases,
        not_phrases: lifted.not_phrases,
        folders: lifted.folders,
        not_folders: lifted.not_folders,
        ..ParsedQuery::default()
    };

    let rest = js::to_lower(&lifted.rest);
    for raw in rest.split(is_js_whitespace) {
        if raw.is_empty() {
            continue;
        }
        let negated = raw.chars().count() > 1 && raw.starts_with('-');
        let word = if negated { &raw[1..] } else { raw };

        if word.chars().count() > 1 && word.starts_with('#') {
            let tag = word[1..].to_string();
            if negated {
                out.not_tags.push(tag)
            } else {
                out.tags.push(tag)
            }
            continue;
        }

        if let Some(path) = word.strip_prefix(IN_PREFIX) {
            if !path.is_empty() {
                if negated {
                    out.not_folders.push(path.to_string())
                } else {
                    out.folders.push(path.to_string())
                }
            }
            continue;
        }

        for term in split_regex().split(word) {
            if !term.is_empty() {
                if negated {
                    out.not_terms.push(term.to_string())
                } else {
                    out.terms.push(term.to_string())
                }
            }
        }
    }
    out
}

/// True when the query names something to match, not only things to reject.
/// `-draft` alone has no answer but "every note you own", so it returns nothing.
pub fn has_positive_term(parsed: &ParsedQuery) -> bool {
    !parsed.terms.is_empty() || !parsed.tags.is_empty() || !parsed.folders.is_empty() || !parsed.phrases.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn words_tags_folders_and_negation() {
        let q = parse_query("Postgres #Work in:Work/DB -draft -#old -in:Archive kebab-case");
        assert_eq!(q.terms, v(&["postgres", "kebab", "case"]));
        assert_eq!(q.tags, v(&["work"]));
        assert_eq!(q.folders, v(&["work/db"]));
        assert_eq!(q.not_terms, v(&["draft"]));
        assert_eq!(q.not_tags, v(&["old"]));
        assert_eq!(q.not_folders, v(&["archive"]));
    }

    #[test]
    fn quotes_escape_operators_and_bind_to_what_precedes_them() {
        let q = parse_query(r##""#include" -"connection  pool" in:"My Work" -in:"Old Stuff" foo-"bar""##);
        assert_eq!(q.phrases, v(&["#include", "bar"]));
        assert_eq!(q.not_phrases, v(&["connection pool"]));
        assert_eq!(q.folders, v(&["my work"]));
        assert_eq!(q.not_folders, v(&["old stuff"]));
        assert_eq!(q.terms, v(&["foo"]));
    }

    #[test]
    fn an_unterminated_quote_is_plain_words() {
        let q = parse_query(r#""conn pool"#);
        assert!(q.phrases.is_empty());
        assert_eq!(q.terms, v(&["conn", "pool"]));
    }

    #[test]
    fn bare_operators_constrain_nothing() {
        let q = parse_query(r#"in: # - """#);
        assert!(q.folders.is_empty());
        assert!(q.tags.is_empty());
        assert!(q.phrases.is_empty());
        assert_eq!(q.terms.len(), 0);
        assert!(!has_positive_term(&q));
        assert!(!has_positive_term(&parse_query("-draft")));
        assert!(has_positive_term(&parse_query("in:Work -draft")));
    }

    #[test]
    fn phrases_collapse_whitespace() {
        assert_eq!(normalise_phrase("  A\n\tB  c "), "a b c");
    }
}
