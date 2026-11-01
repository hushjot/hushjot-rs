//! Test support: an in-memory HushJot server that enforces the rules the real
//! one does, and helpers to stand up devices against it.
//!
//! The fake is deliberately strict where `firestore.rules` is strict — the
//! revision compare-and-swap, the 900 KiB limit, metering with a quota and a
//! push window, create-only versions, no deletes — so an engine bug that the
//! real server would refuse fails here too. It also records every byte it
//! stores, so tests can assert no plaintext ever arrives.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use hushjot::backend::{
    Backend, Cursor, KeyRecordDoc, PauseVerdict, Position, PullPage, RemoteFolder, RemoteNote, RemoteShare,
    RemoteVersion, ShareIndexEntry,
};
use hushjot::core::envelope::Key;
use hushjot::core::kdf::Argon2Params;
use hushjot::core::key_record::{create_key_record_for_tests, KeyRecord};
use hushjot::core::usage::{
    charged_bytes, meter_delta, Usage, BASE_STORAGE_BYTES, DAILY_PUSH_LIMIT, MAX_PAYLOAD_BYTES, PUSH_WINDOW_MS,
};
use hushjot::{Error, HushJot, MemorySecrets, MemoryStore, Result, UnlockOptions, Vault};

pub const PASSPHRASE: &str = "correct horse battery staple";
pub const RECOVERY: &str = "K7Q4-9XMV-2ZWR-8DHC-6BTN-4YFJ-3GKP-1SAE";

#[derive(Default)]
pub struct State {
    pub notes: BTreeMap<String, RemoteNote>,
    pub folders: BTreeMap<String, RemoteFolder>,
    pub versions: BTreeMap<(String, String), RemoteVersion>,
    pub shares: BTreeMap<String, RemoteShare>,
    pub share_index: BTreeMap<String, ShareIndexEntry>,
    pub key_record: Option<KeyRecordDoc>,
    pub bytes: i64,
    pub quota: i64,
    pub pn: i64,
    pub ws: Option<i64>,
    pub hist: i64,
    pub shares_this_month: i64,
    pub commits: usize,
    /// Every byte string the server was ever handed.
    pub received: Vec<Vec<u8>>,
}

/// How long a test's pause waits to be let go before it fails the test.
pub const PAUSE_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The fake server, shared by every device in a test.
pub struct FakeServer {
    pub state: Mutex<State>,
    clock: AtomicI64,
    pub approved: AtomicBool,
    pub paused: AtomicBool,
    pub offline: AtomicBool,
    pub session_expired: AtomicBool,
    /// Accept the next note write but report a transient failure (a lost ack).
    pub drop_next_ack: AtomicBool,
    pub dek: Key,
}

impl FakeServer {
    pub fn new() -> Arc<Self> {
        let dek = Key::generate();
        let params = Argon2Params { memory_kib: 64, iterations: 1, ..Argon2Params::default() };
        let record = create_key_record_for_tests(&dek, PASSPHRASE, RECOVERY, &params, 1_790_035_200_000);
        Arc::new(Self {
            state: Mutex::new(State {
                key_record: Some(KeyRecordDoc::Record(Box::new(record))),
                quota: BASE_STORAGE_BYTES,
                ..State::default()
            }),
            clock: AtomicI64::new(chrono_now()),
            approved: AtomicBool::new(true),
            paused: AtomicBool::new(false),
            offline: AtomicBool::new(false),
            session_expired: AtomicBool::new(false),
            drop_next_ack: AtomicBool::new(false),
            dek,
        })
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn record(&self) -> KeyRecord {
        match self.state().key_record.clone() {
            Some(KeyRecordDoc::Record(r)) => *r,
            _ => panic!("no record"),
        }
    }

    /// Server time: strictly increasing, like `request.time` across commits.
    fn tick(&self) -> i64 {
        self.clock.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn gate_read(&self) -> Result<()> {
        if self.offline.load(Ordering::SeqCst) {
            return Err(Error::Transient("offline".into()));
        }
        if self.session_expired.load(Ordering::SeqCst) {
            return Err(Error::SessionExpired);
        }
        if self.paused.load(Ordering::SeqCst) {
            return Err(Error::Paused);
        }
        if !self.approved.load(Ordering::SeqCst) {
            return Err(Error::NotApproved);
        }
        Ok(())
    }

    /// The meter check every metered commit makes. `Ok(false)` is a refusal.
    fn meter(&self, s: &mut State, delta: i64, reset: bool) -> bool {
        let now = self.clock.load(Ordering::SeqCst);
        if delta > 0 && s.bytes + delta > s.quota {
            return false;
        }
        if reset {
            if s.ws.is_some_and(|ws| now <= ws + PUSH_WINDOW_MS) {
                return false;
            }
            s.ws = Some(now);
            s.pn = 1;
        } else {
            if s.pn + 1 > DAILY_PUSH_LIMIT {
                return false;
            }
            s.pn += 1;
        }
        s.bytes += delta;
        s.commits += 1;
        true
    }

    pub fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    pub fn advance_clock(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    /// Everything stored, flattened, for plaintext audits.
    pub fn all_stored_bytes(&self) -> Vec<Vec<u8>> {
        self.state().received.clone()
    }
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn note_size(n: &RemoteNote) -> i64 {
    n.stored_bytes()
}

#[async_trait]
impl Backend for FakeServer {
    async fn uid(&self) -> String {
        "uid-1".into()
    }

    async fn key_record(&self) -> Result<KeyRecordDoc> {
        self.gate_read()?;
        Ok(self.state().key_record.clone().unwrap_or(KeyRecordDoc::Missing))
    }

    async fn fetch_note(&self, id: &str) -> Result<Option<RemoteNote>> {
        self.gate_read()?;
        Ok(self.state().notes.get(id).cloned())
    }

    async fn push_note(&self, note: &RemoteNote, stored_bytes: i64) -> Result<()> {
        self.gate_read()?;
        let size = note_size(note);
        if size as usize > MAX_PAYLOAD_BYTES {
            return Err(Error::PayloadTooLarge { note_id: note.id.clone(), bytes: size as usize });
        }
        let now = self.tick();
        let mut s = self.state();
        let existing = s.notes.get(&note.id).cloned();
        let actual = existing.as_ref().map_or(0, note_size);
        let cas_ok = match &existing {
            None => note.revision > 0,
            Some(old) => note.revision == old.revision + 1,
        };
        // The real backend retries once with the size the server really holds
        // when its own record was stale (`settle_refusal`), so the fake meters
        // against what is stored and ignores `stored_bytes`.
        let _ = stored_bytes;
        let attempt = |s: &mut State, reset: bool| cas_ok && self.meter(s, meter_delta(size, actual), reset);
        let accepted = attempt(&mut s, false) || (cas_ok && s.pn >= DAILY_PUSH_LIMIT && attempt(&mut s, true));
        if accepted {
            s.received.push(note.meta.clone());
            if let Some(body) = &note.body {
                s.received.push(body.clone());
            }
            s.notes.insert(note.id.clone(), RemoteNote { synced_at: now, ..note.clone() });
            drop(s);
            if self.drop_next_ack.swap(false, Ordering::SeqCst) {
                return Err(Error::Transient("ack lost".into()));
            }
            return Ok(());
        }
        // Explain the refusal the way FirestoreBackend does.
        if !cas_ok {
            return Err(Error::RevisionConflict { note_id: note.id.clone() });
        }
        let delta = meter_delta(size, actual);
        if delta > 0 && s.bytes + delta > s.quota {
            return Err(Error::QuotaExceeded { delta, used: Some(s.bytes), quota: Some(s.quota) });
        }
        if s.pn >= DAILY_PUSH_LIMIT {
            return Err(Error::PushLimit { retry_at: s.ws.map(|w| w + PUSH_WINDOW_MS) });
        }
        Err(Error::RevisionConflict { note_id: note.id.clone() })
    }

    async fn push_folder(&self, folder: &RemoteFolder, _stored_bytes: i64) -> Result<()> {
        self.gate_read()?;
        let now = self.tick();
        let mut s = self.state();
        let actual = s.folders.get(&folder.id).map_or(0, |f| f.data.len() as i64);
        let delta = meter_delta(folder.data.len() as i64, actual);
        if !self.meter(&mut s, delta, false) && !(s.pn >= DAILY_PUSH_LIMIT && self.meter(&mut s, delta, true)) {
            if delta > 0 && s.bytes + delta > s.quota {
                return Err(Error::QuotaExceeded { delta, used: Some(s.bytes), quota: Some(s.quota) });
            }
            return Err(Error::PushLimit { retry_at: s.ws.map(|w| w + PUSH_WINDOW_MS) });
        }
        s.received.push(folder.data.clone());
        s.folders.insert(folder.id.clone(), RemoteFolder { synced_at: now, ..folder.clone() });
        Ok(())
    }

    async fn pull(&self, cursor: &Cursor, limit: usize) -> Result<PullPage> {
        self.gate_read()?;
        let s = self.state();
        fn page<T: Clone>(items: Vec<(i64, String, T)>, from: &Position, limit: usize) -> (Vec<T>, Position, bool) {
            let mut sorted: Vec<_> =
                items.into_iter().filter(|(at, id, _)| (*at, id.as_str()) > (from.at, from.id.as_str())).collect();
            sorted.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
            sorted.truncate(limit);
            let full = sorted.len() == limit;
            let next = sorted.last().map_or(from.clone(), |(at, id, _)| Position { at: *at, id: id.clone() });
            (sorted.into_iter().map(|(_, _, t)| t).collect(), next, full)
        }
        let (notes, n_pos, n_full) =
            page(s.notes.values().map(|n| (n.synced_at, n.id.clone(), n.clone())).collect(), &cursor.notes, limit);
        let (folders, f_pos, f_full) =
            page(s.folders.values().map(|f| (f.synced_at, f.id.clone(), f.clone())).collect(), &cursor.folders, limit);
        Ok(PullPage { notes, folders, cursor: Cursor { notes: n_pos, folders: f_pos }, has_more: n_full || f_full })
    }

    async fn push_versions(&self, versions: &[RemoteVersion]) -> Result<Vec<RemoteVersion>> {
        self.gate_read()?;
        let mut s = self.state();
        for v in versions {
            let key = (v.note_id.clone(), v.id.clone());
            if s.versions.contains_key(&key) {
                continue; // create-only; already there counts as written
            }
            if v.expire_at > chrono_now() + 31 * 86_400_000 {
                panic!("version expiry beyond the 31-day rule");
            }
            if !self.meter(&mut s, 0, false) {
                return Err(Error::PushLimit { retry_at: s.ws.map(|w| w + PUSH_WINDOW_MS) });
            }
            s.hist += charged_bytes((v.meta.len() + v.body.len()) as i64);
            s.received.push(v.meta.clone());
            s.received.push(v.body.clone());
            s.versions.insert(key, RemoteVersion { created_at: Some(self.clock.load(Ordering::SeqCst)), ..v.clone() });
        }
        Ok(Vec::new())
    }

    async fn list_versions(&self, note_id: &str) -> Result<Vec<RemoteVersion>> {
        self.gate_read()?;
        let mut out: Vec<RemoteVersion> =
            self.state().versions.values().filter(|v| v.note_id == note_id).cloned().collect();
        out.sort_by_key(|v| std::cmp::Reverse(v.created_at));
        Ok(out)
    }

    async fn publish_share(&self, share: &RemoteShare, index: &ShareIndexEntry) -> Result<()> {
        self.gate_read()?;
        let now = self.tick();
        let mut s = self.state();
        if s.shares_this_month >= 100 {
            return Err(Error::ShareLimit);
        }
        if !self.meter(&mut s, 0, false) {
            return Err(Error::PushLimit { retry_at: None });
        }
        s.shares_this_month += 1;
        s.received.push(share.payload.clone());
        s.received.push(index.key.clone());
        s.shares.insert(share.id.clone(), share.clone());
        s.share_index.insert(index.id.clone(), ShareIndexEntry { created_at: now, ..index.clone() });
        Ok(())
    }

    async fn revoke_share(&self, id: &str) -> Result<()> {
        self.gate_read()?;
        let mut s = self.state();
        s.shares.remove(id);
        s.share_index.remove(id);
        Ok(())
    }

    async fn list_shares(&self) -> Result<Vec<ShareIndexEntry>> {
        self.gate_read()?;
        let mut out: Vec<_> = self.state().share_index.values().cloned().collect();
        out.sort_by_key(|v| std::cmp::Reverse(v.created_at));
        Ok(out)
    }

    async fn usage(&self) -> Result<Usage> {
        self.gate_read()?;
        let s = self.state();
        Ok(Usage {
            bytes: s.bytes,
            quota: s.quota,
            history_this_month: s.hist,
            shares_this_month: s.shares_this_month,
            window_start: s.ws,
            pushes_in_window: s.pn,
        })
    }

    async fn probe_pause(&self) -> PauseVerdict {
        if self.offline.load(Ordering::SeqCst) {
            PauseVerdict::Unknown
        } else if self.paused.load(Ordering::SeqCst) {
            PauseVerdict::Paused
        } else {
            PauseVerdict::Running
        }
    }
}

/// A device: its own store and secrets, unlocked against the shared server.
pub struct Device {
    pub hj: HushJot,
    pub vault: Vault,
}

pub async fn device(server: &Arc<FakeServer>) -> Device {
    device_with_store(server, Arc::new(MemoryStore::new())).await
}

pub async fn device_with_store(server: &Arc<FakeServer>, store: Arc<dyn hushjot::Store>) -> Device {
    let hj = HushJot::builder().store_arc(store).secrets(MemorySecrets::new()).build().unwrap();
    let account = hj.account_with_backend("me@example.com", server.clone()).await;
    let vault = account.unlock(PASSPHRASE, UnlockOptions::default()).await.unwrap();
    Device { hj, vault }
}

/// Whether any byte string the server holds contains `needle`.
pub fn server_saw(server: &FakeServer, needle: &str) -> bool {
    server.all_stored_bytes().iter().any(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
}
