//! The versioned authenticated-encryption envelope (the app's
//! `src/core/crypto/envelope.ts` and `binding.ts`).
//!
//! AES-256-GCM from RustCrypto. Nothing here invents cryptography; this module
//! is framing and versioning around an audited primitive.
//!
//! ## Wire format
//!
//! ```text
//! [0]      version  (u8)   1 = header-only AAD, 2 = bound to a record
//! [1]      alg id   (u8)   1 = AES-256-GCM
//! [2..14)  nonce    (12 bytes, random per encryption, never reused)
//! [14..]   ciphertext || 16-byte GCM tag
//! ```
//!
//! The two header bytes are authenticated, so the header cannot be rewritten
//! to a weaker algorithm without the tag failing.
//!
//! ## Version 2: bound to a record
//!
//! A version-1 envelope proves its bytes were not modified but not *where*
//! they belong: a valid body from note A, moved into note B's slot by whoever
//! holds the database, would open as B. Version 2 appends the record context
//! (`hushjot:<kind>:<id>`, see [`record_context`]) to the AAD, so the tag only
//! verifies in the slot it was sealed for. The context is not stored; the
//! opener supplies it. Every note meta, note body, folder and share-key record
//! sealed under the vault key **must** be sealed with its context.
//!
//! Sealing without a context still writes version 1, deliberately: the key
//! record's wraps and a share's payload are each the only thing sealed under
//! their key, so there is nothing to swap them with.
//!
//! ## The legacy window
//!
//! Older vaults still hold version-1 records. While [`ACCEPT_UNBOUND_LEGACY`]
//! is true a version-1 envelope opens where a context was asked for. Closing
//! the window is a format change and moves [`crate::FORMAT_VERSION`].
//!
//! ## Nonces
//!
//! A fresh 96-bit random nonce per encryption, never a counter that could
//! restart after a backup restore. At 2^32 encryptions under one key the
//! collision probability is around 2^-33.

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::bytes::random_bytes;
use crate::error::DecryptError;

/// The unbound format: the AAD is the header alone.
pub const ENVELOPE_VERSION: u8 = 1;
/// The bound format: the AAD is the header followed by the record context.
pub const ENVELOPE_VERSION_BOUND: u8 = 2;
/// The only algorithm: AES-256-GCM.
pub const ALG_AES_256_GCM: u8 = 1;
/// Whether a version-1 envelope may be opened where a record context is given.
pub const ACCEPT_UNBOUND_LEGACY: bool = true;

const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const HEADER_BYTES: usize = 2 + NONCE_BYTES;
/// The sealed size of an empty plaintext: header, nonce and tag.
pub const EMPTY_SEAL_BYTES: usize = HEADER_BYTES + TAG_BYTES;

/// A 256-bit AES key: the vault's data-encryption key, a key-encryption key,
/// or a share key.
///
/// Zeroed on drop. `Debug` prints nothing of the key, and there is no
/// `Display`, so a key cannot reach a log by accident.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Key([u8; 32]);

impl Key {
    /// Wrap 32 raw key bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Wrap a slice, which must be exactly 32 bytes.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        let array: [u8; 32] = bytes.try_into().ok()?;
        Some(Self(array))
    }

    /// A fresh random key.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&random_bytes(32));
        Self(bytes)
    }

    /// The raw key bytes. Handle with care: whatever you copy them into is
    /// yours to zero.
    pub fn expose_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.0).expect("a 32-byte key is always a valid AES-256 key")
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

impl PartialEq for Key {
    /// Constant-time, so comparing keys leaks nothing through timing.
    fn eq(&self, other: &Self) -> bool {
        let mut diff = 0u8;
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl Eq for Key {}

/// What a bound envelope can belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordKind {
    /// A note's sealed metadata (`meta`).
    NoteMeta,
    /// A note's sealed body.
    NoteBody,
    /// A folder's sealed `{name, parentId}`.
    Folder,
    /// A share's key, sealed under the vault key in the owner's share index.
    ShareKey,
}

impl RecordKind {
    /// The kind as it appears in a record context.
    pub fn as_str(self) -> &'static str {
        match self {
            RecordKind::NoteMeta => "note-meta",
            RecordKind::NoteBody => "note-body",
            RecordKind::Folder => "folder",
            RecordKind::ShareKey => "share-key",
        }
    }
}

/// The context a record's envelope is bound to: `hushjot:<kind>:<id>`.
///
/// The kind keeps a note's meta from passing as its own body; the id keeps one
/// note's body from passing as another's. Ids never contain `:`, so the string
/// is unambiguous.
pub fn record_context(kind: RecordKind, id: &str) -> String {
    format!("hushjot:{}:{}", kind.as_str(), id)
}

/// True for a version-1 envelope.
pub fn is_unbound_envelope(envelope: &[u8]) -> bool {
    envelope.first() == Some(&ENVELOPE_VERSION)
}

fn aad_for(head: &[u8], context: Option<&str>) -> Vec<u8> {
    let mut aad = head.to_vec();
    if let Some(context) = context {
        aad.extend_from_slice(context.as_bytes());
    }
    aad
}

/// Seal `plaintext` under `key`.
///
/// With a `context` the envelope is version 2 and opens only under that same
/// context; without one it is version 1.
pub fn seal(key: &Key, plaintext: &[u8], context: Option<&str>) -> Vec<u8> {
    let version = if context.is_some() { ENVELOPE_VERSION_BOUND } else { ENVELOPE_VERSION };
    let head = [version, ALG_AES_256_GCM];
    let nonce = random_bytes(NONCE_BYTES);
    let ciphertext = key
        .cipher()
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: &aad_for(&head, context) })
        .expect("AES-GCM encryption cannot fail for in-memory buffers");

    let mut out = Vec::with_capacity(HEADER_BYTES + ciphertext.len());
    out.extend_from_slice(&head);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    out
}

/// Open an envelope. Pass the same `context` it was sealed under: a version-2
/// envelope refuses to open without it, and a version-1 envelope opened with
/// one is accepted only while the legacy window is open.
pub fn open(key: &Key, envelope: &[u8], context: Option<&str>) -> Result<Vec<u8>, DecryptError> {
    if envelope.len() < EMPTY_SEAL_BYTES {
        return Err(DecryptError);
    }
    let version = envelope[0];
    let alg = envelope[1];
    let bound = version == ENVELOPE_VERSION_BOUND;
    if !bound && version != ENVELOPE_VERSION {
        return Err(DecryptError);
    }
    if alg != ALG_AES_256_GCM {
        return Err(DecryptError);
    }
    // A record in the wrong slot must look exactly like a tampered one.
    let refused = if bound { context.is_none() } else { context.is_some() && !ACCEPT_UNBOUND_LEGACY };
    if refused {
        return Err(DecryptError);
    }

    // A version-1 envelope was sealed with the header alone, whatever the
    // caller now expects of it.
    let aad = aad_for(&envelope[..2], if bound { context } else { None });
    let nonce = Nonce::from_slice(&envelope[2..HEADER_BYTES]);
    key.cipher().decrypt(nonce, Payload { msg: &envelope[HEADER_BYTES..], aad: &aad }).map_err(|_| DecryptError)
}

/// Seal UTF-8 text.
pub fn seal_text(key: &Key, text: &str, context: Option<&str>) -> Vec<u8> {
    seal(key, text.as_bytes(), context)
}

/// Open an envelope holding UTF-8 text. Invalid UTF-8 is replaced with U+FFFD,
/// as the app's `TextDecoder` does, rather than refused.
pub fn open_text(key: &Key, envelope: &[u8], context: Option<&str>) -> Result<String, DecryptError> {
    let plain = open(key, envelope, context)?;
    Ok(match String::from_utf8(plain) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_header_nonce_ciphertext_tag() {
        let key = Key::generate();
        let sealed = seal(&key, b"hello", None);
        assert_eq!(sealed.len(), EMPTY_SEAL_BYTES + 5);
        assert_eq!(&sealed[..2], &[1, 1]);
        let bound = seal(&key, b"hello", Some("hushjot:note-body:n-1"));
        assert_eq!(&bound[..2], &[2, 1]);
        assert_eq!(seal(&key, b"", None).len(), EMPTY_SEAL_BYTES);
    }

    #[test]
    fn round_trips_bound_and_unbound() {
        let key = Key::generate();
        let ctx = record_context(RecordKind::NoteBody, "n-1");
        assert_eq!(open(&key, &seal(&key, b"x", None), None).unwrap(), b"x");
        assert_eq!(open(&key, &seal(&key, b"y", Some(&ctx)), Some(&ctx)).unwrap(), b"y");
        assert_eq!(open_text(&key, &seal_text(&key, "日本語", Some(&ctx)), Some(&ctx)).unwrap(), "日本語");
    }

    #[test]
    fn nonces_are_fresh_per_seal() {
        let key = Key::generate();
        let a = seal(&key, b"same", None);
        let b = seal(&key, b"same", None);
        assert_ne!(a[2..14], b[2..14]);
        assert_ne!(a, b);
    }

    #[test]
    fn a_bound_envelope_opens_only_in_its_own_slot() {
        let key = Key::generate();
        let body_a = record_context(RecordKind::NoteBody, "a");
        let body_b = record_context(RecordKind::NoteBody, "b");
        let meta_a = record_context(RecordKind::NoteMeta, "a");
        let sealed = seal(&key, b"secret", Some(&body_a));
        assert_eq!(open(&key, &sealed, Some(&body_b)), Err(DecryptError));
        assert_eq!(open(&key, &sealed, Some(&meta_a)), Err(DecryptError));
        assert_eq!(open(&key, &sealed, None), Err(DecryptError));
    }

    #[test]
    fn a_legacy_envelope_opens_under_a_context_while_the_window_is_open() {
        let key = Key::generate();
        let sealed = seal(&key, b"old", None);
        let ctx = record_context(RecordKind::NoteMeta, "n");
        assert_eq!(open(&key, &sealed, Some(&ctx)).is_ok(), ACCEPT_UNBOUND_LEGACY);
    }

    #[test]
    fn every_single_byte_flip_is_detected() {
        let key = Key::generate();
        let ctx = record_context(RecordKind::Folder, "f");
        let sealed = seal(&key, b"tamper with me", Some(&ctx));
        for i in 0..sealed.len() {
            let mut bad = sealed.clone();
            bad[i] ^= 0x01;
            assert_eq!(open(&key, &bad, Some(&ctx)), Err(DecryptError), "flip at byte {i}");
        }
    }

    #[test]
    fn wrong_keys_short_inputs_and_unknown_headers_all_fail_the_same_way() {
        let key = Key::generate();
        let sealed = seal(&key, b"x", None);
        assert_eq!(open(&Key::generate(), &sealed, None), Err(DecryptError));
        assert_eq!(open(&key, &sealed[..29], None), Err(DecryptError));
        assert_eq!(open(&key, &[], None), Err(DecryptError));
        let mut v3 = sealed.clone();
        v3[0] = 3;
        assert_eq!(open(&key, &v3, None), Err(DecryptError));
        let mut alg2 = sealed;
        alg2[1] = 2;
        assert_eq!(open(&key, &alg2, None), Err(DecryptError));
    }

    #[test]
    fn keys_never_print() {
        let key = Key::from_bytes([7; 32]);
        assert_eq!(format!("{key:?}"), "Key(<redacted>)");
    }

    #[test]
    fn contexts_follow_the_frozen_spelling() {
        assert_eq!(record_context(RecordKind::NoteMeta, "x"), "hushjot:note-meta:x");
        assert_eq!(record_context(RecordKind::NoteBody, "x"), "hushjot:note-body:x");
        assert_eq!(record_context(RecordKind::Folder, "x"), "hushjot:folder:x");
        assert_eq!(record_context(RecordKind::ShareKey, "x"), "hushjot:share-key:x");
    }
}
