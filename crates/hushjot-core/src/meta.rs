//! The sealed JSON shapes: a note's metadata, a folder, a share payload (the
//! app's `src/core/repo/codec.ts` and `src/core/share/payload.ts`).
//!
//! ## Field order and omission are part of the contract
//!
//! [`note_meta_json`] writes `title`, `folderId`, then `pinnedAt`, `excerpt`
//! and `createdAt` **only when set**. An unpinned note with no excerpt is then
//! byte-for-byte the shape it had before pins existed, and two clients sealing
//! the same note produce the same plaintext. The contract vectors pin the exact
//! strings.
//!
//! ## The full-replace trap
//!
//! Every Firestore write replaces the whole document, so whoever re-seals a
//! meta must carry every field across. Dropping `createdAt` resets a note's
//! created date on every device; dropping `excerpt` blanks the second line of
//! its row on a phone; dropping `pinnedAt` unpins it. Readers go through
//! [`read_note_meta`] so an absent field is a typed absence, not a surprise.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::FormatError;

/// A note's sealed metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NoteMetaPayload {
    /// The stored title. Usually [`crate::notes::derive_title`] of the body,
    /// but not always: a conflict copy carries a suffix.
    pub title: String,
    /// The folder the note is filed in, or `None` for the top level.
    pub folder_id: Option<String>,
    /// When the note was pinned (epoch ms), or `None`.
    pub pinned_at: Option<i64>,
    /// The opening of the body after the title, for a phone row. `None` when
    /// empty.
    pub excerpt: Option<String>,
    /// When the note was created (epoch ms). `None` on notes written before
    /// the field existed — never invent one.
    pub created_at: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MetaOut<'a> {
    title: &'a str,
    folder_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    excerpt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<i64>,
}

/// The JSON a meta payload is sealed as. See the module notes on order and
/// omission.
pub fn note_meta_json(payload: &NoteMetaPayload) -> String {
    let out = MetaOut {
        title: &payload.title,
        folder_id: payload.folder_id.as_deref(),
        pinned_at: payload.pinned_at,
        excerpt: payload.excerpt.as_deref().filter(|e| !e.is_empty()),
        created_at: payload.created_at,
    };
    serde_json::to_string(&out).expect("meta serialisation cannot fail")
}

fn as_ms(value: Option<&Value>) -> Option<i64> {
    let number = value?.as_number()?;
    number.as_i64().or_else(|| number.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
}

/// Normalise decoded meta JSON, supplying the defaults the app supplies.
pub fn read_note_meta(value: &Value) -> NoteMetaPayload {
    NoteMetaPayload {
        title: value.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
        folder_id: value.get("folderId").and_then(Value::as_str).map(str::to_string),
        pinned_at: as_ms(value.get("pinnedAt")),
        excerpt: value.get("excerpt").and_then(Value::as_str).filter(|e| !e.is_empty()).map(str::to_string),
        created_at: as_ms(value.get("createdAt")),
    }
}

/// Parse meta plaintext.
pub fn parse_note_meta(json: &[u8]) -> Result<NoteMetaPayload, FormatError> {
    let value: Value = serde_json::from_slice(json).map_err(|_| FormatError::new("note meta is not JSON"))?;
    Ok(read_note_meta(&value))
}

/// A folder's sealed payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderPayload {
    /// The folder's name.
    #[serde(default)]
    pub name: String,
    /// The parent folder, or `None` for the top level.
    #[serde(default)]
    pub parent_id: Option<String>,
}

impl FolderPayload {
    /// The JSON it is sealed as: `{"name":…,"parentId":…}`.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("folder serialisation cannot fail")
    }

    /// Parse folder plaintext.
    pub fn parse(json: &[u8]) -> Result<Self, FormatError> {
        serde_json::from_slice(json).map_err(|_| FormatError::new("folder payload is not JSON"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_meta_omits_every_optional_field() {
        let meta = NoteMetaPayload { title: "Shopping".into(), ..Default::default() };
        assert_eq!(note_meta_json(&meta), r#"{"title":"Shopping","folderId":null}"#);
    }

    #[test]
    fn every_field_appears_in_the_fixed_order() {
        let meta = NoteMetaPayload {
            title: "T".into(),
            folder_id: Some("f".into()),
            pinned_at: Some(5),
            excerpt: Some("e".into()),
            created_at: Some(0),
        };
        assert_eq!(note_meta_json(&meta), r#"{"title":"T","folderId":"f","pinnedAt":5,"excerpt":"e","createdAt":0}"#);
    }

    #[test]
    fn empty_excerpts_are_omitted_and_read_back_as_absent() {
        let meta = NoteMetaPayload { title: "T".into(), excerpt: Some(String::new()), ..Default::default() };
        assert_eq!(note_meta_json(&meta), r#"{"title":"T","folderId":null}"#);
        assert_eq!(read_note_meta(&json!({"title": "T", "excerpt": ""})).excerpt, None);
    }

    #[test]
    fn reading_supplies_defaults_for_absent_and_mistyped_fields() {
        let meta = read_note_meta(&json!({"pinnedAt": "soon", "createdAt": 0}));
        assert_eq!(meta.title, "");
        assert_eq!(meta.folder_id, None);
        assert_eq!(meta.pinned_at, None);
        assert_eq!(meta.created_at, Some(0));
    }

    #[test]
    fn escaping_matches_json_stringify() {
        let meta = NoteMetaPayload { title: "a\"b\\c\n\u{1}é🔐/".into(), ..Default::default() };
        assert_eq!(note_meta_json(&meta), "{\"title\":\"a\\\"b\\\\c\\n\\u0001é🔐/\",\"folderId\":null}");
    }

    #[test]
    fn folders_serialise_name_then_parent() {
        let folder = FolderPayload { name: "Rust".into(), parent_id: Some("f-1".into()) };
        assert_eq!(folder.to_json(), r#"{"name":"Rust","parentId":"f-1"}"#);
        assert_eq!(FolderPayload::parse(br#"{"name":"x"}"#).unwrap().parent_id, None);
    }
}
