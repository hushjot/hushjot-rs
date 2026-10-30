//! Local search: the query grammar, the token index and phrase verification.
//!
//! The typical flow, which `hushjot::Vault::search` runs for you:
//!
//! 1. [`parse_query`] the user's text;
//! 2. resolve `parsed.folders` to ids with
//!    [`crate::folders::folders_matching`] (with `prefix: true`) and
//!    [`crate::folders::subtree_ids`];
//! 3. [`SearchIndex::search_parsed`] with those ids;
//! 4. if there are phrases, [`verify_phrases`] against the note bodies.
//!
//! ```
//! use hushjot_core::search::{parse_query, verify_phrases, IndexDocument, SearchIndex, SearchOptions};
//!
//! let mut index = SearchIndex::new();
//! index.add(&IndexDocument {
//!     id: "n1".into(),
//!     title: "Pools".into(),
//!     content: "# Pools\nThe connection pool is full.".into(),
//!     updated_at: 1,
//!     folder_id: None,
//! });
//! let parsed = parse_query("\"connection pool\"");
//! let candidates = index.search_parsed(&parsed, &SearchOptions::default());
//! let hits = verify_phrases(&candidates, &parsed, 10, |_| Some("# Pools\nThe connection pool is full.".into()));
//! assert_eq!(hits.len(), 1);
//! ```

mod index;
mod phrase;
mod query;

pub use index::{tokenize, IndexDocument, SearchHit, SearchIndex, SearchOptions, TagCount};
pub use phrase::{verify_phrases, VERIFY_MAX};
pub use query::{has_positive_term, normalise_phrase, parse_query, ParsedQuery};
