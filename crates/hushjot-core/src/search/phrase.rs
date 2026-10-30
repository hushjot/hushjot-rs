//! Confirming a `"quoted phrase"` (the app's `src/core/search/phrase.ts`).
//!
//! The index narrows to notes holding all of a phrase's words in any order;
//! this re-reads those few bodies to check the words are adjacent. Positions
//! are deliberately not stored in the index — they would make it close enough
//! to the text to rebuild it.

use super::index::SearchHit;
use super::query::{normalise_phrase, ParsedQuery};

/// The most candidate bodies one phrase query may open. The loop also stops at
/// `limit` verified hits, so the cap only bites when many notes hold all the
/// words and few hold them adjacently.
pub const VERIFY_MAX: usize = 200;

/// Keep the candidates whose text contains every phrase and no excluded
/// phrase, preserving the index's order. `load_body` returns `None` for a body
/// that cannot be read, which is then not a hit.
pub fn verify_phrases(
    candidates: &[SearchHit],
    parsed: &ParsedQuery,
    limit: usize,
    mut load_body: impl FnMut(&str) -> Option<String>,
) -> Vec<SearchHit> {
    let mut out = Vec::new();
    for hit in candidates.iter().take(VERIFY_MAX) {
        if out.len() >= limit {
            break;
        }
        let Some(body) = load_body(&hit.id) else { continue };
        let hay = normalise_phrase(&body);
        if parsed.phrases.iter().all(|p| hay.contains(p.as_str()))
            && !parsed.not_phrases.iter().any(|p| hay.contains(p.as_str()))
        {
            out.push(hit.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::parse_query;

    fn hit(id: &str) -> SearchHit {
        SearchHit { id: id.into(), score: 1.0, in_title: false }
    }

    #[test]
    fn keeps_adjacent_phrases_and_drops_excluded_ones() {
        let parsed = parse_query(r#""connection pool" -"pool size""#);
        let bodies = |id: &str| match id {
            "a" => Some("The Connection\npool is full".to_string()),
            "b" => Some("pool connection".to_string()),
            "c" => Some("connection pool size".to_string()),
            _ => None,
        };
        let out = verify_phrases(&[hit("a"), hit("b"), hit("c"), hit("gone")], &parsed, 10, bodies);
        assert_eq!(out.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn stops_at_the_limit() {
        let parsed = parse_query(r#""x""#);
        let out = verify_phrases(&[hit("a"), hit("b")], &parsed, 1, |_| Some("x".into()));
        assert_eq!(out.len(), 1);
    }
}
