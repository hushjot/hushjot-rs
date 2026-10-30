//! Titles, excerpts and list order (the app's `src/core/notes/model.ts`).
//!
//! A note has no title field the user edits: the title is derived from the
//! first meaningful line, so the note is exactly its Markdown and there is no
//! second field to sync or conflict. These functions are frozen contract —
//! the app, the CLI and this crate must derive the same title from the same
//! text — so they are line-for-line ports, JavaScript string semantics
//! included (see [`crate::js`]).

use std::cmp::Ordering;
use std::sync::OnceLock;

use regex::Regex;

use crate::collate;
use crate::js::{self, JS_WS_CLASS};

/// The title of a note with no text.
pub const UNTITLED: &str = "Untitled";
/// Titles are cut to this many UTF-16 units.
pub const MAX_TITLE: usize = 120;
/// Excerpts are cut to this many UTF-16 units.
pub const MAX_EXCERPT: usize = 100;
/// Only this much of a note (UTF-16 units) is scanned for a title or excerpt.
pub const HEAD_SCAN: usize = 4096;

struct LinePatterns {
    heading: Regex,
    bullet: Regex,
    ordered: Regex,
    quote: Regex,
    structural_only: Regex,
}

fn patterns() -> &'static LinePatterns {
    static PATTERNS: OnceLock<LinePatterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let ws = JS_WS_CLASS;
        LinePatterns {
            heading: Regex::new(&format!(r"^#{{1,6}}[{ws}]*")).unwrap(),
            bullet: Regex::new(&format!(r"^[-*+][{ws}]+(\[[ xX]\][{ws}]*)?")).unwrap(),
            ordered: Regex::new(&format!(r"^[0-9]+[.)][{ws}]+")).unwrap(),
            quote: Regex::new(&format!(r"^>[{ws}]?")).unwrap(),
            structural_only: Regex::new(&format!(r"^[-*_=|#>~{ws}]+$")).unwrap(),
        }
    })
}

/// Strip the Markdown marks from one line, or return `""` if it carries no
/// text. Shared by [`derive_title`] and [`excerpt_of`] so the two agree about
/// where the title ends.
fn clean_line(raw: &str) -> String {
    let line = js::trim(raw);
    if line.is_empty() {
        return String::new();
    }
    let p = patterns();
    let step = p.heading.replace(line, "");
    let step = p.bullet.replace(&step, "");
    let step = p.ordered.replace(&step, "");
    let step = p.quote.replace(&step, "");
    let cleaned = js::trim(&step);
    if cleaned.is_empty() || p.structural_only.is_match(cleaned) {
        return String::new();
    }
    cleaned.to_string()
}

fn head(content: &str) -> &str {
    if js::utf16_len(content) > HEAD_SCAN {
        js::slice_utf16(content, HEAD_SCAN)
    } else {
        content
    }
}

/// The title: the first line with content, stripped of heading, list and
/// quote marks, cut to [`MAX_TITLE`]; [`UNTITLED`] when there is none.
pub fn derive_title(content: &str) -> String {
    for raw in head(content).split('\n') {
        let cleaned = clean_line(raw);
        if !cleaned.is_empty() {
            return js::slice_utf16(&cleaned, MAX_TITLE).to_string();
        }
    }
    UNTITLED.to_string()
}

/// The text after the title line, flattened to one line and cut to
/// [`MAX_EXCERPT`]. Fence markers are skipped.
pub fn excerpt_of(content: &str) -> String {
    let mut seen_title = false;
    let mut out = String::new();
    for raw in head(content).split('\n') {
        if js::trim_start(raw).starts_with("```") {
            continue;
        }
        let cleaned = clean_line(raw);
        if cleaned.is_empty() {
            continue;
        }
        if !seen_title {
            seen_title = true;
            continue;
        }
        if out.is_empty() {
            out = cleaned;
        } else {
            out.push(' ');
            out.push_str(&cleaned);
        }
        if js::utf16_len(&out) >= MAX_EXCERPT {
            break;
        }
    }
    js::trim(js::slice_utf16(&out, MAX_EXCERPT)).to_string()
}

/// Word count over Markdown source (spaces, tabs and line breaks separate
/// words; markup is counted as written).
pub fn count_words(content: &str) -> usize {
    let mut count = 0;
    let mut in_word = false;
    for c in content.chars() {
        if matches!(c, ' ' | '\t' | '\n' | '\r') {
            in_word = false;
        } else if !in_word {
            in_word = true;
            count += 1;
        }
    }
    count
}

/// The title a conflict copy is stored under:
/// `"<derived title> (conflict copy YYYY-MM-DD HH:MM)"`, UTC, as the app
/// writes it.
pub fn conflict_copy_title(content: &str, now_ms: i64) -> String {
    let stamp = chrono::DateTime::from_timestamp_millis(now_ms).unwrap_or_default().format("%Y-%m-%d %H:%M");
    format!("{} (conflict copy {stamp})", derive_title(content))
}

/// What a note list needs to order notes.
pub trait Listable {
    /// The note id.
    fn id(&self) -> &str;
    /// The stored title.
    fn title(&self) -> &str;
    /// When the note was pinned, if it is.
    fn pinned_at(&self) -> Option<i64>;
    /// When the note was last edited.
    fn updated_at(&self) -> i64;
    /// When the note was created.
    fn created_at(&self) -> i64;
}

/// Which key a list is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// Most recently edited first. The default.
    #[default]
    Edited,
    /// Most recently created first.
    Created,
    /// Alphabetical, naturally (see [`crate::collate`]).
    Title,
}

/// A list order. A per-device display preference, never synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoteSort {
    /// The key.
    pub key: SortKey,
    /// Reverse it.
    pub reversed: bool,
}

impl NoteSort {
    /// Whether this is the order the product shipped with.
    pub fn is_default(&self) -> bool {
        self.key == SortKey::Edited && !self.reversed
    }
}

fn by_id<T: Listable>(a: &T, b: &T) -> Ordering {
    a.id().cmp(b.id())
}

fn base_compare<T: Listable>(key: SortKey, a: &T, b: &T) -> Ordering {
    let recency = b.updated_at().cmp(&a.updated_at());
    match key {
        SortKey::Title => collate::compare(a.title(), b.title()).then(recency).then_with(|| by_id(a, b)),
        SortKey::Created => b.created_at().cmp(&a.created_at()).then(recency).then_with(|| by_id(a, b)),
        SortKey::Edited => recency.then_with(|| by_id(a, b)),
    }
}

/// The comparator for one sort. Every key falls through to recency, then id,
/// so the order is total and the same on every device.
pub fn compare_for<T: Listable>(sort: NoteSort, a: &T, b: &T) -> Ordering {
    let ordering = base_compare(sort.key, a, b);
    if sort.reversed {
        ordering.reverse()
    } else {
        ordering
    }
}

/// The order a note list shows: pinned notes first, then everything else.
///
/// Under the default sort the pins are ordered by when they were pinned (so the
/// top of the list holds still while a pinned note is edited) and the rest keep
/// the **caller's order**, which should already be most-recent-first. Every
/// other sort orders both groups itself.
pub fn order_for_list<T: Listable + Clone>(metas: &[T], sort: NoteSort) -> Vec<T> {
    if sort.is_default() {
        let mut pinned: Vec<T> = metas.iter().filter(|m| m.pinned_at().is_some()).cloned().collect();
        pinned.sort_by_key(|m| std::cmp::Reverse(m.pinned_at().unwrap_or(0)));
        pinned.extend(metas.iter().filter(|m| m.pinned_at().is_none()).cloned());
        return pinned;
    }
    let (mut pinned, mut rest): (Vec<T>, Vec<T>) = metas.iter().cloned().partition(|m| m.pinned_at().is_some());
    pinned.sort_by(|a, b| compare_for(sort, a, b));
    rest.sort_by(|a, b| compare_for(sort, a, b));
    pinned.extend(rest);
    pinned
}

/// Ids of notes whose title equals `wanted` at base strength — what a
/// `[[Note link]]` resolves to. Ambiguity is returned, in input order.
pub fn notes_matching<'a, T: Listable>(metas: &'a [T], wanted: &str) -> Vec<&'a str> {
    metas.iter().filter(|m| collate::equal(m.title(), wanted)).map(Listable::id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug)]
    struct M(&'static str, &'static str, Option<i64>, i64, i64);

    impl Listable for M {
        fn id(&self) -> &str {
            self.0
        }
        fn title(&self) -> &str {
            self.1
        }
        fn pinned_at(&self) -> Option<i64> {
            self.2
        }
        fn updated_at(&self) -> i64 {
            self.3
        }
        fn created_at(&self) -> i64 {
            self.4
        }
    }

    #[test]
    fn titles_strip_markdown_marks() {
        assert_eq!(derive_title("# Heading\nbody"), "Heading");
        assert_eq!(derive_title("- [x] done thing"), "done thing");
        assert_eq!(derive_title("12) twelfth"), "twelfth");
        assert_eq!(derive_title("> quote"), "quote");
        assert_eq!(derive_title("###"), "Untitled");
        assert_eq!(derive_title("| --- | --- |\nreal"), "real");
        assert_eq!(derive_title("\u{FEFF}  padded  "), "padded");
        assert_eq!(derive_title("🔐"), "🔐");
    }

    #[test]
    fn titles_are_cut_in_utf16_units() {
        let long = "🔐".repeat(100);
        assert_eq!(js::utf16_len(&derive_title(&long)), 120);
        let a = "a".repeat(200);
        assert_eq!(derive_title(&a).len(), 120);
    }

    #[test]
    fn excerpts_skip_the_title_and_fences() {
        assert_eq!(excerpt_of("# T\n\n```sql\nselect 1\n```\nafter"), "select 1 after");
        assert_eq!(excerpt_of("only"), "");
        // Cut at 100, then trimmed: the cut lands on a space.
        assert_eq!(js::utf16_len(&excerpt_of(&format!("t\n{}", "word ".repeat(50)))), 99);
    }

    #[test]
    fn excerpt_cut_then_trimmed() {
        let content = format!("t\n{} x", "a".repeat(99));
        assert_eq!(excerpt_of(&content), "a".repeat(99));
    }

    #[test]
    fn word_counts_follow_whitespace() {
        assert_eq!(count_words("one two\nthree\t four"), 4);
        assert_eq!(count_words(""), 0);
    }

    #[test]
    fn conflict_copies_are_stamped_in_utc() {
        assert_eq!(conflict_copy_title("# Plan\nx", 1_758_500_000_000), "Plan (conflict copy 2025-09-22 00:13)");
    }

    #[test]
    fn default_order_puts_pins_first_by_pin_time_and_keeps_the_rest() {
        let list = vec![
            M("a", "A", None, 3, 0),
            M("b", "B", Some(1), 2, 0),
            M("c", "C", Some(5), 1, 0),
            M("d", "D", None, 9, 0),
        ];
        let ids: Vec<_> = order_for_list(&list, NoteSort::default()).iter().map(|m| m.0).collect();
        assert_eq!(ids, ["c", "b", "a", "d"]);
    }

    #[test]
    fn other_sorts_order_both_groups() {
        let list = vec![M("a", "note 10", None, 1, 5), M("b", "note 2", None, 2, 6), M("c", "zed", Some(1), 0, 1)];
        let by_title: Vec<_> =
            order_for_list(&list, NoteSort { key: SortKey::Title, reversed: false }).iter().map(|m| m.0).collect();
        assert_eq!(by_title, ["c", "b", "a"]);
        let created_rev: Vec<_> =
            order_for_list(&list, NoteSort { key: SortKey::Created, reversed: true }).iter().map(|m| m.0).collect();
        assert_eq!(created_rev, ["c", "a", "b"]);
    }

    #[test]
    fn links_resolve_at_base_strength() {
        let list = vec![M("a", "Café", None, 1, 1), M("b", "other", None, 1, 1), M("c", "CAFE", None, 1, 1)];
        assert_eq!(notes_matching(&list, "cafe"), ["a", "c"]);
    }
}
