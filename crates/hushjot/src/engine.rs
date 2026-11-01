//! The sync engine (a port of the app's `src/core/sync/engine.ts`; its header
//! is the long-form reasoning for everything here).
//!
//! ```text
//! push   queued local changes ──► server   (revision compare-and-swap may refuse)
//! pull   changes since cursor ──► local     (higher server revision wins)
//! ```
//!
//! ## Two revision counters
//!
//! A note's local `revision` advances on every local write; the server's
//! advances by exactly one per accepted write. They are different numbers.
//! The engine records the server's (`base`, in `sync:note:<id>`) and every push
//! sends `base + 1`. Pushing the local counter instead would be refused on
//! every multi-edit push, read as a conflict, and fork the note forever.
//!
//! ## Conflicts
//!
//! A refused push means another device moved the note first. The text this
//! device typed is never discarded: the server's version keeps the id and the
//! local text becomes a separate "(conflict copy …)" note. Two exceptions,
//! both proofs rather than guesses — a lost acknowledgement (the server holds
//! exactly what we sent) is adopted, and a metadata-only change (the stored
//! body is still the one the server last agreed to, by nonce) is rebased onto
//! the server's version without a copy. Two consecutive conflicts park the
//! note so one bad push cannot mint copies forever.
//!
//! ## Cadence
//!
//! A text edit syncs 800 ms after the last change, or at most 3 s into
//! continuous editing; a structural change (create, delete, move, pin, any
//! folder change) on a 100 ms nudge. With nothing queued, a 60 s backstop pulls
//! anyway — over REST there is no listener, so this is how remote changes
//! arrive; call [`crate::Vault::sync_now`] when your window gains focus for
//! immediacy. Consecutive failures back off exponentially to five minutes.
//!
//! ## Limits and pauses
//!
//! Storage full and the daily push limit hold changes on the device — queued
//! and editable — rather than failing them, and report through
//! [`SyncEvent`]. A maintenance pause stops all requests and re-probes every
//! fifteen minutes; nothing about it signs anyone out.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep_until, Instant};

use hushjot_core::history::{seal_id_of, VersionReason};
use hushjot_core::meta::NoteMetaPayload;
use hushjot_core::notes::{conflict_copy_title, excerpt_of};
use hushjot_core::usage::meter_delta;

use crate::backend::{Cursor, PauseVerdict, RemoteFolder, RemoteNote, RemoteVersion};
use crate::codec;
use crate::error::{Error, Result};
use crate::store::{ChangeKind, QueuedChange, StoredFolder, StoredNote, VersionState};
use crate::vault::{now_ms, VaultInner};

const PULL_PAGE: usize = 200;
const PUSH_PAGE: usize = 200;
const VERSION_PAGE: usize = 20;
const VERSION_PAGE_BYTES: usize = 4 * 1024 * 1024;
const CONFLICT_LIMIT: u32 = 2;
const BACKOFF_CEILING: Duration = Duration::from_secs(5 * 60);
const STORAGE_RETRY_MS: i64 = 10 * 60_000;
const PUSH_LIMIT_RETRY_MS: i64 = 15 * 60_000;
const PAUSE_PROBE: Duration = Duration::from_secs(15 * 60);
const LARGE_NOTE_BYTES: i64 = 64 * 1024;
const MAX_NOTE_BYTES: i64 = 900 * 1024;
const LARGE_NOTE_STRETCH: f64 = 10.0;
const CURSOR_KEY: &str = "sync:cursor";

/// Sync timings. The defaults are the app's; change them only with reason.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Quiet time after a text edit before pushing.
    pub idle: Duration,
    /// The most a burst of continuous editing waits.
    pub checkpoint: Duration,
    /// Delay for a structural change.
    pub nudge: Duration,
    /// How often an idle device pulls.
    pub backstop: Duration,
    /// Upload history versions.
    pub upload_history: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            idle: Duration::from_millis(800),
            checkpoint: Duration::from_secs(3),
            nudge: Duration::from_millis(100),
            backstop: Duration::from_secs(60),
            upload_history: true,
        }
    }
}

/// Where sync stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    /// Everything is on the server.
    Idle,
    /// A cycle is running, or more is queued behind a full page.
    Syncing,
    /// The network or service is unreachable; work is queued.
    Offline,
    /// Something is stuck (a parked note); see the events for why.
    Error,
    /// The session expired; sign in again.
    SignedOut,
    /// The account has no plan to sync.
    NotApproved,
    /// The service is paused for maintenance.
    Paused,
    /// Sync is not running.
    Stopped,
}

/// Things an editor will want to react to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyncEvent {
    /// The status changed.
    Status(SyncStatus),
    /// Remote changes (or a conflict resolution) rewrote these locally:
    /// reload them, and reload an open editor only if the user has not typed
    /// since.
    Changed {
        /// Notes changed underneath you.
        notes: Vec<String>,
        /// Folders changed underneath you.
        folders: Vec<String>,
    },
    /// A conflict was preserved: `note_id` now holds the other device's text
    /// and `copy_id` holds this device's.
    ConflictCopy {
        /// The original note.
        note_id: String,
        /// The new copy holding the local text.
        copy_id: String,
    },
    /// Local changes not yet on the server.
    Pending(usize),
    /// Changes are waiting because storage is full; 0 once they move.
    StorageFull {
        /// How many.
        waiting: usize,
    },
    /// Changes are waiting for the daily push window to roll over.
    PushLimit {
        /// How many.
        waiting: usize,
        /// When sync will try again (epoch ms).
        retry_at: Option<i64>,
    },
    /// A note is over the sealed size limit and cannot sync until it shrinks.
    NoteTooLarge {
        /// The note.
        note_id: String,
    },
    /// The session expired; sign in again. Notes stay editable.
    SessionExpired,
    /// The account has no plan. Notes stay editable and queued.
    NotApproved,
    /// The service is paused for maintenance. Notes stay editable and queued.
    Paused,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NoteSyncState {
    base: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seen: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body_seal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_bytes: Option<i64>,
}

impl NoteSyncState {
    fn with_body_seal(mut self, body: Option<&[u8]>) -> Self {
        if let Some(seal) = body.and_then(seal_id_of) {
            self.body_seal = Some(seal);
        }
        self
    }
}

/// The checkpoint ceiling for a note of `bytes` stored size: big notes wait
/// longer, because every push re-uploads the whole note.
pub fn ceiling_for(bytes: i64, base: Duration) -> Duration {
    if bytes <= LARGE_NOTE_BYTES {
        return base;
    }
    let span = (bytes.min(MAX_NOTE_BYTES) - LARGE_NOTE_BYTES) as f64 / (MAX_NOTE_BYTES - LARGE_NOTE_BYTES) as f64;
    base.mul_f64(1.0 + span * (LARGE_NOTE_STRETCH - 1.0))
}

pub(crate) enum Command {
    Queued { id: String, immediate: bool },
    SyncNow(oneshot::Sender<SyncStatus>),
    Stop,
}

/// A running engine.
pub(crate) struct EngineHandle {
    tx: mpsc::UnboundedSender<Command>,
}

impl EngineHandle {
    pub fn queued(&self, id: &str, immediate: bool) {
        let _ = self.tx.send(Command::Queued { id: id.to_string(), immediate });
    }

    pub fn stop(self) {
        let _ = self.tx.send(Command::Stop);
    }

    pub fn clone_sender(&self) -> mpsc::UnboundedSender<Command> {
        self.tx.clone()
    }

    pub async fn sync_now(tx: mpsc::UnboundedSender<Command>) -> Option<SyncStatus> {
        let (reply, rx) = oneshot::channel();
        tx.send(Command::SyncNow(reply)).ok()?;
        rx.await.ok()
    }
}

pub(crate) struct Engine {
    vault: Arc<VaultInner>,
    options: SyncOptions,
    status: SyncStatus,
    pending: usize,
    retryable: bool,
    conflict_streak: HashMap<String, u32>,
    parked: HashSet<String>,
    pushed_bytes: HashMap<String, i64>,
    storage_parked: HashSet<String>,
    storage_full: bool,
    storage_held_at: i64,
    limit_parked: HashSet<String>,
    push_limited_until: i64,
    failure_streak: u32,
    session_dead: bool,
    paused: bool,
    idle: Option<Instant>,
    ceiling: Option<Instant>,
    nudge: Option<Instant>,
    backstop: Option<Instant>,
    changed_notes: HashSet<String>,
    changed_folders: HashSet<String>,
    /// Set when this cycle re-queued work it made itself (a rebased meta or a
    /// conflict copy), which is owed a push straight away.
    requeued: bool,
}

fn is_cycle_fatal(error: &Error) -> bool {
    matches!(error, Error::SessionExpired | Error::NotApproved | Error::Paused | Error::Transient(_))
}

impl Engine {
    fn new(vault: Arc<VaultInner>, options: SyncOptions) -> Self {
        Self {
            vault,
            options,
            status: SyncStatus::Idle,
            pending: 0,
            retryable: false,
            conflict_streak: HashMap::new(),
            parked: HashSet::new(),
            pushed_bytes: HashMap::new(),
            storage_parked: HashSet::new(),
            storage_full: false,
            storage_held_at: 0,
            limit_parked: HashSet::new(),
            push_limited_until: 0,
            failure_streak: 0,
            session_dead: false,
            paused: false,
            idle: None,
            ceiling: None,
            nudge: None,
            backstop: None,
            changed_notes: HashSet::new(),
            changed_folders: HashSet::new(),
            requeued: false,
        }
    }

    pub fn spawn(vault: Arc<VaultInner>, options: SyncOptions) -> EngineHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        let engine = Self::new(vault, options);
        tokio::spawn(engine.run(rx));
        let handle = EngineHandle { tx };
        // The first cycle runs straight away: pull what the account holds,
        // push what was queued while sync was off.
        let _ = handle.tx.send(Command::Queued { id: String::new(), immediate: true });
        handle
    }

    /// One cycle without a background task.
    pub async fn run_once(vault: Arc<VaultInner>, options: SyncOptions) -> SyncStatus {
        let mut engine = Self::new(vault, options);
        engine.cycle().await;
        engine.status
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Command>) {
        loop {
            let deadline = [self.idle, self.ceiling, self.nudge, self.backstop].into_iter().flatten().min();
            tokio::select! {
                command = rx.recv() => match command {
                    None | Some(Command::Stop) => break,
                    Some(Command::Queued { id, immediate }) => {
                        // A fresh edit earns a parked note one more attempt.
                        self.parked.remove(&id);
                        self.pending = self.vault.store.queued().map_or(self.pending, |q| q.len());
                        self.publish_pending();
                        if immediate { self.schedule_nudge() } else { self.schedule_checkpoint(Some(&id)) }
                    }
                    Some(Command::SyncNow(reply)) => {
                        self.cycle().await;
                        let _ = reply.send(self.status);
                    }
                },
                () = async { sleep_until(deadline.expect("guarded")).await }, if deadline.is_some() => {
                    self.cycle().await;
                }
            }
        }
        self.set_status(SyncStatus::Stopped);
    }

    /// One sync, plus the immediate follow-ups owed: the rest of a paged
    /// push, or work the cycle re-queued itself. Bounded, because a note that
    /// keeps losing is parked by the conflict breaker rather than retried here.
    async fn cycle(&mut self) {
        for _ in 0..64 {
            let owed = self.run_sync().await;
            if !owed || self.session_dead {
                break;
            }
        }
    }

    // --- status and events ---------------------------------------------------

    fn emit(&self, event: SyncEvent) {
        let _ = self.vault.events.send(event);
    }

    fn set_status(&mut self, next: SyncStatus) {
        if self.status != next {
            self.status = next;
            self.vault.status.send_replace(next);
            self.emit(SyncEvent::Status(next));
        }
    }

    fn publish_pending(&self) {
        self.vault.pending.store(self.pending, Ordering::Relaxed);
        self.emit(SyncEvent::Pending(self.pending));
    }

    fn refresh_pending(&mut self) {
        if let Ok(queue) = self.vault.store.queued() {
            self.pending = queue.len();
            self.retryable = queue.iter().any(|q| !self.parked.contains(&q.id));
        }
        self.publish_pending();
    }

    // --- timers --------------------------------------------------------------

    fn retry_delay(&self) -> Duration {
        if self.failure_streak == 0 {
            return self.options.idle;
        }
        self.options.idle.saturating_mul(1 << self.failure_streak.min(12)).min(BACKOFF_CEILING)
    }

    fn schedule_checkpoint(&mut self, note: Option<&str>) {
        if self.session_dead {
            return;
        }
        let now = Instant::now();
        self.idle = Some(now + self.retry_delay());
        if self.ceiling.is_none() {
            let bytes = note.and_then(|id| self.pushed_bytes.get(id)).copied().unwrap_or(0);
            self.ceiling = Some(now + ceiling_for(bytes, self.options.checkpoint).max(self.retry_delay()));
        }
    }

    fn schedule_nudge(&mut self) {
        if self.session_dead {
            return;
        }
        if self.failure_streak > 0 {
            self.schedule_checkpoint(None);
            return;
        }
        if self.nudge.is_none() {
            self.nudge = Some(Instant::now() + self.options.nudge);
        }
    }

    fn schedule_backstop(&mut self) {
        let delay = if self.paused { PAUSE_PROBE } else { self.options.backstop.max(self.retry_delay()) };
        if self.session_dead && !self.paused {
            return;
        }
        self.backstop = Some(Instant::now() + delay);
    }

    fn clear_timers(&mut self) {
        self.idle = None;
        self.ceiling = None;
        self.nudge = None;
        self.backstop = None;
    }

    // --- limits --------------------------------------------------------------

    fn hold_for_space(&mut self, id: &str) {
        self.storage_full = true;
        self.storage_held_at = now_ms();
        self.parked.insert(id.to_string());
        if self.storage_parked.insert(id.to_string()) {
            self.emit(SyncEvent::StorageFull { waiting: self.storage_parked.len() });
        }
    }

    fn space_freed(&mut self) {
        self.storage_full = false;
        if self.storage_parked.is_empty() {
            return;
        }
        for id in self.storage_parked.drain() {
            self.parked.remove(&id);
        }
        self.emit(SyncEvent::StorageFull { waiting: 0 });
    }

    fn sent(&mut self, id: &str) {
        if self.storage_parked.remove(id) {
            self.emit(SyncEvent::StorageFull { waiting: self.storage_parked.len() });
        }
        if self.limit_parked.remove(id) {
            let retry_at = (!self.limit_parked.is_empty()).then_some(self.push_limited_until);
            self.emit(SyncEvent::PushLimit { waiting: self.limit_parked.len(), retry_at });
        }
    }

    fn push_limited(&self) -> bool {
        now_ms() < self.push_limited_until
    }

    fn limit_reached(&mut self, retry_at: Option<i64>) {
        self.push_limited_until = retry_at.unwrap_or(0).max(now_ms() + PUSH_LIMIT_RETRY_MS);
        if !self.limit_parked.is_empty() {
            self.emit(SyncEvent::PushLimit {
                waiting: self.limit_parked.len(),
                retry_at: Some(self.push_limited_until),
            });
        }
    }

    fn hold_for_limit(&mut self, id: &str) {
        self.parked.insert(id.to_string());
        if self.limit_parked.insert(id.to_string()) {
            self.emit(SyncEvent::PushLimit {
                waiting: self.limit_parked.len(),
                retry_at: Some(self.push_limited_until),
            });
        }
    }

    fn retry_held_for_limit(&mut self) {
        if self.limit_parked.is_empty() || self.push_limited() {
            return;
        }
        for id in &self.limit_parked {
            self.parked.remove(id);
        }
    }

    fn retry_held_for_space(&mut self) {
        if self.storage_parked.is_empty() || now_ms() - self.storage_held_at < STORAGE_RETRY_MS {
            return;
        }
        for id in &self.storage_parked {
            self.parked.remove(id);
        }
    }

    // --- bookkeeping -----------------------------------------------------------

    fn load_state(&self, id: &str) -> Result<Option<NoteSyncState>> {
        Ok(self.vault.store.get(&format!("sync:note:{id}"))?.and_then(|s| serde_json::from_str(&s).ok()))
    }

    fn save_state(&self, id: &str, state: &NoteSyncState) -> Result<()> {
        let json = serde_json::to_string(state).expect("sync state serialises");
        self.vault.store.set(&format!("sync:note:{id}"), Some(&json))
    }

    fn folder_bytes(&self, id: &str) -> Result<Option<i64>> {
        Ok(self.vault.store.get(&format!("sync:folder:{id}"))?.and_then(|s| s.parse().ok()))
    }

    fn save_folder_bytes(&self, id: &str, bytes: i64) -> Result<()> {
        self.vault.store.set(&format!("sync:folder:{id}"), Some(&bytes.to_string()))
    }

    fn load_cursor(&self) -> Result<Cursor> {
        Ok(self.vault.store.get(CURSOR_KEY)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
    }

    fn save_cursor(&self, cursor: &Cursor) -> Result<()> {
        self.vault.store.set(CURSOR_KEY, Some(&serde_json::to_string(cursor).expect("cursor serialises")))
    }

    fn mark_note_changed(&mut self, id: &str) {
        self.vault.reindex(id);
        self.changed_notes.insert(id.to_string());
    }

    // --- the cycle -------------------------------------------------------------

    /// Returns true when a full page was pushed and another cycle is owed.
    async fn run_sync(&mut self) -> bool {
        if self.paused {
            match self.vault.backend.probe_pause().await {
                PauseVerdict::Paused | PauseVerdict::Unknown => {
                    self.clear_timers();
                    self.schedule_backstop();
                    return false;
                }
                PauseVerdict::Running => {
                    self.paused = false;
                    self.session_dead = false;
                }
            }
        }
        if self.session_dead {
            return false;
        }

        self.clear_timers();
        self.set_status(SyncStatus::Syncing);
        self.storage_full = false;
        self.requeued = false;
        self.changed_notes.clear();
        self.changed_folders.clear();
        self.retry_held_for_space();
        self.retry_held_for_limit();

        let result = self.push_and_pull().await;
        self.refresh_pending();
        let mut deferred = false;

        match result {
            Ok(was_deferred) => {
                deferred = was_deferred;
                self.failure_streak = 0;
                let next = if deferred {
                    SyncStatus::Syncing
                } else if self.pending > 0 {
                    SyncStatus::Error
                } else {
                    SyncStatus::Idle
                };
                self.set_status(next);
            }
            Err(error) => {
                self.failure_streak += 1;
                match error {
                    Error::NotApproved => match self.vault.backend.probe_pause().await {
                        PauseVerdict::Paused => self.enter_pause(),
                        PauseVerdict::Unknown => self.set_status(SyncStatus::Offline),
                        PauseVerdict::Running => {
                            self.session_dead = true;
                            self.clear_timers();
                            self.set_status(SyncStatus::NotApproved);
                            self.emit(SyncEvent::NotApproved);
                        }
                    },
                    Error::Paused => self.enter_pause(),
                    Error::SessionExpired => {
                        self.session_dead = true;
                        self.clear_timers();
                        self.set_status(SyncStatus::SignedOut);
                        self.emit(SyncEvent::SessionExpired);
                    }
                    Error::Transient(_) => self.set_status(SyncStatus::Offline),
                    _ => self.set_status(SyncStatus::Error),
                }
            }
        }

        if !self.changed_notes.is_empty() || !self.changed_folders.is_empty() {
            let mut notes: Vec<String> = self.changed_notes.drain().collect();
            let mut folders: Vec<String> = self.changed_folders.drain().collect();
            notes.sort();
            folders.sort();
            self.emit(SyncEvent::Changed { notes, folders });
        }

        if !self.session_dead || self.paused {
            if self.retryable && !self.paused {
                self.schedule_checkpoint(None);
            } else {
                self.schedule_backstop();
            }
        }
        deferred || (self.requeued && !self.session_dead)
    }

    fn enter_pause(&mut self) {
        self.paused = true;
        self.session_dead = true;
        self.clear_timers();
        self.set_status(SyncStatus::Paused);
        self.emit(SyncEvent::Paused);
    }

    async fn push_and_pull(&mut self) -> Result<bool> {
        let deferred = self.push().await?;
        if !deferred && self.options.upload_history {
            self.push_versions(None).await?;
        }
        self.pull().await?;
        Ok(deferred)
    }

    // --- push ------------------------------------------------------------------

    async fn push(&mut self) -> Result<bool> {
        let queue = self.vault.store.queued()?;
        self.pending = queue.len();
        self.publish_pending();
        if queue.is_empty() {
            return Ok(false);
        }
        let deferred = queue.len() > PUSH_PAGE;
        let mut first_failure = None;
        for entry in queue.into_iter().take(PUSH_PAGE) {
            if let Err(error) = self.push_one(&entry).await {
                if is_cycle_fatal(&error) {
                    return Err(error);
                }
                // About this record, not the connection: park it, carry on.
                self.parked.insert(entry.id.clone());
                first_failure.get_or_insert(error);
            }
            self.pending = self.pending.saturating_sub(1);
            self.publish_pending();
        }
        match first_failure {
            Some(error) => Err(error),
            None => Ok(deferred),
        }
    }

    async fn push_one(&mut self, entry: &QueuedChange) -> Result<()> {
        if self.parked.contains(&entry.id) {
            return Ok(());
        }
        if self.push_limited() {
            self.hold_for_limit(&entry.id);
            return Ok(());
        }
        match entry.kind {
            ChangeKind::Folder => self.push_folder(entry).await,
            ChangeKind::Note => self.push_note(entry).await,
        }
    }

    async fn push_folder(&mut self, entry: &QueuedChange) -> Result<()> {
        let id = &entry.id;
        let Some(raw) = self.vault.store.folder(id)? else {
            return self.vault.store.dequeue(id, None);
        };
        let stored = self.folder_bytes(id)?.unwrap_or(0);
        let size = raw.data.len() as i64;
        if self.storage_full && meter_delta(size, stored) > 0 {
            self.hold_for_space(id);
            return Ok(());
        }
        let remote = RemoteFolder {
            id: raw.id.clone(),
            data: raw.data.clone(),
            created_at: raw.created_at,
            updated_at: raw.updated_at,
            deleted: raw.deleted,
            synced_at: 0,
        };
        match self.vault.backend.push_folder(&remote, stored).await {
            Ok(()) => {}
            Err(Error::QuotaExceeded { .. }) => {
                self.hold_for_space(id);
                return Ok(());
            }
            Err(Error::PushLimit { retry_at }) => {
                self.limit_reached(retry_at);
                self.hold_for_limit(id);
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        self.save_folder_bytes(id, size)?;
        self.sent(id);
        if meter_delta(size, stored) < 0 {
            self.space_freed();
        }
        self.vault.store.dequeue(id, Some(entry.token))
    }

    async fn push_note(&mut self, entry: &QueuedChange) -> Result<()> {
        let id = &entry.id;
        let Some(raw) = self.vault.store.note(id)? else {
            return self.vault.store.dequeue(id, None);
        };
        let state = self.load_state(id)?;

        // A newer server revision was seen while this note had unpushed edits:
        // pushing now would overwrite it. It is a conflict; say so.
        if let Some(s) = &state {
            if s.seen.is_some_and(|seen| seen > s.base) {
                return self.resolve_conflict(entry, &raw, s.clone()).await;
            }
        }

        // Never assume a revision: one read, once per note, to learn the base.
        let mut state = match state {
            Some(s) => s,
            None => {
                let remote = self.vault.backend.fetch_note(id).await?;
                let s = NoteSyncState {
                    base: remote.as_ref().map_or(0, |r| r.revision),
                    base_bytes: Some(remote.as_ref().map_or(0, RemoteNote::stored_bytes)),
                    ..NoteSyncState::default()
                };
                self.save_state(id, &s)?;
                s
            }
        };
        if state.base_bytes.is_none() {
            let bytes = if state.base == 0 {
                0
            } else {
                let remote = self.vault.backend.fetch_note(id).await?;
                let bytes = remote.as_ref().map_or(0, RemoteNote::stored_bytes);
                if remote.is_some_and(|r| r.revision == state.base) {
                    state.base_bytes = Some(bytes);
                    self.save_state(id, &state)?;
                }
                bytes
            };
            state.base_bytes = Some(bytes);
        }
        let stored = state.base_bytes.unwrap_or(0);
        let next = state.base + 1;
        let size = hushjot_core::usage::counted_note_bytes(Some(&raw.meta), raw.body.as_deref());
        if self.storage_full && meter_delta(size, stored) > 0 {
            self.hold_for_space(id);
            return Ok(());
        }

        // A delete's versions land before the delete, so a device that hears
        // of the tombstone can already fetch what the note said.
        if raw.deleted && self.options.upload_history {
            self.push_versions(Some(id)).await?;
        }

        let remote = RemoteNote {
            id: id.clone(),
            meta: raw.meta.clone(),
            body: raw.body.clone(),
            revision: next,
            updated_at: raw.updated_at,
            deleted: raw.deleted,
            synced_at: 0,
        };
        match self.vault.backend.push_note(&remote, stored).await {
            Ok(()) => {
                self.pushed_bytes.insert(id.clone(), size);
                self.save_state(
                    id,
                    &NoteSyncState { base: next, base_bytes: Some(size), ..NoteSyncState::default() }
                        .with_body_seal(raw.body.as_deref()),
                )?;
                self.conflict_streak.remove(id);
                self.sent(id);
                if meter_delta(size, stored) < 0 {
                    self.space_freed();
                }
                // Conditional: an edit made while this push was in flight
                // re-queued the note, and that entry has to survive.
                self.vault.store.dequeue(id, Some(entry.token))
            }
            Err(Error::RevisionConflict { .. }) => self.resolve_conflict(entry, &raw, state).await,
            Err(Error::QuotaExceeded { .. }) => {
                self.hold_for_space(id);
                Ok(())
            }
            Err(Error::PushLimit { retry_at }) => {
                self.limit_reached(retry_at);
                self.hold_for_limit(id);
                Ok(())
            }
            Err(error @ Error::PayloadTooLarge { .. }) => {
                // Parked, never dropped: the queue entry is what tells a pull
                // this note holds text it has not managed to send.
                self.parked.insert(id.clone());
                self.emit(SyncEvent::NoteTooLarge { note_id: id.clone() });
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    /// Preserve both sides (§16 of the app's spec).
    async fn resolve_conflict(&mut self, entry: &QueuedChange, raw: &StoredNote, state: NoteSyncState) -> Result<()> {
        let id = &entry.id;
        let remote = self.vault.backend.fetch_note(id).await?;
        // (from here on one step with any local write of the note; no await
        // follows, so the guard never crosses one)
        let vault = Arc::clone(&self.vault);
        let _writing = vault.write_guard();

        // Refused but not moved: not a conflict. Park rather than copy forever.
        let Some(remote) = remote.filter(|r| r.revision != state.base) else {
            self.parked.insert(id.clone());
            return Ok(());
        };

        // Our own write, acknowledged by the server but not by the network.
        // The app compares `updatedAt` alone; this also requires the stored
        // bytes to be ours, which no other device can produce (every seal draws
        // a fresh nonce). Two devices writing in the same millisecond would
        // otherwise each take the other's write for their own and drop their
        // own change without a copy.
        if remote.revision == state.base + 1
            && remote.updated_at == raw.updated_at
            && remote.meta == raw.meta
            && remote.body == raw.body
        {
            self.save_state(
                id,
                &NoteSyncState {
                    base: remote.revision,
                    base_bytes: Some(remote.stored_bytes()),
                    ..NoteSyncState::default()
                }
                .with_body_seal(raw.body.as_deref()),
            )?;
            self.conflict_streak.remove(id);
            return self.vault.store.dequeue(id, Some(entry.token));
        }

        let streak = self.conflict_streak.entry(id.clone()).or_insert(0);
        *streak += 1;
        if *streak >= CONFLICT_LIMIT {
            self.parked.insert(id.clone());
            return Ok(());
        }

        // Only metadata changed here: adopt theirs, lay our pin and folder over it.
        let body_unchanged = match (&raw.body, &state.body_seal) {
            (Some(body), Some(seal)) => seal_id_of(body).as_deref() == Some(seal.as_str()),
            _ => false,
        };
        if !raw.deleted && !remote.deleted && body_unchanged {
            return self.adopt_with_local_meta(entry, raw, &remote);
        }

        let copy_id = self.create_conflict_copy(id, &remote);
        let local_revision = self.vault.store.note(id)?.map_or(0, |n| n.revision);
        self.vault.store.put_note(&StoredNote {
            id: id.clone(),
            meta: remote.meta.clone(),
            body: remote.body.clone(),
            updated_at: remote.updated_at,
            revision: local_revision + 1,
            deleted: remote.deleted,
        })?;
        self.save_state(
            id,
            &NoteSyncState {
                base: remote.revision,
                base_bytes: Some(remote.stored_bytes()),
                ..NoteSyncState::default()
            }
            .with_body_seal(remote.body.as_deref()),
        )?;
        self.mark_note_changed(id);
        self.vault.store.dequeue(id, Some(entry.token))?;
        if let Some(copy_id) = copy_id {
            self.vault.store.enqueue(&copy_id, ChangeKind::Note)?;
            self.requeued = true;
            self.mark_note_changed(&copy_id);
            self.emit(SyncEvent::ConflictCopy { note_id: id.clone(), copy_id });
        }
        Ok(())
    }

    /// The local text as a new note, unless the two sides say the same thing.
    fn create_conflict_copy(&self, id: &str, remote: &RemoteNote) -> Option<String> {
        let key = &self.vault.key;
        let local = self.vault.store.note(id).ok()??;
        let content = codec::open_body(key, id, local.body.as_ref()?).ok()?;
        if content.is_empty() {
            return None;
        }
        if let Some(body) = &remote.body {
            if codec::open_body(key, id, body).is_ok_and(|theirs| theirs == content) {
                return None;
            }
        }
        let folder_id = codec::open_meta(key, id, &local.meta).ok().and_then(|m| m.folder_id);
        let now = now_ms();
        let copy_id = uuid::Uuid::new_v4().to_string();
        let excerpt = excerpt_of(&content);
        let meta = NoteMetaPayload {
            title: conflict_copy_title(&content, now),
            folder_id,
            pinned_at: None,
            excerpt: (!excerpt.is_empty()).then_some(excerpt),
            created_at: Some(now),
        };
        self.vault
            .store
            .put_note(&StoredNote {
                id: copy_id.clone(),
                meta: codec::seal_meta(key, &copy_id, &meta),
                body: Some(codec::seal_body(key, &copy_id, &content)),
                updated_at: now,
                revision: 1,
                deleted: false,
            })
            .ok()?;
        Some(copy_id)
    }

    fn rebase_meta(&self, id: &str, local: &[u8], remote: &[u8]) -> Option<Vec<u8>> {
        let key = &self.vault.key;
        let mine = codec::open_meta(key, id, local).ok()?;
        let theirs = codec::open_meta(key, id, remote).ok()?;
        let folder_id = match &mine.folder_id {
            None => None,
            Some(f) if self.vault.live_folder_exists(f).unwrap_or(false) => Some(f.clone()),
            Some(_) => theirs.folder_id.clone(),
        };
        if folder_id == theirs.folder_id && mine.pinned_at == theirs.pinned_at {
            return None;
        }
        Some(codec::seal_meta(key, id, &NoteMetaPayload { folder_id, pinned_at: mine.pinned_at, ..theirs }))
    }

    fn adopt_with_local_meta(&mut self, entry: &QueuedChange, raw: &StoredNote, remote: &RemoteNote) -> Result<()> {
        let id = &entry.id;
        let merged = self.rebase_meta(id, &raw.meta, &remote.meta);
        let keep_local = merged.is_some();
        self.vault.store.put_note(&StoredNote {
            id: id.clone(),
            meta: merged.unwrap_or_else(|| remote.meta.clone()),
            body: remote.body.clone(),
            // A fresh stamp for a merged meta, so its push cannot be mistaken
            // for the remote write it was merged onto.
            updated_at: if keep_local { now_ms().max(remote.updated_at + 1) } else { remote.updated_at },
            revision: raw.revision + 1,
            deleted: false,
        })?;
        self.save_state(
            id,
            &NoteSyncState {
                base: remote.revision,
                base_bytes: Some(remote.stored_bytes()),
                ..NoteSyncState::default()
            }
            .with_body_seal(remote.body.as_deref()),
        )?;
        self.mark_note_changed(id);
        if keep_local {
            self.requeued = true;
            return self.vault.store.enqueue(id, ChangeKind::Note);
        }
        self.conflict_streak.remove(id);
        self.vault.store.dequeue(id, Some(entry.token))
    }

    /// Upload a page of queued versions (or all of one note's), letting only
    /// the server's own verdicts affect the cycle.
    async fn push_versions(&mut self, note: Option<&str>) -> Result<()> {
        if self.push_limited() {
            return Ok(());
        }
        let store = &self.vault.store;
        let candidates = match note {
            Some(id) => store.versions_for(id)?.into_iter().filter(|v| v.state == VersionState::Queued).collect(),
            None => store.pending_versions(VERSION_PAGE)?,
        };
        let mut bytes = 0;
        let page: Vec<_> = candidates
            .into_iter()
            .take_while(|v| {
                bytes += v.bytes();
                bytes <= VERSION_PAGE_BYTES
            })
            .collect();
        if page.is_empty() {
            return Ok(());
        }
        let remote: Vec<RemoteVersion> = page
            .iter()
            .map(|v| RemoteVersion {
                note_id: v.note_id.clone(),
                id: v.seal_id.clone(),
                meta: v.meta.clone(),
                body: v.body.clone(),
                at: v.at,
                reason: v.reason,
                expire_at: v.expire_at,
                created_at: None,
            })
            .collect();
        let refused = match self.vault.backend.push_versions(&remote).await {
            Ok(refused) => refused,
            Err(Error::PushLimit { retry_at }) => {
                self.limit_reached(retry_at);
                return Ok(());
            }
            Err(e) if is_cycle_fatal(&e) => return Err(e),
            Err(_) => return Ok(()),
        };
        let refused: HashSet<(String, String)> = refused.into_iter().map(|v| (v.note_id, v.id)).collect();
        for v in page {
            let state = if refused.contains(&(v.note_id.clone(), v.seal_id.clone())) {
                VersionState::Local
            } else {
                VersionState::Pushed
            };
            store.set_version_state(&v.note_id, &v.seal_id, state)?;
        }
        Ok(())
    }

    // --- pull ------------------------------------------------------------------

    async fn pull(&mut self) -> Result<()> {
        let mut cursor = self.load_cursor()?;
        loop {
            let page = self.vault.backend.pull(&cursor, PULL_PAGE).await?;
            if page.notes.is_empty() && page.folders.is_empty() {
                if page.cursor != cursor {
                    self.save_cursor(&page.cursor)?;
                }
                break;
            }
            for note in &page.notes {
                self.apply_remote_note(note)?;
            }
            for folder in &page.folders {
                self.apply_remote_folder(folder)?;
            }
            let advanced = page.cursor != cursor;
            cursor = page.cursor;
            self.save_cursor(&cursor)?;
            if !page.has_more || !advanced {
                break;
            }
        }
        Ok(())
    }

    fn apply_remote_note(&mut self, remote: &RemoteNote) -> Result<()> {
        // (one step with any local write of the note: an edit's read and write
        // never straddle this)
        let vault = Arc::clone(&self.vault);
        let _writing = vault.write_guard();

        let state = self.load_state(&remote.id)?;
        // At or below what we recorded: our own write echoing back.
        if state.as_ref().is_some_and(|s| remote.revision <= s.base) {
            return Ok(());
        }
        // Local edits not pushed yet: let the push decide, and record the
        // sighting so a note with no base cannot bootstrap over this change.
        if self.vault.store.is_queued(&remote.id)? {
            let mut s = state.unwrap_or_default();
            s.seen = Some(remote.revision);
            return self.save_state(&remote.id, &s);
        }
        let local = self.vault.store.note(&remote.id)?;
        if state.is_none() && local.as_ref().is_some_and(|l| l.revision >= remote.revision) {
            return Ok(());
        }
        // Deleted elsewhere: keep this device's text in history first.
        if remote.deleted {
            if let Some(l) = local.as_ref().filter(|l| !l.deleted && l.body.is_some()) {
                let _ = self.vault.capture_version(l, VersionReason::Delete, now_ms());
            }
        }
        self.vault.store.put_note(&StoredNote {
            id: remote.id.clone(),
            meta: remote.meta.clone(),
            body: remote.body.clone(),
            updated_at: remote.updated_at,
            revision: local.map_or(0, |l| l.revision) + 1,
            deleted: remote.deleted,
        })?;
        self.save_state(
            &remote.id,
            &NoteSyncState {
                base: remote.revision,
                base_bytes: Some(remote.stored_bytes()),
                ..NoteSyncState::default()
            }
            .with_body_seal(remote.body.as_deref()),
        )?;
        self.mark_note_changed(&remote.id);
        Ok(())
    }

    fn apply_remote_folder(&mut self, folder: &RemoteFolder) -> Result<()> {
        self.save_folder_bytes(&folder.id, folder.data.len() as i64)?;
        let row = StoredFolder {
            id: folder.id.clone(),
            data: folder.data.clone(),
            created_at: folder.created_at,
            updated_at: folder.updated_at,
            deleted: folder.deleted,
        };
        let write = match self.vault.store.folder(&folder.id)? {
            None => true,
            // A tombstone wins outright, and a local deletion stands.
            Some(local) if folder.deleted => !local.deleted,
            Some(local) if local.deleted => false,
            Some(_) if self.vault.store.is_queued(&folder.id)? => false,
            // Strictly newer, so our own write echoing back is a no-op.
            Some(local) => folder.updated_at > local.updated_at,
        };
        if write {
            self.vault.store.put_folder(&row)?;
            self.changed_folders.insert(folder.id.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_notes_wait_longer_up_to_ten_times() {
        let base = Duration::from_secs(3);
        assert_eq!(ceiling_for(1_000, base), base);
        assert_eq!(ceiling_for(LARGE_NOTE_BYTES, base), base);
        assert_eq!(ceiling_for(MAX_NOTE_BYTES, base), Duration::from_secs(30));
        assert_eq!(ceiling_for(10 * MAX_NOTE_BYTES, base), Duration::from_secs(30));
    }

    #[test]
    fn sync_state_uses_the_apps_field_names() {
        let state = NoteSyncState { base: 3, seen: None, body_seal: Some("ab".into()), base_bytes: Some(10) };
        assert_eq!(serde_json::to_string(&state).unwrap(), r#"{"base":3,"bodySeal":"ab","baseBytes":10}"#);
    }
}
