//! A durable [`Store`] in one SQLite file.
//!
//! One connection behind a mutex: the store's work is small row reads and
//! writes, and serialising them is simpler and safer than a pool. The file
//! holds ciphertext only (see the module notes on [`super`]), but it is still
//! the user's data — put it in the app's private data directory.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use hushjot_core::history::VersionReason;

use super::{ChangeKind, QueuedChange, Store, StoredFolder, StoredNote, StoredVersion, VersionState};
use crate::error::{Error, Result};

const SCHEMA_VERSION: i32 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS notes (
  id TEXT PRIMARY KEY, meta BLOB NOT NULL, body BLOB, updated_at INTEGER NOT NULL,
  revision INTEGER NOT NULL, deleted INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS folders (
  id TEXT PRIMARY KEY, data BLOB NOT NULL, created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL, deleted INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS queue (id TEXT PRIMARY KEY, kind TEXT NOT NULL, token INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS versions (
  note_id TEXT NOT NULL, seal_id TEXT NOT NULL, at INTEGER NOT NULL, captured_at INTEGER NOT NULL,
  expire_at INTEGER NOT NULL, reason TEXT NOT NULL, meta BLOB NOT NULL, body BLOB NOT NULL,
  state TEXT NOT NULL, PRIMARY KEY (note_id, seal_id)
);
CREATE INDEX IF NOT EXISTS versions_state ON versions (state, captured_at);
";

/// A [`Store`] backed by a SQLite file.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

fn store_err(e: rusqlite::Error) -> Error {
    Error::Store(e.to_string())
}

impl SqliteStore {
    /// Open (or create) the store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(Connection::open(path).map_err(store_err)?)
    }

    /// An in-memory SQLite store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().map_err(store_err)?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL").map_err(store_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(store_err)?;
        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).map_err(store_err)?;
        if version > SCHEMA_VERSION {
            return Err(Error::Store(format!("store schema {version} is newer than this SDK understands")));
        }
        conn.execute_batch(SCHEMA).map_err(store_err)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(store_err)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&conn).map_err(store_err)
    }
}

fn read_note(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredNote> {
    Ok(StoredNote {
        id: row.get(0)?,
        meta: row.get(1)?,
        body: row.get(2)?,
        updated_at: row.get(3)?,
        revision: row.get(4)?,
        deleted: row.get::<_, i64>(5)? != 0,
    })
}

fn read_folder(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredFolder> {
    Ok(StoredFolder {
        id: row.get(0)?,
        data: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        deleted: row.get::<_, i64>(4)? != 0,
    })
}

fn read_version(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredVersion> {
    let reason: String = row.get(5)?;
    let state: String = row.get(8)?;
    Ok(StoredVersion {
        note_id: row.get(0)?,
        seal_id: row.get(1)?,
        at: row.get(2)?,
        captured_at: row.get(3)?,
        expire_at: row.get(4)?,
        reason: VersionReason::parse(&reason).unwrap_or(VersionReason::Edit),
        meta: row.get(6)?,
        body: row.get(7)?,
        state: VersionState::parse(&state),
    })
}

const VERSION_COLUMNS: &str = "note_id, seal_id, at, captured_at, expire_at, reason, meta, body, state";

impl Store for SqliteStore {
    fn note(&self, id: &str) -> Result<Option<StoredNote>> {
        self.with(|c| {
            c.query_row(
                "SELECT id, meta, body, updated_at, revision, deleted FROM notes WHERE id = ?1",
                [id],
                read_note,
            )
            .optional()
        })
    }

    fn put_note(&self, n: &StoredNote) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO notes (id, meta, body, updated_at, revision, deleted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![n.id, n.meta, n.body, n.updated_at, n.revision, n.deleted as i64],
            )
            .map(drop)
        })
    }

    fn notes(&self) -> Result<Vec<StoredNote>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT id, meta, body, updated_at, revision, deleted FROM notes")?;
            let rows = stmt.query_map([], read_note)?;
            rows.collect()
        })
    }

    fn folder(&self, id: &str) -> Result<Option<StoredFolder>> {
        self.with(|c| {
            c.query_row(
                "SELECT id, data, created_at, updated_at, deleted FROM folders WHERE id = ?1",
                [id],
                read_folder,
            )
            .optional()
        })
    }

    fn put_folder(&self, f: &StoredFolder) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO folders (id, data, created_at, updated_at, deleted) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![f.id, f.data, f.created_at, f.updated_at, f.deleted as i64],
            )
            .map(drop)
        })
    }

    fn folders(&self) -> Result<Vec<StoredFolder>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT id, data, created_at, updated_at, deleted FROM folders")?;
            let rows = stmt.query_map([], read_folder)?;
            rows.collect()
        })
    }

    fn enqueue(&self, id: &str, kind: ChangeKind) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO queue (id, kind, token) VALUES (?1, ?2, (SELECT COALESCE(MAX(token), 0) + 1 FROM queue))",
                params![id, kind.as_str()],
            )
            .map(drop)
        })
    }

    fn dequeue(&self, id: &str, token: Option<i64>) -> Result<()> {
        self.with(|c| {
            match token {
                Some(t) => c.execute("DELETE FROM queue WHERE id = ?1 AND token = ?2", params![id, t]),
                None => c.execute("DELETE FROM queue WHERE id = ?1", [id]),
            }
            .map(drop)
        })
    }

    fn queued(&self) -> Result<Vec<QueuedChange>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT id, kind, token FROM queue ORDER BY token")?;
            let rows = stmt.query_map([], |r| {
                let kind: String = r.get(1)?;
                Ok(QueuedChange { id: r.get(0)?, kind: ChangeKind::parse(&kind), token: r.get(2)? })
            })?;
            rows.collect()
        })
    }

    fn is_queued(&self, id: &str) -> Result<bool> {
        self.with(|c| {
            c.query_row("SELECT 1 FROM queue WHERE id = ?1", [id], |_| Ok(())).optional().map(|r| r.is_some())
        })
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        self.with(|c| c.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0)).optional())
    }

    fn set(&self, key: &str, value: Option<&str>) -> Result<()> {
        self.with(|c| {
            match value {
                Some(v) => c.execute("INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)", params![key, v]),
                None => c.execute("DELETE FROM kv WHERE key = ?1", [key]),
            }
            .map(drop)
        })
    }

    fn clear_prefix(&self, prefix: &str) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM kv WHERE substr(key, 1, length(?1)) = ?1", [prefix]).map(drop))
    }

    fn put_version(&self, v: &StoredVersion) -> Result<()> {
        self.with(|c| {
            c.execute(
                &format!(
                    "INSERT OR IGNORE INTO versions ({VERSION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
                ),
                params![
                    v.note_id,
                    v.seal_id,
                    v.at,
                    v.captured_at,
                    v.expire_at,
                    v.reason.as_str(),
                    v.meta,
                    v.body,
                    v.state.as_str()
                ],
            )
            .map(drop)
        })
    }

    fn versions_for(&self, note_id: &str) -> Result<Vec<StoredVersion>> {
        self.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {VERSION_COLUMNS} FROM versions WHERE note_id = ?1 ORDER BY captured_at DESC"
            ))?;
            let rows = stmt.query_map([note_id], read_version)?;
            rows.collect()
        })
    }

    fn pending_versions(&self, limit: usize) -> Result<Vec<StoredVersion>> {
        self.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {VERSION_COLUMNS} FROM versions WHERE state = 'queued' ORDER BY captured_at LIMIT ?1"
            ))?;
            let rows = stmt.query_map([limit as i64], read_version)?;
            rows.collect()
        })
    }

    fn set_version_state(&self, note_id: &str, seal_id: &str, state: VersionState) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE versions SET state = ?3 WHERE note_id = ?1 AND seal_id = ?2",
                params![note_id, seal_id, state.as_str()],
            )
            .map(drop)
        })
    }

    fn prune_versions(&self, now: i64) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM versions WHERE expire_at <= ?1", [now]).map(drop))
    }

    fn wipe(&self) -> Result<()> {
        self.with(|c| {
            c.execute_batch(
                "DELETE FROM notes; DELETE FROM folders; DELETE FROM queue; DELETE FROM kv; DELETE FROM versions;",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_store_conforms() {
        crate::store::memory::conformance::run(&SqliteStore::open_in_memory().unwrap());
    }

    #[test]
    fn rows_survive_reopening_the_file() {
        let dir = std::env::temp_dir().join(format!("hushjot-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.db");
        {
            let store = SqliteStore::open(&path).unwrap();
            store
                .put_note(&StoredNote {
                    id: "n".into(),
                    meta: vec![1],
                    body: None,
                    updated_at: 1,
                    revision: 1,
                    deleted: true,
                })
                .unwrap();
            store.enqueue("n", ChangeKind::Note).unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        assert!(store.note("n").unwrap().unwrap().deleted);
        assert!(store.is_queued("n").unwrap());
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }
}
