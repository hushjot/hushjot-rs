//! The local store: what this device holds, **ciphertext only**.
//!
//! The rule is the one the app's IndexedDB follows. Notes, folders and history
//! versions are stored exactly as sealed — byte for byte what the server holds
//! — so copying the store gets someone what a breach of the backend would give
//! them and nothing more. The search index is never stored; it is rebuilt in
//! memory after unlock.
//!
//! Alongside the sealed rows the store keeps the sync bookkeeping: the outbox
//! of local changes not yet pushed, each note's server revision (`sync:` keys
//! in the key-value table), the pull cursor and the account binding.
//!
//! [`MemoryStore`] is for tests and ephemeral use; [`SqliteStore`] (feature
//! `sqlite-store`) is the durable one. Implement [`Store`] yourself to keep the
//! rows in your application's own database.

mod memory;
#[cfg(feature = "sqlite-store")]
mod sqlite;

pub use memory::MemoryStore;
#[cfg(feature = "sqlite-store")]
pub use sqlite::SqliteStore;

use hushjot_core::history::VersionReason;

use crate::error::Result;

/// A note row. `revision` is the **local** counter, which advances on every
/// local write and is never sent to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNote {
    /// The note id.
    pub id: String,
    /// Sealed meta.
    pub meta: Vec<u8>,
    /// Sealed body; `None` on a tombstone.
    pub body: Option<Vec<u8>>,
    /// Last change (ms).
    pub updated_at: i64,
    /// Local revision.
    pub revision: i64,
    /// A tombstone.
    pub deleted: bool,
}

/// A folder row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFolder {
    /// The folder id.
    pub id: String,
    /// Sealed `{name, parentId}`.
    pub data: Vec<u8>,
    /// Creation time (ms).
    pub created_at: i64,
    /// Last change (ms).
    pub updated_at: i64,
    /// A tombstone.
    pub deleted: bool,
}

/// What an outbox entry refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A note.
    Note,
    /// A folder.
    Folder,
}

#[cfg_attr(not(feature = "sqlite-store"), allow(dead_code))]
impl ChangeKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Note => "note",
            ChangeKind::Folder => "folder",
        }
    }

    pub(crate) fn parse(s: &str) -> Self {
        if s == "folder" {
            ChangeKind::Folder
        } else {
            ChangeKind::Note
        }
    }
}

/// One outbox entry. `token` changes on every enqueue, so a push that started
/// before a newer edit cannot dequeue the newer edit's entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedChange {
    /// The note or folder id.
    pub id: String,
    /// Which.
    pub kind: ChangeKind,
    /// The enqueue token.
    pub token: i64,
}

/// Where a version stands relative to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionState {
    /// Waiting to upload.
    Queued,
    /// Uploaded.
    Pushed,
    /// Kept on this device only (the month's history allowance was used up).
    Local,
}

#[cfg_attr(not(feature = "sqlite-store"), allow(dead_code))]
impl VersionState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            VersionState::Queued => "queued",
            VersionState::Pushed => "pushed",
            VersionState::Local => "local",
        }
    }

    pub(crate) fn parse(s: &str) -> Self {
        match s {
            "pushed" => VersionState::Pushed,
            "local" => VersionState::Local,
            _ => VersionState::Queued,
        }
    }
}

/// A history version kept on this device: a byte-for-byte copy of a note's
/// stored ciphertext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVersion {
    /// The note.
    pub note_id: String,
    /// The body's nonce in hex.
    pub seal_id: String,
    /// When this text was last current.
    pub at: i64,
    /// When this device took the copy.
    pub captured_at: i64,
    /// When it expires.
    pub expire_at: i64,
    /// Why.
    pub reason: VersionReason,
    /// Sealed meta.
    pub meta: Vec<u8>,
    /// Sealed body.
    pub body: Vec<u8>,
    /// Upload state.
    pub state: VersionState,
}

impl StoredVersion {
    /// Its size, meta plus body.
    pub fn bytes(&self) -> usize {
        self.meta.len() + self.body.len()
    }
}

/// Durable local state. Every method is synchronous and must be safe to call
/// from several threads; implementations serialise internally.
pub trait Store: Send + Sync {
    /// A note row.
    fn note(&self, id: &str) -> Result<Option<StoredNote>>;
    /// Insert or replace a note row.
    fn put_note(&self, note: &StoredNote) -> Result<()>;
    /// Every note row, tombstones included.
    fn notes(&self) -> Result<Vec<StoredNote>>;
    /// A folder row.
    fn folder(&self, id: &str) -> Result<Option<StoredFolder>>;
    /// Insert or replace a folder row.
    fn put_folder(&self, folder: &StoredFolder) -> Result<()>;
    /// Every folder row, tombstones included.
    fn folders(&self) -> Result<Vec<StoredFolder>>;

    /// Queue a change for the next push, with a fresh token.
    fn enqueue(&self, id: &str, kind: ChangeKind) -> Result<()>;
    /// Remove an entry — only if its token still matches, when one is given.
    fn dequeue(&self, id: &str, token: Option<i64>) -> Result<()>;
    /// The outbox, oldest first.
    fn queued(&self) -> Result<Vec<QueuedChange>>;
    /// Whether an id is in the outbox.
    fn is_queued(&self, id: &str) -> Result<bool>;

    /// A key-value entry.
    fn get(&self, key: &str) -> Result<Option<String>>;
    /// Set (or, with `None`, remove) a key-value entry.
    fn set(&self, key: &str, value: Option<&str>) -> Result<()>;
    /// Remove every key starting with `prefix`.
    fn clear_prefix(&self, prefix: &str) -> Result<()>;

    /// Keep a version; a version already held (same note and seal) is left as is.
    fn put_version(&self, version: &StoredVersion) -> Result<()>;
    /// A note's versions, newest capture first.
    fn versions_for(&self, note_id: &str) -> Result<Vec<StoredVersion>>;
    /// Versions waiting to upload, oldest capture first.
    fn pending_versions(&self, limit: usize) -> Result<Vec<StoredVersion>>;
    /// Move a version to `state`.
    fn set_version_state(&self, note_id: &str, seal_id: &str, state: VersionState) -> Result<()>;
    /// Drop versions that expired before `now`.
    fn prune_versions(&self, now: i64) -> Result<()>;

    /// Erase everything.
    fn wipe(&self) -> Result<()>;
}
