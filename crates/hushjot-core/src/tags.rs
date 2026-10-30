//! Inline `#tags`, read from a note's text (the app's `src/core/notes/tags.ts`).
//!
//! A tag is text in the note, never a stored field: it survives export, copy
//! and import, and it is on every device the moment the text is. The rules are
//! Bear's and Obsidian's — `#` then a letter, digit or underscore, at the start
//! of a line or after whitespace — which keeps `[link](#anchor)`, `C#` and
//! `# Heading` out. Also refused: tags with no letter (`#2024`), CSS colours
//! with a digit (`#ff0000`), and anything inside code (fenced blocks, indented
//! blocks, inline backtick spans). `a/b` nests.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use crate::js::{self, JS_WS_CLASS};

/// A tag as found in a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    /// Identity: lowercased, so `#Work` and `#work` are one tag.
    pub key: String,
    /// How the note first wrote it.
    pub label: String,
}

/// Longest tag, in UTF-16 units.
pub const MAX_TAG_LENGTH: usize = 64;

struct Patterns {
    tag: Regex,
    has_letter: Regex,
    hex_colour: Regex,
    fence: Regex,
    indented: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        tag: Regex::new(&format!(r"(?:^|[{JS_WS_CLASS}])#([\p{{L}}\p{{N}}_][\p{{L}}\p{{N}}_/-]*)")).unwrap(),
        has_letter: Regex::new(r"\p{L}").unwrap(),
        hex_colour: Regex::new(r"(?i)^(?:[0-9a-f]{3}|[0-9a-f]{4}|[0-9a-f]{6}|[0-9a-f]{8})$").unwrap(),
        fence: Regex::new(r"^ {0,3}(`{3,}|~{3,})").unwrap(),
        indented: Regex::new(r"^(?: {4}|\t)").unwrap(),
    })
}

/// The tag a matched body names, or `None`. Trailing `-` on each segment is
/// dropped and empty segments collapse (`#a//b` is `a/b`).
pub fn normalise_tag(body: &str) -> Option<String> {
    let label = body
        .split('/')
        .map(|segment| segment.trim_end_matches('-'))
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    let p = patterns();
    if label.is_empty() || js::utf16_len(&label) > MAX_TAG_LENGTH {
        return None;
    }
    if !p.has_letter.is_match(&label) {
        return None;
    }
    if p.hex_colour.is_match(&label) && label.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(label)
}

/// Blank out inline code spans, as the app's
/// `` /(`+)[^`]*?(?:`(?!\1)[^`]*?)*\1/g `` does. The pattern uses a
/// backreference, which the `regex` crate does not support, so this walks the
/// same search the JavaScript engine would.
fn blank_inline_code(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let is_tick = |i: usize| chars.get(i) == Some(&'`');
    let run_at = |i: usize, n: usize| (0..n).all(|k| is_tick(i + k));

    // Where a span opened at `start` with `n` backticks would end, if anywhere.
    let close = |start: usize, n: usize| -> Option<usize> {
        let mut i = start + n;
        loop {
            if run_at(i, n) {
                return Some(i + n);
            }
            if i >= chars.len() {
                return None;
            }
            if is_tick(i) && run_at(i + 1, n) {
                return None;
            }
            i += 1;
        }
    };

    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if !is_tick(i) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut run = 0;
        while is_tick(i + run) {
            run += 1;
        }
        // Greedy `+`, backtracking to shorter openers.
        let found = (1..=run).rev().find_map(|n| close(i, n));
        match found {
            Some(end) => {
                out.extend(std::iter::repeat_n(' ', end - i));
                i = end;
            }
            None => {
                out.push('`');
                i += 1;
            }
        }
    }
    out.into_iter().collect()
}

/// Tags in `content`, each once, in the order first written.
pub fn extract_tags(content: &str) -> Vec<Tag> {
    if !content.contains('#') {
        return Vec::new();
    }
    let p = patterns();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut fence: Option<String> = None;
    let mut previous_blank = true;
    let mut in_indented_code = false;

    for line in content.split('\n') {
        let fence_match = p.fence.captures(line).map(|c| c[1].to_string());
        if let Some(open) = &fence {
            if let Some(close) = &fence_match {
                if close.chars().next() == open.chars().next() && close.len() >= open.len() {
                    fence = None;
                }
            }
            previous_blank = false;
            continue;
        }
        if fence_match.is_some() {
            fence = fence_match;
            previous_blank = false;
            continue;
        }

        let blank = js::trim(line).is_empty();
        let indented = p.indented.is_match(line);
        in_indented_code = !blank && indented && (previous_blank || in_indented_code);
        previous_blank = blank;
        if blank || in_indented_code || !line.contains('#') {
            continue;
        }

        let text = if line.contains('`') { blank_inline_code(line) } else { line.to_string() };
        for captures in p.tag.captures_iter(&text) {
            let Some(label) = normalise_tag(&captures[1]) else { continue };
            let key = js::to_lower(&label);
            if seen.insert(key.clone()) {
                out.push(Tag { key, label });
            }
        }
    }
    out
}

/// True when a note tagged `key` belongs under the filter `filter`: equal, or
/// nested beneath it.
pub fn tag_matches(key: &str, filter: &str) -> bool {
    key == filter || (key.len() > filter.len() && key.starts_with(filter) && key.as_bytes()[filter.len()] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(content: &str) -> Vec<String> {
        extract_tags(content).into_iter().map(|t| t.key).collect()
    }

    #[test]
    fn finds_tags_at_line_start_and_after_whitespace() {
        assert_eq!(keys("#work and #Home\nmore #work"), ["work", "home"]);
        assert_eq!(extract_tags("#Work")[0].label, "Work");
    }

    #[test]
    fn ignores_headings_anchors_and_language_names() {
        assert!(keys("# Heading\n[x](#anchor) C# a#b").is_empty());
    }

    #[test]
    fn refuses_numbers_and_colours() {
        assert!(keys("#2024 #ff0000 #1e90ff").is_empty());
        assert_eq!(keys("#fade #abc1x"), ["fade", "abc1x"]);
    }

    #[test]
    fn ignores_code() {
        assert!(keys("```\n#inside\n```").is_empty());
        assert!(keys("~~~~\n#inside\n~~~\n#still\n~~~~").is_empty());
        assert!(keys("para\n\n    #indented").is_empty());
        assert_eq!(keys("para\n    #continuation"), ["continuation"]);
        assert!(keys("use `#include` here").is_empty());
        assert_eq!(keys("`code` #real"), ["real"]);
        assert_eq!(keys("``a`b`` #x"), ["x"]);
    }

    #[test]
    fn normalises_nesting_and_trailing_punctuation() {
        assert_eq!(keys("#a//b #done/ #next-"), ["a/b", "done", "next"]);
        assert_eq!(normalise_tag(&"a".repeat(65)), None);
    }

    #[test]
    fn nested_filters_match_children_only_at_a_separator() {
        assert!(tag_matches("work", "work"));
        assert!(tag_matches("work/clients", "work"));
        assert!(!tag_matches("workshop", "work"));
    }

    #[test]
    fn inline_code_blanking_follows_the_regex_engine() {
        assert_eq!(blank_inline_code("a `b` c"), "a     c");
        assert_eq!(blank_inline_code("``a`"), "  a`");
        assert_eq!(blank_inline_code("no ticks"), "no ticks");
        assert_eq!(blank_inline_code("`unclosed"), "`unclosed");
    }
}
