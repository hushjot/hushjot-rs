//! The local search index (the app's `src/core/search/index.ts`).
//!
//! Search has to be local because the server holds only ciphertext. This is an
//! inverted token index with two-character prefix buckets, so a prefix query
//! touches one bucket instead of every token. It holds **tokens, never text**:
//! nothing here can rebuild a note, and it is never persisted — a serialised
//! index would be every note's words in the clear.
//!
//! Collections preserve insertion order (`IndexMap`/`IndexSet`), as the app's
//! `Map` and `Set` do, so that results tied on score and recency come back in
//! the same order the app shows them.

use std::collections::HashSet;

use indexmap::{IndexMap, IndexSet};

use super::query::{has_positive_term, parse_query, split_regex, ParsedQuery};
use crate::collate;
use crate::js;
use crate::tags::{extract_tags, tag_matches};

const MIN_TOKEN: usize = 2;
const MAX_TOKEN: usize = 40;
const BUCKET_LEN: usize = 2;

/// Lowercase and split on anything that is not a letter, digit or underscore;
/// drop tokens shorter than 2 and cut those longer than 40 (UTF-16 units).
pub fn tokenize(text: &str) -> Vec<String> {
    let lower = js::to_lower(text);
    split_regex()
        .split(&lower)
        .filter(|raw| js::utf16_len(raw) >= MIN_TOKEN)
        .map(
            |raw| {
                if js::utf16_len(raw) > MAX_TOKEN {
                    js::slice_utf16(raw, MAX_TOKEN).to_string()
                } else {
                    raw.to_string()
                }
            },
        )
        .collect()
}

/// The first `BUCKET_LEN` UTF-16 units of a token.
fn bucket_key(token: &str) -> String {
    js::slice_utf16(token, BUCKET_LEN).to_string()
}

/// One search result.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// The note id.
    pub id: String,
    /// Higher is better. Title matches score ten times body matches.
    pub score: f64,
    /// Whether any term matched the title.
    pub in_title: bool,
}

/// A note as the index takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDocument {
    /// The note id.
    pub id: String,
    /// Its stored title.
    pub title: String,
    /// Its text.
    pub content: String,
    /// When it was last edited; breaks score ties.
    pub updated_at: i64,
    /// Its folder, for `in:` scoping.
    pub folder_id: Option<String>,
}

/// Folder scoping, resolved to ids by the caller. `None` inside a set means
/// unfiled, which is what `in:/` resolves to.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// Results to return; 50 when `None`.
    pub limit: Option<usize>,
    /// Keep only notes in these folders.
    pub folder_ids: Option<HashSet<Option<String>>>,
    /// Drop notes in these folders.
    pub not_folder_ids: Option<HashSet<Option<String>>>,
}

/// A tag and how many notes carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagCount {
    /// Lowercased identity.
    pub key: String,
    /// Display spelling.
    pub label: String,
    /// Notes carrying it.
    pub count: usize,
}

#[derive(Debug, Clone)]
struct DocEntry {
    updated_at: i64,
    folder_id: Option<String>,
    tokens: IndexSet<String>,
    title_tokens: IndexSet<String>,
    tags: Vec<String>,
}

/// The in-memory index. Build it after unlock; drop it on lock.
#[derive(Debug, Default, Clone)]
pub struct SearchIndex {
    postings: IndexMap<String, IndexSet<String>>,
    title_postings: IndexMap<String, IndexSet<String>>,
    buckets: IndexMap<String, IndexSet<String>>,
    docs: IndexMap<String, DocEntry>,
    tag_postings: IndexMap<String, IndexSet<String>>,
    tag_labels: IndexMap<String, String>,
    tag_version: u64,
}

impl SearchIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Notes indexed.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Whether nothing is indexed.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Distinct tokens indexed.
    pub fn token_count(&self) -> usize {
        self.postings.len()
    }

    /// Bumped whenever the set of tags or any tag's count changes.
    pub fn tag_version(&self) -> u64 {
        self.tag_version
    }

    fn add_token(&mut self, token: &str, id: &str, title: bool) {
        let map = if title { &mut self.title_postings } else { &mut self.postings };
        map.entry(token.to_string()).or_default().insert(id.to_string());
        if !title {
            self.buckets.entry(bucket_key(token)).or_default().insert(token.to_string());
        }
    }

    fn remove_token(&mut self, token: &str, id: &str, title: bool) {
        let map = if title { &mut self.title_postings } else { &mut self.postings };
        let Some(posting) = map.get_mut(token) else { return };
        posting.shift_remove(id);
        if !posting.is_empty() {
            return;
        }
        map.shift_remove(token);
        if !title {
            let key = bucket_key(token);
            if let Some(bucket) = self.buckets.get_mut(&key) {
                bucket.shift_remove(token);
                if bucket.is_empty() {
                    self.buckets.shift_remove(&key);
                }
            }
        }
    }

    /// Add or replace a note. Replacing is remove-then-add, so an edited note
    /// keeps no postings for words that were deleted.
    pub fn add(&mut self, doc: &IndexDocument) {
        let tagged = extract_tags(&doc.content);
        let tags: Vec<String> = tagged.iter().map(|t| t.key.clone()).collect();
        let before = self.docs.get(&doc.id).map(|d| d.tags.clone());
        let same_tags = before.as_ref() == Some(&tags);

        self.remove_inner(&doc.id, same_tags);

        let title_tokens: IndexSet<String> = tokenize(&doc.title).into_iter().collect();
        let mut tokens: IndexSet<String> = tokenize(&doc.content).into_iter().collect();
        for t in &title_tokens {
            tokens.insert(t.clone());
        }
        for token in &tokens {
            self.add_token(token, &doc.id, false);
        }
        for token in &title_tokens {
            self.add_token(token, &doc.id, true);
        }
        for tag in &tagged {
            if !self.tag_postings.contains_key(&tag.key) {
                self.tag_labels.insert(tag.key.clone(), tag.label.clone());
            }
            self.tag_postings.entry(tag.key.clone()).or_default().insert(doc.id.clone());
        }
        if !same_tags && (!tags.is_empty() || before.as_ref().is_some_and(|b| !b.is_empty())) {
            self.tag_version += 1;
        }
        self.docs.insert(
            doc.id.clone(),
            DocEntry { updated_at: doc.updated_at, folder_id: doc.folder_id.clone(), tokens, title_tokens, tags },
        );
    }

    /// Remove a note.
    pub fn remove(&mut self, id: &str) {
        self.remove_inner(id, false);
    }

    fn remove_inner(&mut self, id: &str, quiet: bool) {
        let Some(entry) = self.docs.shift_remove(id) else { return };
        for token in &entry.tokens {
            self.remove_token(token, id, false);
        }
        for token in &entry.title_tokens {
            self.remove_token(token, id, true);
        }
        for key in &entry.tags {
            if let Some(posting) = self.tag_postings.get_mut(key) {
                posting.shift_remove(id);
                if posting.is_empty() {
                    self.tag_postings.shift_remove(key);
                    self.tag_labels.shift_remove(key);
                }
            }
        }
        if !entry.tags.is_empty() && !quiet {
            self.tag_version += 1;
        }
    }

    /// Re-scope a note without touching its tokens — for a move, which has no
    /// body in hand. Routing a move through [`SearchIndex::add`] with empty
    /// content would delete every word the note owns from the index.
    pub fn set_folder(&mut self, id: &str, folder_id: Option<String>) {
        if let Some(entry) = self.docs.get_mut(id) {
            entry.folder_id = folder_id;
        }
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        *self = Self { tag_version: self.tag_version + 1, ..Self::default() };
    }

    /// Every tag with its count, in natural order.
    pub fn tag_summary(&self) -> Vec<TagCount> {
        let mut out: Vec<TagCount> = self
            .tag_postings
            .iter()
            .map(|(key, ids)| TagCount {
                key: key.clone(),
                label: self.tag_labels.get(key).cloned().unwrap_or_else(|| key.clone()),
                count: ids.len(),
            })
            .collect();
        out.sort_by(|a, b| collate::compare(&a.key, &b.key));
        out
    }

    /// Notes tagged `filter` or anything nested under it, newest first.
    pub fn ids_for_tag(&self, filter: &str) -> Vec<String> {
        let ids = self.ids_matching_tag(&js::to_lower(filter), false);
        self.by_recency(ids.into_iter())
    }

    fn ids_matching_tag(&self, filter: &str, prefix: bool) -> IndexSet<String> {
        let mut out = IndexSet::new();
        for (key, ids) in &self.tag_postings {
            let hit = if prefix { key.starts_with(filter) } else { tag_matches(key, filter) };
            if hit {
                out.extend(ids.iter().cloned());
            }
        }
        out
    }

    fn updated(&self, id: &str) -> i64 {
        self.docs.get(id).map_or(0, |d| d.updated_at)
    }

    fn by_recency(&self, ids: impl Iterator<Item = String>) -> Vec<String> {
        let mut out: Vec<String> = ids.collect();
        out.sort_by_key(|id| std::cmp::Reverse(self.updated(id)));
        out
    }

    fn tokens_with_prefix(&self, prefix: &str) -> Vec<&str> {
        if js::utf16_len(prefix) >= BUCKET_LEN {
            let Some(bucket) = self.buckets.get(&bucket_key(prefix)) else { return Vec::new() };
            return bucket.iter().filter(|t| t.starts_with(prefix)).map(String::as_str).collect();
        }
        self.postings.keys().filter(|t| t.starts_with(prefix)).map(String::as_str).collect()
    }

    fn ids_in_folders(&self, folders: &HashSet<Option<String>>) -> IndexSet<String> {
        self.docs.iter().filter(|(_, e)| folders.contains(&e.folder_id)).map(|(id, _)| id.clone()).collect()
    }

    /// Search with an unparsed query and no folder scope (`in:` is ignored —
    /// use [`SearchIndex::search_parsed`] after resolving folders).
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        self.search_parsed(&parse_query(query), &SearchOptions { limit: Some(limit), ..SearchOptions::default() })
    }

    /// The real search. When `parsed.phrases` is non-empty the hits are only
    /// *candidates*: adjacency needs the text, which the index never holds, so
    /// verify them with [`super::verify_phrases`].
    pub fn search_parsed(&self, parsed: &ParsedQuery, options: &SearchOptions) -> Vec<SearchHit> {
        let limit = options.limit.unwrap_or(50);
        if !has_positive_term(parsed) {
            return Vec::new();
        }

        let mut allowed: Option<IndexSet<String>> = None;
        let narrow = |ids: IndexSet<String>, allowed: &mut Option<IndexSet<String>>| -> bool {
            match allowed {
                Some(current) => current.retain(|id| ids.contains(id)),
                None => *allowed = Some(ids),
            }
            !allowed.as_ref().is_some_and(IndexSet::is_empty)
        };
        for filter in &parsed.tags {
            if !narrow(self.ids_matching_tag(filter, true), &mut allowed) {
                return Vec::new();
            }
        }
        if let Some(folders) = &options.folder_ids {
            if !narrow(self.ids_in_folders(folders), &mut allowed) {
                return Vec::new();
            }
        }

        let mut denied: HashSet<String> = HashSet::new();
        for filter in &parsed.not_tags {
            denied.extend(self.ids_matching_tag(filter, true));
        }
        if let Some(folders) = &options.not_folder_ids {
            denied.extend(self.ids_in_folders(folders));
        }
        for term in &parsed.not_terms {
            for token in self.tokens_with_prefix(term) {
                if let Some(ids) = self.postings.get(token) {
                    denied.extend(ids.iter().cloned());
                }
            }
        }
        let keep = |id: &str| !denied.contains(id) && allowed.as_ref().is_none_or(|a| a.contains(id));

        let mut terms: Vec<String> = parsed.terms.clone();
        for phrase in &parsed.phrases {
            terms.extend(split_regex().split(phrase).filter(|w| !w.is_empty()).map(str::to_string));
        }

        if terms.is_empty() {
            let ids = self.by_recency(allowed.iter().flatten().filter(|id| keep(id)).cloned());
            return ids.into_iter().take(limit).map(|id| SearchHit { id, score: 1.0, in_title: false }).collect();
        }

        let mut candidates: Option<IndexMap<String, (f64, bool)>> = None;
        for term in &terms {
            let mut matched: IndexMap<String, (f64, bool)> = IndexMap::new();
            let term_len = js::utf16_len(term) as f64;
            for token in self.tokens_with_prefix(term) {
                let closeness = term_len / js::utf16_len(token) as f64;
                let in_title_ids = self.title_postings.get(token);
                for id in self.postings.get(token).into_iter().flatten() {
                    if !keep(id) {
                        continue;
                    }
                    let in_title = in_title_ids.is_some_and(|t| t.contains(id));
                    let score = closeness * if in_title { 10.0 } else { 1.0 };
                    match matched.get_mut(id) {
                        None => {
                            matched.insert(id.clone(), (score, in_title));
                        }
                        Some(existing) if score > existing.0 => {
                            *existing = (score, in_title || existing.1);
                        }
                        Some(existing) => {
                            if in_title {
                                existing.1 = true;
                            }
                        }
                    }
                }
            }
            candidates = Some(match candidates {
                None => matched,
                Some(mut acc) => {
                    acc.retain(|id, slot| match matched.get(id) {
                        None => false,
                        Some(hit) => {
                            slot.0 += hit.0;
                            slot.1 |= hit.1;
                            true
                        }
                    });
                    acc
                }
            });
            if candidates.as_ref().is_some_and(IndexMap::is_empty) {
                return Vec::new();
            }
        }

        let mut hits: Vec<SearchHit> = candidates
            .unwrap_or_default()
            .into_iter()
            .map(|(id, (score, in_title))| SearchHit { id, score, in_title })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.updated(&b.id).cmp(&self.updated(&a.id)))
        });
        hits.truncate(limit);
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, title: &str, content: &str, updated: i64, folder: Option<&str>) -> IndexDocument {
        IndexDocument {
            id: id.into(),
            title: title.into(),
            content: content.into(),
            updated_at: updated,
            folder_id: folder.map(Into::into),
        }
    }

    fn ids(hits: &[SearchHit]) -> Vec<&str> {
        hits.iter().map(|h| h.id.as_str()).collect()
    }

    fn sample() -> SearchIndex {
        let mut index = SearchIndex::new();
        index.add(&doc("pg", "Postgres tuning", "# Postgres tuning\nconnection pool size #db", 3, Some("work")));
        index.add(&doc("my", "MySQL", "# MySQL\npostgres comparison #db/legacy", 2, None));
        index.add(&doc("draft", "Draft", "# Draft\npool notes #draft", 1, Some("work")));
        index
    }

    #[test]
    fn tokenizer_matches_the_contract_shape() {
        assert_eq!(
            tokenize("CamelCase and snake_case and kebab-case"),
            ["camelcase", "and", "snake_case", "and", "kebab", "case"]
        );
        assert_eq!(tokenize("a bb ccc"), ["bb", "ccc"]);
        assert_eq!(tokenize(&"x".repeat(50))[0].len(), 40);
    }

    #[test]
    fn title_matches_outrank_body_matches() {
        let hits = sample().search("postgres", 10);
        assert_eq!(ids(&hits), ["pg", "my"]);
        assert!(hits[0].in_title && !hits[1].in_title);
        assert!((hits[0].score - 10.0).abs() < 1e-9);
    }

    #[test]
    fn terms_and_prefixes_narrow() {
        let index = sample();
        // "postgres" in pg's title scores 2.5; "pool" (0.5) beats "postgres" in a body (0.25).
        assert_eq!(ids(&index.search("po", 10)), ["pg", "draft", "my"]);
        assert_eq!(ids(&index.search("pool conn", 10)), ["pg"]);
        assert!(index.search("pool zebra", 10).is_empty());
    }

    #[test]
    fn tags_filter_by_prefix_and_nesting() {
        let index = sample();
        assert_eq!(ids(&index.search("#db", 10)), ["pg", "my"]);
        assert_eq!(ids(&index.search("#db/le", 10)), ["my"]);
        assert_eq!(index.ids_for_tag("db"), ["pg", "my"]);
        assert_eq!(ids(&index.search("pool -#draft", 10)), ["pg"]);
        let summary = index.tag_summary();
        assert_eq!(
            summary.iter().map(|t| (t.key.as_str(), t.count)).collect::<Vec<_>>(),
            [("db", 1), ("db/legacy", 1), ("draft", 1)]
        );
    }

    #[test]
    fn negations_and_negation_only_queries() {
        let index = sample();
        assert_eq!(ids(&index.search("pool -draft", 10)), ["pg"]);
        assert!(index.search("-pool", 10).is_empty());
    }

    #[test]
    fn folder_scopes_resolve_by_id_with_none_meaning_unfiled() {
        let index = sample();
        let parsed = parse_query("pool");
        let options =
            SearchOptions { folder_ids: Some(HashSet::from([Some("work".to_string())])), ..Default::default() };
        assert_eq!(ids(&index.search_parsed(&parsed, &options)), ["pg", "draft"]);
        let unfiled = SearchOptions { folder_ids: Some(HashSet::from([None])), ..Default::default() };
        assert_eq!(ids(&index.search_parsed(&parse_query("postgres"), &unfiled)), ["my"]);
        let not_work =
            SearchOptions { not_folder_ids: Some(HashSet::from([Some("work".into())])), ..Default::default() };
        assert_eq!(ids(&index.search_parsed(&parse_query("postgres"), &not_work)), ["my"]);
    }

    #[test]
    fn edits_replace_postings_and_moves_keep_them() {
        let mut index = sample();
        index.add(&doc("pg", "Renamed", "# Renamed\nnothing here", 4, Some("work")));
        assert!(index.search("postgres", 10).iter().all(|h| h.id != "pg"));
        assert!(index.ids_for_tag("db").iter().all(|id| id != "pg"));
        index.set_folder("pg", None);
        let unfiled = SearchOptions { folder_ids: Some(HashSet::from([None])), ..Default::default() };
        assert_eq!(ids(&index.search_parsed(&parse_query("renamed"), &unfiled)), ["pg"]);
        index.remove("pg");
        assert!(index.search("renamed", 10).is_empty());
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn tag_version_moves_only_when_tags_change() {
        let mut index = sample();
        let v = index.tag_version();
        index.add(&doc("pg", "Postgres tuning", "# Postgres tuning\nedited text #db", 5, None));
        assert_eq!(index.tag_version(), v);
        index.add(&doc("pg", "Postgres tuning", "# Postgres tuning\nno tags", 6, None));
        assert!(index.tag_version() > v);
    }

    #[test]
    fn limits_apply() {
        assert_eq!(sample().search("po", 2).len(), 2);
    }
}
