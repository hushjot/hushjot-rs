//! An unlocked vault: the notes, folders, history, shares and search an
//! editor works with (the app's `Workspace`, minus the UI).
//!
//! ## Local-first
//!
//! Every write — create, edit, pin, move, delete — seals the change, stores it
//! and queues it, and returns. Nothing on that path waits for the network: the
//! sync engine pushes on its own cadence, and an editor stays fully usable
//! offline, signed out, or while the service is paused. Reads come from the
//! local store and are decrypted on demand.
//!
//! ## What the vault guarantees
//!
//! - Only ciphertext reaches the store or the server.
//! - Every vault-key seal is bound to its record (`hushjot:<kind>:<id>`).
//! - Every meta write carries folder, pin and created date across, and derives
//!   title and excerpt from the text.
//! - A delete is a tombstone, never a hard delete, and its final text is kept
//!   in history first.
//! - An edit's previous text is kept in history at the start of each editing
//!   session and at least every ten minutes.
//! - A conflict is never resolved destructively: the losing side becomes a
//!   separate "(conflict copy …)" note.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, watch};

use hushjot_core::envelope::Key;
use hushjot_core::folders::{self, Folder, FolderNode};
use hushjot_core::history::{seal_id_of, tier_for, utc_day_start, utc_hour_start, CaptureTracker, VersionReason};
use hushjot_core::meta::{FolderPayload, NoteMetaPayload};
use hushjot_core::notes::{order_for_list, Listable, NoteSort};
use hushjot_core::search::{self as core_search, IndexDocument, SearchIndex, SearchOptions};
use hushjot_core::share::{self as core_share, ShareExpiry, SharePayload, ShareRef};
use hushjot_core::usage::Usage;

use crate::backend::{Backend, RemoteShare, RemoteVersion, ShareIndexEntry};
use crate::codec;
use crate::engine::{Engine, EngineHandle, SyncEvent, SyncOptions, SyncStatus};
use crate::error::{Error, Result};
use crate::store::{ChangeKind, Store, StoredFolder, StoredNote, StoredVersion, VersionState};

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A folder's next `updatedAt`. Folders are last-write-wins on this stamp and
/// a remote copy applies only when strictly newer, so a change made in the same
/// millisecond as the previous one must still move it forward.
fn next_stamp(previous: i64) -> i64 {
    now_ms().max(previous + 1)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A note as a list shows it: no body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteSummary {
    /// The note id.
    pub id: String,
    /// Its title.
    pub title: String,
    /// Its folder, or `None` at the top level.
    pub folder_id: Option<String>,
    /// When it was pinned, if it is.
    pub pinned_at: Option<i64>,
    /// When it was created. Falls back to `updated_at` for notes written
    /// before the field existed, the same answer every device reaches.
    pub created_at: i64,
    /// When it was last changed.
    pub updated_at: i64,
    /// The text after the title, for a second line.
    pub excerpt: Option<String>,
}

impl Listable for NoteSummary {
    fn id(&self) -> &str {
        &self.id
    }
    fn title(&self) -> &str {
        &self.title
    }
    fn pinned_at(&self) -> Option<i64> {
        self.pinned_at
    }
    fn updated_at(&self) -> i64 {
        self.updated_at
    }
    fn created_at(&self) -> i64 {
        self.created_at
    }
}

/// A note with its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Everything a list shows.
    pub summary: NoteSummary,
    /// The Markdown.
    pub content: String,
}

/// A deleted note that can still be brought back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedNote {
    /// The note id.
    pub id: String,
    /// Its title when deleted.
    pub title: String,
    /// When it was deleted.
    pub deleted_at: i64,
}

/// A version of a note, as a history list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    /// The version id (the sealed body's nonce, hex).
    pub id: String,
    /// When this text was last current.
    pub at: i64,
    /// Why it was kept.
    pub reason: VersionReason,
    /// When it expires.
    pub expire_at: i64,
    /// Whether this device holds it (otherwise it is fetched from the server).
    pub local: bool,
}

/// An opened version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteVersion {
    /// Its listing.
    pub info: VersionInfo,
    /// The title it had.
    pub title: String,
    /// The folder it was in.
    pub folder_id: Option<String>,
    /// The text.
    pub content: String,
}

/// A published share link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedShare {
    /// The share id.
    pub id: String,
    /// The note it was published from.
    pub note_id: String,
    /// The link. Anyone holding it can read the snapshot until it expires.
    pub url: String,
    /// Expiry (ms).
    pub expire_at: i64,
}

/// One search result.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// The note.
    pub note: NoteSummary,
    /// Relevance; higher is better.
    pub score: f64,
    /// Whether a term matched the title.
    pub in_title: bool,
}

/// The vault's write guard, held ([`VaultInner::write_guard`]). Note rows read
/// through it are [`Held`]: the only rows the vault's write helpers take
/// ([`VaultInner::write_content`], `write_meta`, `tombstone`), so those never
/// write from a row read before taking it. The engine's own note writes, and
/// `create` and `undelete`, hold the guard and write the store directly.
pub(crate) struct Writing<'a> {
    _guard: std::sync::MutexGuard<'a, ()>,
    inner: &'a VaultInner,
}

/// A note row read while the write guard is held, valid while that guard is.
pub(crate) struct Held<'w> {
    row: StoredNote,
    _guard: std::marker::PhantomData<&'w ()>,
}

impl std::ops::Deref for Held<'_> {
    type Target = StoredNote;

    fn deref(&self) -> &StoredNote {
        &self.row
    }
}

impl Writing<'_> {
    /// Note `id`'s row as it is, tombstone or not.
    pub fn note(&self, id: &str) -> Result<Option<Held<'_>>> {
        Ok(self.inner.store.note(id)?.map(|row| Held { row, _guard: std::marker::PhantomData }))
    }

    /// Every note row, tombstones included.
    pub fn notes(&self) -> Result<Vec<Held<'_>>> {
        Ok(self.inner.store.notes()?.into_iter().map(|row| Held { row, _guard: std::marker::PhantomData }).collect())
    }

    /// Note `id`'s row, unless it's gone or a tombstone.
    pub fn live_note(&self, id: &str) -> Result<Held<'_>> {
        self.note(id)?.filter(|n| !n.deleted).ok_or_else(|| Error::NotFound(format!("note {id}")))
    }
}

pub(crate) struct VaultInner {
    pub uid: String,
    pub email: String,
    pub key: Key,
    pub store: Arc<dyn Store>,
    pub backend: Arc<dyn Backend>,
    pub search: Mutex<SearchIndex>,
    pub capture: Mutex<CaptureTracker>,
    /// Held across every read-then-write of a note row, local or remote: an
    /// edit, pin, move, delete, restore or undelete here, a folder's delete of
    /// its notes, and the engine applying a pulled note or settling a conflict.
    /// Without it a pull landing between an edit's read and its write is
    /// overwritten unseen, and `update_if_unchanged` could not promise
    /// anything. Folder rows aren't covered: a folder's own changes, local or
    /// pulled, are last-write-wins as before. Taken before any other lock of the
    /// vault's, and never held across an await.
    pub writes: Mutex<()>,
    pub events: broadcast::Sender<SyncEvent>,
    pub status: watch::Sender<SyncStatus>,
    pub pending: AtomicUsize,
    pub engine: Mutex<Option<EngineHandle>>,
    pub share_origin: String,
}

impl VaultInner {
    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One note write at a time; see [`VaultInner::writes`].
    pub fn write_guard(&self) -> Writing<'_> {
        Writing { _guard: Self::lock(&self.writes), inner: self }
    }

    pub fn search_index(&self) -> std::sync::MutexGuard<'_, SearchIndex> {
        Self::lock(&self.search)
    }

    /// Queue a change and tell the engine, if one is running.
    pub fn queue(&self, id: &str, kind: ChangeKind, immediate: bool) -> Result<()> {
        self.store.enqueue(id, kind)?;
        self.pending.store(self.store.queued()?.len(), Ordering::Relaxed);
        if let Some(engine) = Self::lock(&self.engine).as_ref() {
            engine.queued(id, immediate);
        }
        Ok(())
    }

    pub fn live_note(&self, id: &str) -> Result<StoredNote> {
        match self.store.note(id)? {
            Some(n) if !n.deleted => Ok(n),
            _ => Err(Error::NotFound(format!("note {id}"))),
        }
    }

    pub fn live_folder_exists(&self, id: &str) -> Result<bool> {
        Ok(self.store.folder(id)?.is_some_and(|f| !f.deleted))
    }

    pub fn summary_of(&self, row: &StoredNote) -> Result<NoteSummary> {
        let meta = codec::open_meta(&self.key, &row.id, &row.meta)?;
        Ok(NoteSummary {
            id: row.id.clone(),
            title: meta.title,
            folder_id: meta.folder_id,
            pinned_at: meta.pinned_at,
            created_at: meta.created_at.unwrap_or(row.updated_at),
            updated_at: row.updated_at,
            excerpt: meta.excerpt,
        })
    }

    /// Re-read one note into the search index (or drop it).
    pub fn reindex(&self, id: &str) {
        let row = self.store.note(id).ok().flatten();
        let mut index = self.search_index();
        match row {
            Some(row) if !row.deleted => {
                let meta = codec::open_meta(&self.key, id, &row.meta).ok();
                let content = row.body.as_ref().and_then(|b| codec::open_body(&self.key, id, b).ok());
                match (meta, content) {
                    (Some(meta), Some(content)) => index.add(&IndexDocument {
                        id: id.to_string(),
                        title: meta.title,
                        content,
                        updated_at: row.updated_at,
                        folder_id: meta.folder_id,
                    }),
                    _ => index.remove(id),
                }
            }
            _ => index.remove(id),
        }
    }

    pub fn rebuild_index(&self) -> Result<()> {
        let rows = self.store.notes()?;
        let mut fresh = SearchIndex::new();
        for row in rows.iter().filter(|r| !r.deleted) {
            let (Ok(meta), Some(Ok(content))) = (
                codec::open_meta(&self.key, &row.id, &row.meta),
                row.body.as_ref().map(|b| codec::open_body(&self.key, &row.id, b)),
            ) else {
                continue;
            };
            fresh.add(&IndexDocument {
                id: row.id.clone(),
                title: meta.title,
                content,
                updated_at: row.updated_at,
                folder_id: meta.folder_id,
            });
        }
        *self.search_index() = fresh;
        Ok(())
    }

    /// Keep the stored text of `row` as a version. Copied, never re-sealed.
    pub fn capture_version(&self, row: &StoredNote, reason: VersionReason, now: i64) -> Result<()> {
        let Some(body) = &row.body else { return Ok(()) };
        let Some(seal_id) = seal_id_of(body) else { return Ok(()) };
        let held = self.store.versions_for(&row.id)?;
        let has_in_day = held.iter().any(|v| utc_day_start(v.captured_at) == utc_day_start(now));
        let has_in_hour = held.iter().any(|v| utc_hour_start(v.captured_at) == utc_hour_start(now));
        self.store.put_version(&StoredVersion {
            note_id: row.id.clone(),
            seal_id,
            at: row.updated_at,
            captured_at: now,
            expire_at: now + tier_for(reason, has_in_day, has_in_hour),
            reason,
            meta: row.meta.clone(),
            body: body.clone(),
            state: VersionState::Queued,
        })
    }

    fn folders_live(&self) -> Result<Vec<Folder>> {
        let mut out = Vec::new();
        for row in self.store.folders()? {
            if row.deleted {
                continue;
            }
            let Ok(payload) = codec::open_folder(&self.key, &row.id, &row.data) else { continue };
            out.push(Folder {
                id: row.id,
                name: payload.name,
                parent_id: payload.parent_id,
                created_at: row.created_at,
                updated_at: row.updated_at,
            });
        }
        Ok(out)
    }

    /// Write `content` as the note's text, keeping every other meta field.
    fn write_content(&self, row: &Held<'_>, content: &str, now: i64) -> Result<()> {
        let previous = codec::open_meta(&self.key, &row.id, &row.meta)?;
        let meta = codec::meta_for_content(content, &previous);
        self.store.put_note(&StoredNote {
            id: row.id.clone(),
            meta: codec::seal_meta(&self.key, &row.id, &meta),
            body: Some(codec::seal_body(&self.key, &row.id, content)),
            updated_at: now,
            revision: row.revision + 1,
            deleted: false,
        })?;
        self.reindex(&row.id);
        self.queue(&row.id, ChangeKind::Note, false)
    }

    /// A metadata-only write: the body bytes stay exactly as stored, which is
    /// what lets a pin or move that loses a race be rebased instead of forked.
    fn write_meta(&self, row: &Held<'_>, change: impl FnOnce(&mut NoteMetaPayload)) -> Result<()> {
        let mut meta = codec::open_meta(&self.key, &row.id, &row.meta)?;
        change(&mut meta);
        self.store.put_note(&StoredNote {
            meta: codec::seal_meta(&self.key, &row.id, &meta),
            updated_at: now_ms(),
            revision: row.revision + 1,
            ..StoredNote::clone(row)
        })?;
        self.search_index().set_folder(&row.id, meta.folder_id.clone());
        self.queue(&row.id, ChangeKind::Note, true)
    }

    fn tombstone(&self, row: &Held<'_>, at: i64) -> Result<()> {
        self.capture_version(row, VersionReason::Delete, at)?;
        self.store.put_note(&StoredNote {
            body: None,
            deleted: true,
            updated_at: at,
            revision: row.revision + 1,
            ..StoredNote::clone(row)
        })?;
        self.search_index().remove(&row.id);
        self.queue(&row.id, ChangeKind::Note, true)
    }
}

/// An unlocked vault. Cheap to clone; every clone is the same vault.
#[derive(Clone)]
pub struct Vault {
    pub(crate) inner: Arc<VaultInner>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").field("uid", &self.inner.uid).finish_non_exhaustive()
    }
}

impl Vault {
    pub(crate) fn new(
        uid: String,
        email: String,
        key: Key,
        store: Arc<dyn Store>,
        backend: Arc<dyn Backend>,
        share_origin: String,
    ) -> Result<Self> {
        let (events, _) = broadcast::channel(256);
        let (status, _) = watch::channel(SyncStatus::Idle);
        let inner = Arc::new(VaultInner {
            uid,
            email,
            key,
            store,
            backend,
            search: Mutex::new(SearchIndex::new()),
            capture: Mutex::new(CaptureTracker::default()),
            writes: Mutex::new(()),
            events,
            status,
            pending: AtomicUsize::new(0),
            engine: Mutex::new(None),
            share_origin,
        });
        inner.rebuild_index()?;
        inner.store.prune_versions(now_ms())?;
        inner.pending.store(inner.store.queued()?.len(), Ordering::Relaxed);
        Ok(Self { inner })
    }

    /// The account's uid.
    pub fn uid(&self) -> &str {
        &self.inner.uid
    }

    /// The account's email.
    pub fn email(&self) -> &str {
        &self.inner.email
    }

    /// Notes: create, read, edit, pin, move, delete.
    pub fn notes(&self) -> Notes<'_> {
        Notes { vault: self }
    }

    /// Folders.
    pub fn folders(&self) -> Folders<'_> {
        Folders { vault: self }
    }

    /// A note's history.
    pub fn history(&self, note_id: impl Into<String>) -> History<'_> {
        History { vault: self, note_id: note_id.into() }
    }

    /// Share links.
    pub fn shares(&self) -> Shares<'_> {
        Shares { vault: self }
    }

    /// Search with the app's query grammar: words, `#tag`, `in:Folder`,
    /// `"exact phrase"`, and `-` to exclude any of them.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let parsed = core_search::parse_query(query);
        let folders = self.inner.folders_live()?;
        let resolve = |paths: &[String]| -> Option<HashSet<Option<String>>> {
            if paths.is_empty() {
                return None;
            }
            let mut out = HashSet::new();
            for path in paths {
                if path == "/" {
                    out.insert(None);
                    continue;
                }
                for id in folders::folders_matching(&folders, path, true) {
                    out.extend(folders::subtree_ids(&folders, &id).into_iter().map(Some));
                }
            }
            Some(out)
        };
        let options = SearchOptions {
            limit: Some(if parsed.phrases.is_empty() && parsed.not_phrases.is_empty() {
                limit
            } else {
                core_search::VERIFY_MAX
            }),
            folder_ids: resolve(&parsed.folders),
            not_folder_ids: resolve(&parsed.not_folders),
        };
        let mut hits = self.inner.search_index().search_parsed(&parsed, &options);
        if !parsed.phrases.is_empty() || !parsed.not_phrases.is_empty() {
            let key = &self.inner.key;
            let store = &self.inner.store;
            hits = core_search::verify_phrases(&hits, &parsed, limit, |id| {
                let row = store.note(id).ok()??;
                codec::open_body(key, id, row.body.as_ref()?).ok()
            });
        }
        let mut out = Vec::with_capacity(hits.len());
        for hit in hits.into_iter().take(limit) {
            let Some(row) = self.inner.store.note(&hit.id)? else { continue };
            if row.deleted {
                continue;
            }
            out.push(SearchResult { note: self.inner.summary_of(&row)?, score: hit.score, in_title: hit.in_title });
        }
        Ok(out)
    }

    /// Every tag in the vault with how many notes carry it.
    pub fn tags(&self) -> Vec<core_search::TagCount> {
        self.inner.search_index().tag_summary()
    }

    /// Start syncing in the background. Requires a tokio runtime. Calling it
    /// again while sync runs does nothing.
    pub fn start_sync(&self, options: SyncOptions) {
        let mut slot = VaultInner::lock(&self.inner.engine);
        if slot.is_none() {
            *slot = Some(Engine::spawn(self.inner.clone(), options));
        }
    }

    /// Stop background sync. Queued changes stay queued.
    pub fn stop_sync(&self) {
        if let Some(handle) = VaultInner::lock(&self.inner.engine).take() {
            handle.stop();
        }
    }

    /// Push and pull now (for example when the app window gains focus), and
    /// wait for the round trip. Runs one cycle inline if sync is not running.
    pub async fn sync_now(&self) -> SyncStatus {
        let handle = VaultInner::lock(&self.inner.engine).as_ref().map(EngineHandle::clone_sender);
        match handle {
            Some(sender) => EngineHandle::sync_now(sender).await.unwrap_or(SyncStatus::Stopped),
            None => Engine::run_once(self.inner.clone(), SyncOptions::default()).await,
        }
    }

    /// Sync events: remote changes, conflict copies, status, limits.
    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.inner.events.subscribe()
    }

    /// The current sync status, as a watchable value.
    pub fn status(&self) -> watch::Receiver<SyncStatus> {
        self.inner.status.subscribe()
    }

    /// Local changes not yet on the server.
    pub fn pending_count(&self) -> usize {
        self.inner.pending.load(Ordering::Relaxed)
    }

    /// The account's storage and push meter.
    pub async fn usage(&self) -> Result<Usage> {
        self.inner.backend.usage().await
    }

    /// Stop sync and drop the key from memory. The local store keeps its
    /// ciphertext; unlock again to read it.
    pub fn lock(self) {
        self.stop_sync();
        self.inner.search_index().clear();
        VaultInner::lock(&self.inner.capture).clear();
    }
}

/// What [`Notes::update_if_unchanged`] (and, as a bool, [`Notes::update`]) did.
/// On `Changed` and `Deleted` the caller still holds text that isn't in the
/// note: dropping the result drops that text.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalUpdate {
    /// The note held the expected text; it holds the new one now.
    Written,
    /// The note held the new text already: nothing written.
    AlreadyCurrent,
    /// The note no longer holds the expected text (an edit pulled from another
    /// device, or a conflict's resolution): nothing written.
    Changed,
    /// The note was deleted (here, or on another device and pulled since):
    /// nothing written.
    Deleted,
}

/// Note operations. See [`Vault::notes`].
pub struct Notes<'a> {
    vault: &'a Vault,
}

impl Notes<'_> {
    fn inner(&self) -> &VaultInner {
        &self.vault.inner
    }

    /// Live notes, pinned first, then most recently edited.
    pub fn list(&self) -> Result<Vec<NoteSummary>> {
        self.list_sorted(NoteSort::default())
    }

    /// Live notes in a chosen order (pins always lead).
    pub fn list_sorted(&self, sort: NoteSort) -> Result<Vec<NoteSummary>> {
        let mut rows: Vec<StoredNote> = self.inner().store.notes()?.into_iter().filter(|r| !r.deleted).collect();
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
        let summaries = rows.iter().map(|r| self.inner().summary_of(r)).collect::<Result<Vec<_>>>()?;
        Ok(order_for_list(&summaries, sort))
    }

    /// Live notes filed directly in `folder_id` (`None` for the top level).
    pub fn in_folder(&self, folder_id: Option<&str>) -> Result<Vec<NoteSummary>> {
        Ok(self.list()?.into_iter().filter(|n| n.folder_id.as_deref() == folder_id).collect())
    }

    /// One note with its text.
    pub fn get(&self, id: &str) -> Result<Note> {
        let row = self.inner().live_note(id)?;
        let content = match &row.body {
            Some(body) => codec::open_body(&self.inner().key, id, body)?,
            None => String::new(),
        };
        Ok(Note { summary: self.inner().summary_of(&row)?, content })
    }

    /// Create a note and return its id.
    pub fn create(&self, content: &str, folder_id: Option<&str>) -> Result<String> {
        let inner = self.inner();
        // (its folder checked as one step with a folder's delete of its notes
        // here; a pulled folder delete isn't covered, as folders aren't)
        let _writing = inner.write_guard();
        if let Some(folder) = folder_id {
            if !inner.live_folder_exists(folder)? {
                return Err(Error::NotFound(format!("folder {folder}")));
            }
        }
        let id = new_id();
        let now = now_ms();
        let base =
            NoteMetaPayload { folder_id: folder_id.map(str::to_string), created_at: Some(now), ..Default::default() };
        let meta = codec::meta_for_content(content, &base);
        inner.store.put_note(&StoredNote {
            id: id.clone(),
            meta: codec::seal_meta(&inner.key, &id, &meta),
            body: Some(codec::seal_body(&inner.key, &id, content)),
            updated_at: now,
            revision: 1,
            deleted: false,
        })?;
        inner.reindex(&id);
        inner.queue(&id, ChangeKind::Note, true)?;
        Ok(id)
    }

    /// Replace a note's text. Returns false when the text is unchanged.
    ///
    /// Call it as often as your editor likes — on a debounce is typical; the
    /// sync engine coalesces pushes, and history keeps one version per editing
    /// session rather than one per call.
    ///
    /// This writes over whatever the note holds now, another device's edit that
    /// a pull brought in since your editor last read the note included. An
    /// editor that can hold unsaved text while such an edit arrives should use
    /// [`Notes::update_if_unchanged`].
    pub fn update(&self, id: &str, content: &str) -> Result<bool> {
        Ok(self.write_text(id, None, content)? == ConditionalUpdate::Written)
    }

    /// Replace a note's text with `content` only while it still holds
    /// `expected`, the text your editor last read or wrote: one step with the
    /// sync, so a pulled edit is never written over unseen. On
    /// [`ConditionalUpdate::Changed`] or [`ConditionalUpdate::Deleted`] nothing
    /// is written; keep the user's text some other way (a new note, say) rather
    /// than retrying blind. After [`crate::SyncEvent::ConflictCopy`], the
    /// editor's text is in the copy already: follow it there (its id, and its
    /// text as `expected`) before the next save.
    ///
    /// A note that never existed is [`Error::NotFound`].
    pub fn update_if_unchanged(&self, id: &str, expected: &str, content: &str) -> Result<ConditionalUpdate> {
        self.write_text(id, Some(expected), content)
    }

    /// [`Notes::update`] and [`Notes::update_if_unchanged`]: the read, the
    /// check and the write under the vault's write guard.
    fn write_text(&self, id: &str, expected: Option<&str>, content: &str) -> Result<ConditionalUpdate> {
        let inner = self.inner();
        let writing = inner.write_guard();
        // (a conditional write says a deletion as its own result, the caller's
        // text still to keep; a plain update says it as before)
        let row = match writing.note(id)? {
            Some(row) if row.deleted && expected.is_some() => return Ok(ConditionalUpdate::Deleted),
            Some(row) if !row.deleted => row,
            _ => return Err(Error::NotFound(format!("note {id}"))),
        };

        let current = row.body.as_ref().map(|b| codec::open_body(&inner.key, id, b)).transpose()?.unwrap_or_default();
        // (what the editor wants is there already, whoever wrote it: nothing to keep elsewhere)
        if current == content {
            return Ok(ConditionalUpdate::AlreadyCurrent);
        }

        if expected.is_some_and(|expected| expected != current) {
            return Ok(ConditionalUpdate::Changed);
        }

        let now = now_ms();
        if VaultInner::lock(&inner.capture).observe(id, now) {
            inner.capture_version(&row, VersionReason::Edit, now)?;
        }
        inner.write_content(&row, content, now)?;

        Ok(ConditionalUpdate::Written)
    }

    /// Pin or unpin. Pinned notes lead every list.
    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        let writing = self.inner().write_guard();
        let row = writing.live_note(id)?;
        let meta = codec::open_meta(&self.inner().key, id, &row.meta)?;
        if meta.pinned_at.is_some() == pinned {
            return Ok(());
        }

        self.inner().write_meta(&row, |m| m.pinned_at = pinned.then(now_ms))
    }

    /// Move to a folder (`None` for the top level).
    pub fn move_to(&self, id: &str, folder_id: Option<&str>) -> Result<()> {
        let inner = self.inner();
        let writing = inner.write_guard();
        let row = writing.live_note(id)?;
        if let Some(folder) = folder_id {
            if !inner.live_folder_exists(folder)? {
                return Err(Error::NotFound(format!("folder {folder}")));
            }
        }
        let meta = codec::open_meta(&inner.key, id, &row.meta)?;
        if meta.folder_id.as_deref() == folder_id {
            return Ok(());
        }
        inner.write_meta(&row, |m| m.folder_id = folder_id.map(str::to_string))
    }

    /// Delete: a tombstone that syncs, with the final text kept in history for
    /// 30 days so [`Notes::undelete`] can bring it back.
    pub fn delete(&self, id: &str) -> Result<()> {
        let writing = self.inner().write_guard();
        let row = writing.live_note(id)?;
        self.inner().tombstone(&row, now_ms())
    }

    /// Deleted notes whose final text this device holds, newest first.
    pub fn recently_deleted(&self) -> Result<Vec<DeletedNote>> {
        let inner = self.inner();
        let mut out = Vec::new();
        for row in inner.store.notes()?.into_iter().filter(|r| r.deleted) {
            if inner.store.versions_for(&row.id)?.is_empty() {
                continue;
            }
            let title = codec::open_meta(&inner.key, &row.id, &row.meta).map(|m| m.title).unwrap_or_default();
            out.push(DeletedNote { id: row.id, title, deleted_at: row.updated_at });
        }
        out.sort_by_key(|d| std::cmp::Reverse(d.deleted_at));
        Ok(out)
    }

    /// Bring a deleted note back from its last version, under its own id.
    /// Looks on this device first, then on the server.
    pub async fn undelete(&self, id: &str) -> Result<()> {
        let inner = self.inner();
        let row = inner.store.note(id)?.ok_or_else(|| Error::NotFound(format!("note {id}")))?;
        if !row.deleted {
            return Ok(());
        }
        let local = inner.store.versions_for(id)?.into_iter().next();
        let (meta, body, at) = match local {
            Some(v) => (v.meta, v.body, v.at),
            None => {
                let remote = inner.backend.list_versions(id).await?;
                let v =
                    remote.into_iter().next().ok_or_else(|| Error::NotFound(format!("no saved text for note {id}")))?;
                (v.meta, v.body, v.at)
            }
        };
        let content = codec::open_body(&inner.key, id, &body)?;
        let old = codec::open_meta(&inner.key, id, &meta).unwrap_or_default();

        // (read again under the guard, after the network: a pull may have
        // brought the note back meanwhile, which is then left as it is, and its
        // folder may have gone)
        let writing = inner.write_guard();
        let Some(row) = writing.note(id)? else { return Err(Error::NotFound(format!("note {id}"))) };
        if !row.deleted {
            return Ok(());
        }

        let folder_id = match old.folder_id {
            Some(f) if inner.live_folder_exists(&f)? => Some(f),
            _ => None,
        };
        let restored = codec::meta_for_content(
            &content,
            &NoteMetaPayload {
                folder_id,
                pinned_at: old.pinned_at,
                created_at: Some(old.created_at.unwrap_or(at)),
                ..Default::default()
            },
        );
        inner.store.put_note(&StoredNote {
            id: id.to_string(),
            meta: codec::seal_meta(&inner.key, id, &restored),
            body: Some(codec::seal_body(&inner.key, id, &content)),
            updated_at: now_ms(),
            revision: row.revision + 1,
            deleted: false,
        })?;
        inner.reindex(id);
        inner.queue(id, ChangeKind::Note, true)
    }
}

/// Folder operations. See [`Vault::folders`].
pub struct Folders<'a> {
    vault: &'a Vault,
}

impl Folders<'_> {
    fn inner(&self) -> &VaultInner {
        &self.vault.inner
    }

    /// Live folders, unordered.
    pub fn list(&self) -> Result<Vec<Folder>> {
        self.inner().folders_live()
    }

    /// The folder tree, siblings by name, every folder reachable.
    pub fn tree(&self) -> Result<Vec<FolderNode>> {
        Ok(folders::build_tree(&self.list()?))
    }

    /// `"Work / DB"` for a folder.
    pub fn path(&self, id: &str) -> Result<String> {
        Ok(folders::folder_path(&self.list()?, id, " / "))
    }

    /// Folders a typed name or path refers to (exact id, then path, then name).
    pub fn find(&self, wanted: &str) -> Result<Vec<String>> {
        Ok(folders::folders_matching(&self.list()?, wanted, false))
    }

    fn save(&self, id: &str, payload: &FolderPayload, created_at: i64, updated_at: i64, deleted: bool) -> Result<()> {
        let inner = self.inner();
        inner.store.put_folder(&StoredFolder {
            id: id.to_string(),
            data: codec::seal_folder(&inner.key, id, payload),
            created_at,
            updated_at,
            deleted,
        })?;
        inner.queue(id, ChangeKind::Folder, true)
    }

    fn get(&self, id: &str) -> Result<(StoredFolder, FolderPayload)> {
        let row = self
            .inner()
            .store
            .folder(id)?
            .filter(|f| !f.deleted)
            .ok_or_else(|| Error::NotFound(format!("folder {id}")))?;
        let payload = codec::open_folder(&self.inner().key, id, &row.data)?;
        Ok((row, payload))
    }

    /// Create a folder and return its id. A blank name becomes "New folder".
    pub fn create(&self, name: &str, parent_id: Option<&str>) -> Result<String> {
        if let Some(parent) = parent_id {
            if !self.inner().live_folder_exists(parent)? {
                return Err(Error::NotFound(format!("folder {parent}")));
            }
        }
        let trimmed = hushjot_core::js::trim(name);
        let name = if trimmed.is_empty() { "New folder" } else { trimmed };
        let id = new_id();
        let now = now_ms();
        self.save(
            &id,
            &FolderPayload { name: name.into(), parent_id: parent_id.map(str::to_string) },
            now,
            now,
            false,
        )?;
        Ok(id)
    }

    /// Rename. A blank name is refused.
    pub fn rename(&self, id: &str, name: &str) -> Result<()> {
        let trimmed = hushjot_core::js::trim(name);
        if trimmed.is_empty() {
            return Err(Error::Invalid("A folder needs a name.".into()));
        }
        let (row, payload) = self.get(id)?;
        if payload.name == trimmed {
            return Ok(());
        }
        self.save(
            id,
            &FolderPayload { name: trimmed.into(), ..payload },
            row.created_at,
            next_stamp(row.updated_at),
            false,
        )
    }

    /// Move under `parent_id` (`None` for the top level), with everything
    /// inside it. Refused when the target is inside the folder itself.
    pub fn move_to(&self, id: &str, parent_id: Option<&str>) -> Result<()> {
        let (row, payload) = self.get(id)?;
        if payload.parent_id.as_deref() == parent_id {
            return Ok(());
        }
        if !folders::can_move_folder(&self.list()?, id, parent_id) {
            return Err(Error::Invalid("A folder cannot move inside itself.".into()));
        }
        self.save(
            id,
            &FolderPayload { parent_id: parent_id.map(str::to_string), ..payload },
            row.created_at,
            next_stamp(row.updated_at),
            false,
        )
    }

    /// What deleting `id` would take with it: (subfolders, notes).
    pub fn contents(&self, id: &str) -> Result<(usize, usize)> {
        let doomed: HashSet<String> = folders::subtree_ids(&self.list()?, id).into_iter().collect();
        if doomed.is_empty() {
            return Ok((0, 0));
        }
        let notes = self
            .vault
            .notes()
            .list()?
            .into_iter()
            .filter(|n| n.folder_id.as_ref().is_some_and(|f| doomed.contains(f)))
            .count();
        Ok((doomed.len() - 1, notes))
    }

    /// Delete a folder with everything inside it — subfolders and notes. Every
    /// note's final text is kept in history first. Warn with
    /// [`Folders::contents`] before calling.
    pub fn delete(&self, id: &str) -> Result<()> {
        let inner = self.inner();
        let doomed: HashSet<String> = folders::subtree_ids(&self.list()?, id).into_iter().collect();
        if doomed.is_empty() {
            return Err(Error::NotFound(format!("folder {id}")));
        }

        let in_doomed = |row: &StoredNote| {
            codec::open_meta(&inner.key, &row.id, &row.meta)
                .ok()
                .and_then(|m| m.folder_id)
                .is_some_and(|f| doomed.contains(&f))
        };
        let at = now_ms();
        // (the notes read, tombstoned and the folders deleted as one step: no
        // note is filed into one of them meanwhile)
        let writing = inner.write_guard();

        for row in writing.notes()?.into_iter().filter(|r| !r.deleted && in_doomed(r)) {
            inner.tombstone(&row, at)?;
        }

        // One clock for the whole subtree: one user action.
        for folder_id in &doomed {
            if let Some(row) = inner.store.folder(folder_id)? {
                inner.store.put_folder(&StoredFolder {
                    deleted: true,
                    updated_at: at.max(row.updated_at + 1),
                    ..row
                })?;
                inner.queue(folder_id, ChangeKind::Folder, true)?;
            }
        }

        Ok(())
    }
}

/// A note's history. See [`Vault::history`].
pub struct History<'a> {
    vault: &'a Vault,
    note_id: String,
}

impl History<'_> {
    fn inner(&self) -> &VaultInner {
        &self.vault.inner
    }

    async fn all(&self) -> Result<Vec<(VersionInfo, Vec<u8>, Vec<u8>)>> {
        let inner = self.inner();
        let now = now_ms();
        let mut out: Vec<(VersionInfo, Vec<u8>, Vec<u8>)> = inner
            .store
            .versions_for(&self.note_id)?
            .into_iter()
            .filter(|v| v.expire_at > now)
            .map(|v| {
                (
                    VersionInfo { id: v.seal_id, at: v.at, reason: v.reason, expire_at: v.expire_at, local: true },
                    v.meta,
                    v.body,
                )
            })
            .collect();
        let remote: Vec<RemoteVersion> = match inner.backend.list_versions(&self.note_id).await {
            Ok(v) => v,
            // History on this device is still worth showing offline.
            Err(e) if e.is_transient() || matches!(e, Error::NotApproved | Error::SessionExpired) => Vec::new(),
            Err(e) => return Err(e),
        };
        for v in remote {
            if out.iter().all(|(info, _, _)| info.id != v.id) {
                out.push((
                    VersionInfo { id: v.id, at: v.at, reason: v.reason, expire_at: v.expire_at, local: false },
                    v.meta,
                    v.body,
                ));
            }
        }
        out.sort_by_key(|v| std::cmp::Reverse(v.0.at));
        Ok(out)
    }

    /// Versions, newest first: this device's and the server's, merged.
    pub async fn list(&self) -> Result<Vec<VersionInfo>> {
        Ok(self.all().await?.into_iter().map(|(info, _, _)| info).collect())
    }

    /// Open one version.
    pub async fn open(&self, version_id: &str) -> Result<NoteVersion> {
        let (info, meta, body) = self
            .all()
            .await?
            .into_iter()
            .find(|(info, _, _)| info.id == version_id)
            .ok_or_else(|| Error::NotFound(format!("version {version_id}")))?;
        let key = &self.inner().key;
        let content = codec::open_body(key, &self.note_id, &body)?;
        let meta = codec::open_meta(key, &self.note_id, &meta).unwrap_or_default();
        Ok(NoteVersion { info, title: meta.title, folder_id: meta.folder_id, content })
    }

    /// Make a version's text the note's text again. The current text is kept
    /// as a version first, so a restore can itself be undone.
    pub async fn restore(&self, version_id: &str) -> Result<()> {
        let version = self.open(version_id).await?;
        let inner = self.inner();
        let writing = inner.write_guard();
        let row = writing.live_note(&self.note_id)?;
        let now = now_ms();
        inner.capture_version(&row, VersionReason::Restore, now)?;
        inner.write_content(&row, &version.content, now)
    }
}

/// Share links. See [`Vault::shares`].
pub struct Shares<'a> {
    vault: &'a Vault,
}

impl Shares<'_> {
    fn inner(&self) -> &VaultInner {
        &self.vault.inner
    }

    async fn publish_ref(&self, note_id: &str, expiry: ShareExpiry, target: ShareRef) -> Result<PublishedShare> {
        let inner = self.inner();
        let note = self.vault.notes().get(note_id)?;
        let key = core_share::import_share_key(&target.key)?;
        let payload = core_share::seal_share(
            &key,
            &SharePayload { title: note.summary.title, body: note.content, updated_at: note.summary.updated_at },
        )?;
        let expire_at = expiry.expire_at(now_ms());
        inner
            .backend
            .publish_share(
                &RemoteShare { id: target.id.clone(), payload, expire_at },
                &ShareIndexEntry {
                    id: target.id.clone(),
                    note_id: note_id.to_string(),
                    key: codec::seal_share_key(&inner.key, &target.id, &target.key),
                    expire_at,
                    created_at: 0,
                },
            )
            .await?;
        Ok(PublishedShare {
            url: core_share::build_share_url(&inner.share_origin, &target),
            id: target.id,
            note_id: note_id.to_string(),
            expire_at,
        })
    }

    /// Publish a snapshot of a note as a link. The link carries a fresh key —
    /// never the vault key — in its fragment, which browsers never send.
    pub async fn publish(&self, note_id: &str, expiry: ShareExpiry) -> Result<PublishedShare> {
        let (_, encoded) = core_share::new_share_key();
        self.publish_ref(note_id, expiry, ShareRef { id: core_share::new_share_id(), key: encoded }).await
    }

    /// Re-publish an existing link with the note's current text; the URL stays
    /// the same.
    pub async fn update(&self, share_id: &str, expiry: ShareExpiry) -> Result<PublishedShare> {
        let entry = self
            .inner()
            .backend
            .list_shares()
            .await?
            .into_iter()
            .find(|s| s.id == share_id)
            .ok_or_else(|| Error::NotFound(format!("share {share_id}")))?;
        let key = codec::open_share_key(&self.inner().key, &entry.id, &entry.key)?;
        self.publish_ref(&entry.note_id, expiry, ShareRef { id: entry.id, key }).await
    }

    /// Withdraw a link.
    pub async fn revoke(&self, share_id: &str) -> Result<()> {
        self.inner().backend.revoke_share(share_id).await
    }

    /// Every unexpired link, newest first, with its URL rebuilt.
    pub async fn list(&self) -> Result<Vec<PublishedShare>> {
        let inner = self.inner();
        let mut out = Vec::new();
        for entry in inner.backend.list_shares().await? {
            let Ok(key) = codec::open_share_key(&inner.key, &entry.id, &entry.key) else { continue };
            out.push(PublishedShare {
                url: core_share::build_share_url(&inner.share_origin, &ShareRef { id: entry.id.clone(), key }),
                id: entry.id,
                note_id: entry.note_id,
                expire_at: entry.expire_at,
            });
        }
        Ok(out)
    }
}
