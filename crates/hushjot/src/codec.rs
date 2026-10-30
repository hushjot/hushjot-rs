//! Sealing and opening rows under the vault key (the app's
//! `src/core/repo/encryptedCodec.ts`).
//!
//! Every envelope sealed under the vault key names its record — kind and id —
//! so a blob only opens in the slot it was written for. Title and excerpt are
//! derived here, at the one point that holds the text, and every other meta
//! field is carried over by the caller: a full-replace write that dropped one
//! would delete it on every device.

use hushjot_core::envelope::{open, open_text, record_context, seal, seal_text, Key, RecordKind};
use hushjot_core::meta::{note_meta_json, parse_note_meta, FolderPayload, NoteMetaPayload};
use hushjot_core::notes::{derive_title, excerpt_of};

use crate::error::Result;

/// Seal a note's meta.
pub fn seal_meta(key: &Key, id: &str, meta: &NoteMetaPayload) -> Vec<u8> {
    seal(key, note_meta_json(meta).as_bytes(), Some(&record_context(RecordKind::NoteMeta, id)))
}

/// Open a note's meta.
pub fn open_meta(key: &Key, id: &str, sealed: &[u8]) -> Result<NoteMetaPayload> {
    let plain = open(key, sealed, Some(&record_context(RecordKind::NoteMeta, id)))?;
    Ok(parse_note_meta(&plain)?)
}

/// Seal a note's body.
pub fn seal_body(key: &Key, id: &str, content: &str) -> Vec<u8> {
    seal_text(key, content, Some(&record_context(RecordKind::NoteBody, id)))
}

/// Open a note's body.
pub fn open_body(key: &Key, id: &str, sealed: &[u8]) -> Result<String> {
    Ok(open_text(key, sealed, Some(&record_context(RecordKind::NoteBody, id)))?)
}

/// The meta for `content`, carrying `previous`'s folder, pin and created date.
pub fn meta_for_content(content: &str, previous: &NoteMetaPayload) -> NoteMetaPayload {
    let excerpt = excerpt_of(content);
    NoteMetaPayload {
        title: derive_title(content),
        folder_id: previous.folder_id.clone(),
        pinned_at: previous.pinned_at,
        excerpt: (!excerpt.is_empty()).then_some(excerpt),
        created_at: previous.created_at,
    }
}

/// Seal a folder.
pub fn seal_folder(key: &Key, id: &str, payload: &FolderPayload) -> Vec<u8> {
    seal(key, payload.to_json().as_bytes(), Some(&record_context(RecordKind::Folder, id)))
}

/// Open a folder.
pub fn open_folder(key: &Key, id: &str, sealed: &[u8]) -> Result<FolderPayload> {
    let plain = open(key, sealed, Some(&record_context(RecordKind::Folder, id)))?;
    Ok(FolderPayload::parse(&plain)?)
}

/// Seal a share key into the owner's share index.
pub fn seal_share_key(key: &Key, share_id: &str, encoded: &str) -> Vec<u8> {
    seal_text(key, encoded, Some(&record_context(RecordKind::ShareKey, share_id)))
}

/// Open a share key from the owner's share index.
pub fn open_share_key(key: &Key, share_id: &str, sealed: &[u8]) -> Result<String> {
    Ok(open_text(key, sealed, Some(&record_context(RecordKind::ShareKey, share_id)))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_open_only_under_their_own_id_and_kind() {
        let key = Key::generate();
        let meta = NoteMetaPayload { title: "T".into(), ..Default::default() };
        let sealed = seal_meta(&key, "a", &meta);
        assert_eq!(open_meta(&key, "a", &sealed).unwrap(), meta);
        assert!(open_meta(&key, "b", &sealed).is_err());
        assert!(open_body(&key, "a", &sealed).is_err());
        let folder = FolderPayload { name: "F".into(), parent_id: None };
        assert_eq!(open_folder(&key, "f", &seal_folder(&key, "f", &folder)).unwrap(), folder);
        assert_eq!(open_share_key(&key, "s", &seal_share_key(&key, "s", "k")).unwrap(), "k");
    }

    #[test]
    fn new_content_keeps_every_carried_field() {
        let previous = NoteMetaPayload {
            title: "old".into(),
            folder_id: Some("f".into()),
            pinned_at: Some(3),
            excerpt: Some("old".into()),
            created_at: Some(1),
        };
        let next = meta_for_content("# New\nsecond line", &previous);
        assert_eq!(next.title, "New");
        assert_eq!(next.excerpt.as_deref(), Some("second line"));
        assert_eq!(next.folder_id.as_deref(), Some("f"));
        assert_eq!(next.pinned_at, Some(3));
        assert_eq!(next.created_at, Some(1));
        assert_eq!(meta_for_content("# Only", &previous).excerpt, None);
    }
}
