//! An in-memory [`Store`], for tests and for apps that keep nothing on disk.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use super::{ChangeKind, QueuedChange, Store, StoredFolder, StoredNote, StoredVersion, VersionState};
use crate::error::Result;

#[derive(Default)]
struct Inner {
    notes: BTreeMap<String, StoredNote>,
    folders: BTreeMap<String, StoredFolder>,
    queue: HashMap<String, (ChangeKind, i64)>,
    next_token: i64,
    kv: BTreeMap<String, String>,
    versions: BTreeMap<(String, String), StoredVersion>,
}

/// A [`Store`] that lives and dies with the process.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> Result<T> {
        let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(f(&mut inner))
    }
}

impl Store for MemoryStore {
    fn note(&self, id: &str) -> Result<Option<StoredNote>> {
        self.with(|i| i.notes.get(id).cloned())
    }

    fn put_note(&self, note: &StoredNote) -> Result<()> {
        self.with(|i| {
            i.notes.insert(note.id.clone(), note.clone());
        })
    }

    fn notes(&self) -> Result<Vec<StoredNote>> {
        self.with(|i| i.notes.values().cloned().collect())
    }

    fn folder(&self, id: &str) -> Result<Option<StoredFolder>> {
        self.with(|i| i.folders.get(id).cloned())
    }

    fn put_folder(&self, folder: &StoredFolder) -> Result<()> {
        self.with(|i| {
            i.folders.insert(folder.id.clone(), folder.clone());
        })
    }

    fn folders(&self) -> Result<Vec<StoredFolder>> {
        self.with(|i| i.folders.values().cloned().collect())
    }

    fn enqueue(&self, id: &str, kind: ChangeKind) -> Result<()> {
        self.with(|i| {
            i.next_token += 1;
            let token = i.next_token;
            i.queue.insert(id.to_string(), (kind, token));
        })
    }

    fn dequeue(&self, id: &str, token: Option<i64>) -> Result<()> {
        self.with(|i| {
            if token.is_none() || i.queue.get(id).map(|(_, t)| *t) == token {
                i.queue.remove(id);
            }
        })
    }

    fn queued(&self) -> Result<Vec<QueuedChange>> {
        self.with(|i| {
            let mut out: Vec<QueuedChange> = i
                .queue
                .iter()
                .map(|(id, (kind, token))| QueuedChange { id: id.clone(), kind: *kind, token: *token })
                .collect();
            out.sort_by_key(|q| q.token);
            out
        })
    }

    fn is_queued(&self, id: &str) -> Result<bool> {
        self.with(|i| i.queue.contains_key(id))
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        self.with(|i| i.kv.get(key).cloned())
    }

    fn set(&self, key: &str, value: Option<&str>) -> Result<()> {
        self.with(|i| match value {
            Some(v) => {
                i.kv.insert(key.to_string(), v.to_string());
            }
            None => {
                i.kv.remove(key);
            }
        })
    }

    fn clear_prefix(&self, prefix: &str) -> Result<()> {
        self.with(|i| i.kv.retain(|k, _| !k.starts_with(prefix)))
    }

    fn put_version(&self, version: &StoredVersion) -> Result<()> {
        self.with(|i| {
            i.versions.entry((version.note_id.clone(), version.seal_id.clone())).or_insert_with(|| version.clone());
        })
    }

    fn versions_for(&self, note_id: &str) -> Result<Vec<StoredVersion>> {
        self.with(|i| {
            let mut out: Vec<StoredVersion> = i.versions.values().filter(|v| v.note_id == note_id).cloned().collect();
            out.sort_by_key(|v| std::cmp::Reverse(v.captured_at));
            out
        })
    }

    fn pending_versions(&self, limit: usize) -> Result<Vec<StoredVersion>> {
        self.with(|i| {
            let mut out: Vec<StoredVersion> =
                i.versions.values().filter(|v| v.state == VersionState::Queued).cloned().collect();
            out.sort_by_key(|v| v.captured_at);
            out.truncate(limit);
            out
        })
    }

    fn set_version_state(&self, note_id: &str, seal_id: &str, state: VersionState) -> Result<()> {
        self.with(|i| {
            if let Some(v) = i.versions.get_mut(&(note_id.to_string(), seal_id.to_string())) {
                v.state = state;
            }
        })
    }

    fn prune_versions(&self, now: i64) -> Result<()> {
        self.with(|i| i.versions.retain(|_, v| v.expire_at > now))
    }

    fn wipe(&self) -> Result<()> {
        self.with(|i| *i = Inner::default())
    }
}

#[cfg(test)]
pub(crate) mod conformance {
    //! Behaviour every [`Store`] must have, run against each implementation.
    use super::*;
    use hushjot_core::history::VersionReason;

    fn note(id: &str, rev: i64) -> StoredNote {
        StoredNote {
            id: id.into(),
            meta: vec![1, 2],
            body: Some(vec![3]),
            updated_at: 5,
            revision: rev,
            deleted: false,
        }
    }

    fn version(note: &str, seal: &str, captured: i64, state: VersionState) -> StoredVersion {
        StoredVersion {
            note_id: note.into(),
            seal_id: seal.into(),
            at: 1,
            captured_at: captured,
            expire_at: captured + 100,
            reason: VersionReason::Edit,
            meta: vec![1],
            body: vec![2],
            state,
        }
    }

    pub fn run(store: &dyn Store) {
        // Rows replace and read back, tombstones included.
        store.put_note(&note("a", 1)).unwrap();
        store.put_note(&StoredNote { body: None, deleted: true, ..note("a", 2) }).unwrap();
        assert_eq!(store.note("a").unwrap().unwrap().revision, 2);
        assert_eq!(store.note("a").unwrap().unwrap().body, None);
        assert_eq!(store.notes().unwrap().len(), 1);
        assert!(store.note("missing").unwrap().is_none());

        let folder = StoredFolder { id: "f".into(), data: vec![9], created_at: 1, updated_at: 2, deleted: false };
        store.put_folder(&folder).unwrap();
        assert_eq!(store.folder("f").unwrap(), Some(folder));
        assert_eq!(store.folders().unwrap().len(), 1);

        // The outbox: oldest first, tokens guard against a stale dequeue.
        store.enqueue("a", ChangeKind::Note).unwrap();
        store.enqueue("f", ChangeKind::Folder).unwrap();
        let queue = store.queued().unwrap();
        assert_eq!(queue.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), ["a", "f"]);
        assert_eq!(queue[1].kind, ChangeKind::Folder);
        let stale = queue[0].token;
        store.enqueue("a", ChangeKind::Note).unwrap();
        store.dequeue("a", Some(stale)).unwrap();
        assert!(store.is_queued("a").unwrap(), "a newer enqueue survives a stale dequeue");
        store.dequeue("a", None).unwrap();
        assert!(!store.is_queued("a").unwrap());

        // Key-value.
        store.set("sync:note:a", Some("{}")).unwrap();
        store.set("app:owner", Some("u")).unwrap();
        assert_eq!(store.get("sync:note:a").unwrap().as_deref(), Some("{}"));
        store.clear_prefix("sync:").unwrap();
        assert_eq!(store.get("sync:note:a").unwrap(), None);
        assert_eq!(store.get("app:owner").unwrap().as_deref(), Some("u"));
        store.set("app:owner", None).unwrap();
        assert_eq!(store.get("app:owner").unwrap(), None);

        // Versions: first write wins, newest first per note, pending oldest first.
        store.put_version(&version("a", "s1", 10, VersionState::Queued)).unwrap();
        store.put_version(&version("a", "s1", 99, VersionState::Pushed)).unwrap();
        store.put_version(&version("a", "s2", 20, VersionState::Queued)).unwrap();
        store.put_version(&version("b", "s3", 5, VersionState::Local)).unwrap();
        let held = store.versions_for("a").unwrap();
        assert_eq!(held.iter().map(|v| v.seal_id.as_str()).collect::<Vec<_>>(), ["s2", "s1"]);
        assert_eq!(held[1].captured_at, 10);
        assert_eq!(
            store.pending_versions(10).unwrap().iter().map(|v| v.seal_id.as_str()).collect::<Vec<_>>(),
            ["s1", "s2"]
        );
        assert_eq!(store.pending_versions(1).unwrap().len(), 1);
        store.set_version_state("a", "s1", VersionState::Pushed).unwrap();
        assert_eq!(store.pending_versions(10).unwrap().len(), 1);
        store.prune_versions(112).unwrap();
        assert_eq!(store.versions_for("a").unwrap().len(), 1);
        assert!(store.versions_for("b").unwrap().is_empty());

        store.wipe().unwrap();
        assert!(store.notes().unwrap().is_empty());
        assert!(store.queued().unwrap().is_empty());
    }

    #[test]
    fn memory_store_conforms() {
        run(&MemoryStore::new());
    }
}
