//! The sync boundary (the app's `src/core/sync/adapter.ts`).
//!
//! Everything that crosses it is ciphertext. The [`Backend`] trait names what
//! sync needs from a server and nothing about how it is reached;
//! [`crate::firestore::FirestoreBackend`] is the real one, and the test suite
//! drives the sync engine against an in-memory fake that enforces the same
//! rules (revision compare-and-swap, metering, refusals).
//!
//! Implementing `Backend` yourself is supported but rarely needed: you must
//! uphold every invariant in `docs/protocol.md`, and the server will refuse
//! writes that do not.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use hushjot_core::history::VersionReason;
use hushjot_core::key_record::KeyRecord;
use hushjot_core::usage::Usage;

use crate::error::Result;

/// A note as the server holds it: still sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNote {
    /// The note id.
    pub id: String,
    /// Sealed meta.
    pub meta: Vec<u8>,
    /// Sealed body; absent on a tombstone.
    pub body: Option<Vec<u8>>,
    /// The **server** revision: accepted writes, not keystrokes.
    pub revision: i64,
    /// When the note was last changed (client clock, ms).
    pub updated_at: i64,
    /// A tombstone.
    pub deleted: bool,
    /// The server's write time (ms). Orders the delta pull.
    pub synced_at: i64,
}

impl RemoteNote {
    /// Its charged-size basis: meta plus body.
    pub fn stored_bytes(&self) -> i64 {
        hushjot_core::usage::counted_note_bytes(Some(&self.meta), self.body.as_deref())
    }
}

/// A folder as the server holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFolder {
    /// The folder id.
    pub id: String,
    /// Sealed `{name, parentId}`.
    pub data: Vec<u8>,
    /// Creation time (ms).
    pub created_at: i64,
    /// Last change (ms); last write wins.
    pub updated_at: i64,
    /// A tombstone.
    pub deleted: bool,
    /// The server's write time (ms).
    pub synced_at: i64,
}

/// One position in a collection's change stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// `syncedAt` of the last document consumed (ms).
    pub at: i64,
    /// Its id, or empty at the very start.
    pub id: String,
}

/// Where the delta pull has reached, one position per collection.
///
/// Two positions, not one: the collections are separate queries, and a shared
/// position let one collection's newest timestamp carry the cursor past
/// documents the other had not delivered yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// Notes.
    pub notes: Position,
    /// Folders.
    pub folders: Position,
}

/// One page of the delta pull.
#[derive(Debug, Clone, Default)]
pub struct PullPage {
    /// Changed notes, oldest first.
    pub notes: Vec<RemoteNote>,
    /// Changed folders, oldest first.
    pub folders: Vec<RemoteFolder>,
    /// Where the next page starts.
    pub cursor: Cursor,
    /// Whether a page was full.
    pub has_more: bool,
}

/// A version of a note, as the server holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteVersion {
    /// The note it belongs to.
    pub note_id: String,
    /// Its id: the body's nonce in hex ([`hushjot_core::history::seal_id_of`]).
    pub id: String,
    /// Sealed meta, byte for byte as the note held it.
    pub meta: Vec<u8>,
    /// Sealed body, byte for byte.
    pub body: Vec<u8>,
    /// When this text was last current (the note's `updatedAt`).
    pub at: i64,
    /// Why it was taken.
    pub reason: VersionReason,
    /// When the server will delete it (ms).
    pub expire_at: i64,
    /// When it was uploaded (server clock, ms), when known.
    pub created_at: Option<i64>,
}

/// A published share: the public half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteShare {
    /// The share id.
    pub id: String,
    /// The payload, sealed under the share key.
    pub payload: Vec<u8>,
    /// Expiry (ms).
    pub expire_at: i64,
}

/// A published share: the owner's private record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareIndexEntry {
    /// The share id.
    pub id: String,
    /// The note it was published from.
    pub note_id: String,
    /// The share key, sealed under the vault key with the `share-key` context.
    pub key: Vec<u8>,
    /// Expiry (ms).
    pub expire_at: i64,
    /// When published (server clock, ms).
    pub created_at: i64,
}

/// What `users/{uid}/keys/primary` holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRecordDoc {
    /// No vault has been created.
    Missing,
    /// The account was reset and has no vault until one is created again.
    Tombstone,
    /// The vault's key record.
    Record(Box<KeyRecord>),
}

/// The answer to "is a refusal a maintenance pause?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseVerdict {
    /// Every client is refused for now.
    Paused,
    /// The service is running; the refusal was about this account.
    Running,
    /// Could not tell.
    Unknown,
}

/// What a sync engine needs from a server. All bytes are ciphertext.
///
/// Errors are the SDK's [`crate::Error`], with these meanings the engine
/// relies on: `RevisionConflict` (the compare-and-swap refused a note),
/// `QuotaExceeded`, `PushLimit`, `PayloadTooLarge`, `NotApproved`,
/// `SessionExpired` and `Transient`.
#[async_trait]
pub trait Backend: Send + Sync {
    /// The signed-in account's uid.
    async fn uid(&self) -> String;
    /// The account's key record.
    async fn key_record(&self) -> Result<KeyRecordDoc>;
    /// One note, or `None`.
    async fn fetch_note(&self, id: &str) -> Result<Option<RemoteNote>>;
    /// Write a note at `note.revision` (which must be the stored revision + 1),
    /// metered against `stored_bytes`, the size of what it replaces.
    async fn push_note(&self, note: &RemoteNote, stored_bytes: i64) -> Result<()>;
    /// Write a folder (last write wins), metered against `stored_bytes`.
    async fn push_folder(&self, folder: &RemoteFolder, stored_bytes: i64) -> Result<()>;
    /// One page of changes after `cursor`.
    async fn pull(&self, cursor: &Cursor, limit: usize) -> Result<PullPage>;
    /// Upload versions, one commit each. Returns those refused (this month's
    /// history allowance is used up); they stay on the device.
    async fn push_versions(&self, versions: &[RemoteVersion]) -> Result<Vec<RemoteVersion>>;
    /// A note's unexpired versions, newest first.
    async fn list_versions(&self, note_id: &str) -> Result<Vec<RemoteVersion>>;
    /// Publish or re-publish a share, with its private index entry, in one commit.
    async fn publish_share(&self, share: &RemoteShare, index: &ShareIndexEntry) -> Result<()>;
    /// Withdraw a share: the public document first, then the private record.
    async fn revoke_share(&self, id: &str) -> Result<()>;
    /// Unexpired shares, newest first.
    async fn list_shares(&self) -> Result<Vec<ShareIndexEntry>>;
    /// The account's meter and allowance.
    async fn usage(&self) -> Result<Usage>;
    /// After a refusal: is every client being refused?
    async fn probe_pause(&self) -> PauseVerdict;
}
