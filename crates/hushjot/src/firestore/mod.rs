//! HushJot Sync over the Firestore REST API (the CLI's `rest.ts`, `write.ts`
//! and `pull.ts`, and the app's Firebase adapter).
//!
//! The REST endpoint *is* Firestore, and the security rules never see which
//! transport a request came over, so this client is held to exactly the rules
//! the web app is. Five things here are load-bearing:
//!
//! - **`syncedAt` is a server transform**, never a value we send. The rules
//!   require `syncedAt == request.time`, so a device with a skewed clock cannot
//!   hide its writes from another device's delta cursor. Writes therefore go
//!   through `:commit` with `updateTransforms`.
//! - **No `updateMask` on documents.** The rules check the resulting document
//!   with `hasOnly`/`hasAll`, so every write is a full replace. The meter is the
//!   single exception: it is merged into, because no client holds all of it.
//! - **Every write carries the meter in the same commit**, or it is refused.
//! - **A 403 on a write is ambiguous.** "Your revision is stale" and "you may
//!   not write here" are the same status. One extra read tells them apart:
//!   misreading an authorisation failure as a conflict would fork a note on
//!   every retry, forever.
//! - **A 403 on a read is checked for a pause** before it is believed (one
//!   `GET status/lockdown`, which only the maintenance ruleset allows).

pub mod value;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use hushjot_core::history::VersionReason;
use hushjot_core::key_record::KeyRecord;
use hushjot_core::usage::{
    charged_bytes, counted_note_bytes, meter_delta, usage_month, Usage, BASE_STORAGE_BYTES, HISTORY_BYTES_PER_MONTH,
    MAX_PAYLOAD_BYTES, SHARES_PER_MONTH, USAGE_DOC,
};

use crate::auth::TokenSource;
use crate::backend::{
    Backend, Cursor, KeyRecordDoc, PauseVerdict, Position, PullPage, RemoteFolder, RemoteNote, RemoteShare,
    RemoteVersion, ShareIndexEntry,
};
use crate::config::Config;
use crate::error::{Error, Result};
use value::{
    boolean, bytes, int, read_bool, read_bytes, read_int, read_map_int, read_string, read_timestamp, reference, string,
    timestamp, to_plain, Fields,
};

/// The document schema version every write carries.
pub const SCHEMA_VERSION: i64 = 1;
/// The sentinel only the maintenance ruleset lets anyone read.
pub const LOCKDOWN_SENTINEL: &str = "status/lockdown";

/// A document as read.
#[derive(Debug, Clone)]
pub struct Document {
    /// The full resource name.
    pub name: String,
    /// Its fields.
    pub fields: Fields,
}

impl Document {
    /// The last path segment.
    pub fn id(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or("")
    }

    fn get(&self, field: &str) -> Option<&Value> {
        self.fields.get(field)
    }
}

/// The meter's half of a commit (the app's `core/sync/usage.ts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeterChange {
    /// The document this commit changes, relative to `users/{uid}`.
    pub last_doc: String,
    /// The change in charged bytes, when the commit writes a note or folder.
    pub bytes: Option<i64>,
    /// Charged bytes of a version uploaded in this commit.
    pub history: i64,
    /// Shares published in this commit.
    pub shares: i64,
    /// Start a new push window instead of counting into the current one.
    pub reset: bool,
    /// `usage_month(now)`.
    pub month: String,
}

impl MeterChange {
    /// A plain push of `last_doc`.
    pub fn new(last_doc: impl Into<String>) -> Self {
        Self {
            last_doc: last_doc.into(),
            bytes: None,
            history: 0,
            shares: 0,
            reset: false,
            month: usage_month(chrono::Utc::now().timestamp_millis()),
        }
    }
}

/// The `:commit` write for the meter: a masked merge with increments, so two
/// writers at once both land. The month is backtick-quoted because a field
/// path segment may not start with a digit.
pub fn usage_write(name: &str, change: &MeterChange) -> Value {
    let mut fields = serde_json::Map::new();
    fields.insert("lastDoc".into(), string(&change.last_doc));
    let mut mask = vec!["lastDoc"];
    if change.reset {
        fields.insert("pn".into(), int(1));
        mask.push("pn");
    }
    let mut transforms = Vec::new();
    if let Some(delta) = change.bytes {
        transforms.push(json!({ "fieldPath": "bytes", "increment": int(delta) }));
    }
    transforms.push(if change.reset {
        json!({ "fieldPath": "ws", "setToServerValue": "REQUEST_TIME" })
    } else {
        json!({ "fieldPath": "pn", "increment": int(1) })
    });
    transforms.push(json!({ "fieldPath": format!("pushes.`{}`", change.month), "increment": int(1) }));
    if change.history > 0 {
        transforms.push(json!({ "fieldPath": format!("hist.`{}`", change.month), "increment": int(change.history) }));
    }
    if change.shares > 0 {
        transforms.push(json!({ "fieldPath": format!("shares.`{}`", change.month), "increment": int(change.shares) }));
    }
    json!({
        "update": { "name": name, "fields": fields },
        "updateMask": { "fieldPaths": mask },
        "updateTransforms": transforms,
    })
}

/// A full-replace document write, with `server_time` fields set to the
/// server's `REQUEST_TIME`.
pub fn document_write(name: &str, fields: Fields, server_time: &[&str]) -> Value {
    let transforms: Vec<Value> =
        server_time.iter().map(|f| json!({ "fieldPath": f, "setToServerValue": "REQUEST_TIME" })).collect();
    json!({ "update": { "name": name, "fields": fields }, "updateTransforms": transforms })
}

/// The fields of a note document.
pub fn note_fields(note: &RemoteNote) -> Fields {
    let mut fields = Fields::new();
    fields.insert("v".into(), int(SCHEMA_VERSION));
    fields.insert("meta".into(), bytes(&note.meta));
    if let Some(body) = &note.body {
        fields.insert("body".into(), bytes(body));
    }
    fields.insert("revision".into(), int(note.revision));
    fields.insert("updatedAt".into(), int(note.updated_at));
    fields.insert("deleted".into(), boolean(note.deleted));
    fields
}

/// The fields of a folder document.
pub fn folder_fields(folder: &RemoteFolder) -> Fields {
    let mut fields = Fields::new();
    fields.insert("v".into(), int(SCHEMA_VERSION));
    fields.insert("data".into(), bytes(&folder.data));
    fields.insert("createdAt".into(), int(folder.created_at));
    fields.insert("updatedAt".into(), int(folder.updated_at));
    fields.insert("deleted".into(), boolean(folder.deleted));
    fields
}

/// Read a note document; `None` when it is not one.
pub fn read_note(doc: &Document) -> Option<RemoteNote> {
    Some(RemoteNote {
        id: doc.id().to_string(),
        meta: read_bytes(doc.get("meta"))?,
        body: read_bytes(doc.get("body")),
        revision: read_int(doc.get("revision"))?,
        updated_at: read_int(doc.get("updatedAt")).unwrap_or(0),
        deleted: read_bool(doc.get("deleted")),
        synced_at: read_timestamp(doc.get("syncedAt")).unwrap_or(0),
    })
}

/// Read a folder document; `None` when it is not one.
pub fn read_folder(doc: &Document) -> Option<RemoteFolder> {
    let created_at = read_int(doc.get("createdAt")).unwrap_or(0);
    Some(RemoteFolder {
        id: doc.id().to_string(),
        data: read_bytes(doc.get("data"))?,
        created_at,
        updated_at: read_int(doc.get("updatedAt")).unwrap_or(created_at),
        deleted: read_bool(doc.get("deleted")),
        synced_at: read_timestamp(doc.get("syncedAt")).unwrap_or(0),
    })
}

fn read_version(note_id: &str, doc: &Document) -> Option<RemoteVersion> {
    Some(RemoteVersion {
        note_id: note_id.to_string(),
        id: doc.id().to_string(),
        meta: read_bytes(doc.get("meta"))?,
        body: read_bytes(doc.get("body"))?,
        at: read_int(doc.get("at")).unwrap_or(0),
        reason: read_string(doc.get("reason")).and_then(|r| VersionReason::parse(&r)).unwrap_or(VersionReason::Edit),
        expire_at: read_timestamp(doc.get("expireAt")).unwrap_or(0),
        created_at: read_timestamp(doc.get("createdAt")),
    })
}

/// The delta query for one collection (the CLI's `pull.ts`): ordered by
/// `(syncedAt, __name__)`, which is total, resuming strictly after the last
/// document consumed. `__name__` is appended to every single-field index
/// automatically, so no composite index is needed.
pub fn pull_query(config: &Config, uid: &str, collection: &str, from: &Position, limit: usize) -> Value {
    let mut query = json!({
        "from": [{ "collectionId": collection }],
        "orderBy": [
            { "field": { "fieldPath": "syncedAt" }, "direction": "ASCENDING" },
            { "field": { "fieldPath": "__name__" }, "direction": "ASCENDING" },
        ],
        "limit": limit,
    });
    if from.at == 0 && from.id.is_empty() {
        return query;
    }
    if !from.id.is_empty() {
        query["startAt"] = json!({
            "values": [timestamp(from.at), reference(&config.document_name(&format!("users/{uid}/{collection}/{}", from.id)))],
            "before": false,
        });
    } else {
        query["where"] = json!({ "fieldFilter": {
            "field": { "fieldPath": "syncedAt" }, "op": "GREATER_THAN_OR_EQUAL", "value": timestamp(from.at),
        }});
    }
    query
}

/// Advance to the page's **last** document, never the largest timestamp seen,
/// and never backwards.
fn advance(from: &Position, last: Option<(String, i64)>) -> Position {
    match last {
        Some((id, at)) if at >= from.at => Position { at, id },
        _ => from.clone(),
    }
}

/// A thin Firestore REST client with token refresh and refusal handling.
pub struct FirestoreClient {
    http: reqwest::Client,
    config: Config,
    tokens: Arc<TokenSource>,
}

enum Verb {
    Get,
    Post(Value),
}

impl FirestoreClient {
    /// A client for `config`, authenticating with `tokens`.
    pub fn new(http: reqwest::Client, config: Config, tokens: Arc<TokenSource>) -> Self {
        Self { http, config, tokens }
    }

    /// The configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    async fn send(&self, verb: &Verb, url: &str) -> Result<reqwest::Response> {
        let mut token = self.tokens.token().await?;
        for attempt in 0..2 {
            let request = match verb {
                Verb::Get => self.http.get(url),
                Verb::Post(body) => self.http.post(url).json(body),
            };
            let response = request
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|e| Error::Transient(format!("could not reach Firestore: {e}")))?;
            if response.status().as_u16() == 401 && attempt == 0 {
                token = self.tokens.refresh_after_401(&token).await?;
                continue;
            }
            return Ok(response);
        }
        Err(Error::SessionExpired)
    }

    fn classify(status: reqwest::StatusCode, context: &str) -> Error {
        match status.as_u16() {
            401 => Error::SessionExpired,
            429 | 500..=599 => Error::Transient(format!("{context}: HTTP {status}")),
            _ => Error::Protocol(format!("{context}: HTTP {status}")),
        }
    }

    /// One `GET status/lockdown`: allowed (404, or 200) means every client is
    /// refused for maintenance; refused means the service is running.
    pub async fn probe_pause(&self) -> PauseVerdict {
        let url = format!("{}/{LOCKDOWN_SENTINEL}", self.config.documents_url());
        match self.send(&Verb::Get, &url).await {
            Ok(r) if r.status().as_u16() == 404 || r.status().is_success() => PauseVerdict::Paused,
            Ok(r) if r.status().as_u16() == 403 => PauseVerdict::Running,
            _ => PauseVerdict::Unknown,
        }
    }

    async fn refused(&self) -> Error {
        match self.probe_pause().await {
            PauseVerdict::Paused => Error::Paused,
            _ => Error::NotApproved,
        }
    }

    /// A document, or `None` when it is absent.
    pub async fn get(&self, relative: &str) -> Result<Option<Document>> {
        let url = format!("{}/{relative}", self.config.documents_url());
        let response = self.send(&Verb::Get, &url).await?;
        match response.status().as_u16() {
            404 => return Ok(None),
            403 => return Err(self.refused().await),
            _ if !response.status().is_success() => return Err(Self::classify(response.status(), relative)),
            _ => {}
        }
        let body: Value = response.json().await.map_err(|e| Error::Protocol(e.to_string()))?;
        let Some(name) = body.get("name").and_then(Value::as_str) else { return Ok(None) };
        Ok(Some(Document {
            name: name.to_string(),
            fields: body.get("fields").and_then(Value::as_object).cloned().unwrap_or_default(),
        }))
    }

    /// A structured query under `parent` (a document path below `documents/`).
    pub async fn run_query(&self, parent: &str, structured_query: Value) -> Result<Vec<Document>> {
        let url = format!("{}/{parent}:runQuery", self.config.documents_url());
        let response = self.send(&Verb::Post(json!({ "structuredQuery": structured_query })), &url).await?;
        if response.status().as_u16() == 403 {
            return Err(self.refused().await);
        }
        if !response.status().is_success() {
            return Err(Self::classify(response.status(), "runQuery"));
        }
        let rows: Vec<Value> = response.json().await.map_err(|e| Error::Protocol(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let doc = row.get("document")?;
                Some(Document {
                    name: doc.get("name")?.as_str()?.to_string(),
                    fields: doc.get("fields").and_then(Value::as_object).cloned().unwrap_or_default(),
                })
            })
            .collect())
    }

    /// Several writes, all or nothing. `Ok(false)` on a 403, which the caller
    /// must disambiguate.
    pub async fn commit(&self, writes: Vec<Value>) -> Result<bool> {
        let url = format!("{}:commit", self.config.documents_url());
        let response = self.send(&Verb::Post(json!({ "writes": writes })), &url).await?;
        if response.status().as_u16() == 403 {
            return Ok(false);
        }
        if !response.status().is_success() {
            return Err(Self::classify(response.status(), "commit"));
        }
        Ok(true)
    }
}

/// [`Backend`] over the Firestore REST API.
pub struct FirestoreBackend {
    client: FirestoreClient,
    uid: String,
}

impl FirestoreBackend {
    /// A backend for `uid`.
    pub fn new(client: FirestoreClient, uid: impl Into<String>) -> Self {
        Self { client, uid: uid.into() }
    }

    fn user_path(&self, relative: &str) -> String {
        format!("users/{}/{relative}", self.uid)
    }

    fn name(&self, relative: &str) -> String {
        self.client.config().document_name(relative)
    }

    fn meter(&self, change: &MeterChange) -> Value {
        usage_write(&self.name(&self.user_path(USAGE_DOC)), change)
    }

    async fn commit_note(&self, note: &RemoteNote, stored: i64, reset: bool) -> Result<bool> {
        let size = note.stored_bytes();
        let change = MeterChange {
            bytes: Some(meter_delta(size, stored)),
            reset,
            ..MeterChange::new(format!("notes/{}", note.id))
        };
        let doc = document_write(
            &self.name(&self.user_path(&format!("notes/{}", note.id))),
            note_fields(note),
            &["syncedAt"],
        );
        self.client.commit(vec![doc, self.meter(&change)]).await
    }

    async fn commit_folder(&self, folder: &RemoteFolder, stored: i64, reset: bool) -> Result<bool> {
        let size = folder.data.len() as i64;
        let change = MeterChange {
            bytes: Some(meter_delta(size, stored)),
            reset,
            ..MeterChange::new(format!("folders/{}", folder.id))
        };
        let doc = document_write(
            &self.name(&self.user_path(&format!("folders/{}", folder.id))),
            folder_fields(folder),
            &["syncedAt"],
        );
        self.client.commit(vec![doc, self.meter(&change)]).await
    }

    /// The app's `settleRefusal`: retry once with the size the server actually
    /// holds, or as a window reset when the meter says the window is full;
    /// otherwise explain the refusal as storage or push limit if it was one.
    async fn settle_refusal<F, Fut>(&self, id: &str, size: i64, actual: i64, sent: i64, retry: F) -> Result<bool>
    where
        F: Fn(i64, bool) -> Fut,
        Fut: std::future::Future<Output = Result<bool>>,
    {
        let usage = self.usage().await.ok();
        let full = usage.is_some_and(|u| u.push_window_full());
        if (actual != sent || full) && retry(actual, full).await? {
            return Ok(true);
        }
        if let Some(usage) = usage {
            let delta = meter_delta(size, actual);
            if delta > 0 && usage.bytes + delta > usage.quota {
                return Err(Error::QuotaExceeded { delta, used: Some(usage.bytes), quota: Some(usage.quota) });
            }
            if full {
                return Err(Error::PushLimit { retry_at: usage.window_retry_at() });
            }
        }
        let _ = id;
        Ok(false)
    }
}

#[async_trait]
impl Backend for FirestoreBackend {
    async fn uid(&self) -> String {
        self.uid.clone()
    }

    async fn key_record(&self) -> Result<KeyRecordDoc> {
        let Some(doc) = self.client.get(&self.user_path("keys/primary")).await? else {
            return Ok(KeyRecordDoc::Missing);
        };
        if let Some(record) = doc.get("record") {
            return Ok(KeyRecordDoc::Record(Box::new(KeyRecord::from_json(&to_plain(record))?)));
        }
        if doc.get("resetAt").is_some() {
            return Ok(KeyRecordDoc::Tombstone);
        }
        Err(Error::Protocol("the key record document is malformed".into()))
    }

    async fn fetch_note(&self, id: &str) -> Result<Option<RemoteNote>> {
        Ok(self.client.get(&self.user_path(&format!("notes/{id}"))).await?.as_ref().and_then(read_note))
    }

    async fn push_note(&self, note: &RemoteNote, stored_bytes: i64) -> Result<()> {
        let size = note.stored_bytes();
        if size as usize > MAX_PAYLOAD_BYTES {
            return Err(Error::PayloadTooLarge { note_id: note.id.clone(), bytes: size as usize });
        }
        if self.commit_note(note, stored_bytes, false).await? {
            return Ok(());
        }
        // Refused. A read carries no revision check, so it says what the
        // refusal meant (a refused read raises NotApproved or Paused itself).
        let stored = self.fetch_note(&note.id).await?;
        if stored.as_ref().is_none_or(|s| s.revision == note.revision - 1) {
            let actual = stored.as_ref().map_or(0, RemoteNote::stored_bytes);
            let retry = |stored: i64, reset: bool| self.commit_note(note, stored, reset);
            if self.settle_refusal(&note.id, size, actual, stored_bytes, retry).await? {
                return Ok(());
            }
        }
        Err(Error::RevisionConflict { note_id: note.id.clone() })
    }

    async fn push_folder(&self, folder: &RemoteFolder, stored_bytes: i64) -> Result<()> {
        if self.commit_folder(folder, stored_bytes, false).await? {
            return Ok(());
        }
        let actual = self
            .client
            .get(&self.user_path(&format!("folders/{}", folder.id)))
            .await?
            .as_ref()
            .and_then(read_folder)
            .map_or(0, |f| f.data.len() as i64);
        let retry = |stored: i64, reset: bool| self.commit_folder(folder, stored, reset);
        if self.settle_refusal(&folder.id, folder.data.len() as i64, actual, stored_bytes, retry).await? {
            return Ok(());
        }
        Err(Error::WriteRefused(format!("the folder {} was refused", folder.id)))
    }

    async fn pull(&self, cursor: &Cursor, limit: usize) -> Result<PullPage> {
        let parent = format!("users/{}", self.uid);
        let config = self.client.config();
        let (notes, folders) = tokio::join!(
            self.client.run_query(&parent, pull_query(config, &self.uid, "notes", &cursor.notes, limit)),
            self.client.run_query(&parent, pull_query(config, &self.uid, "folders", &cursor.folders, limit)),
        );
        let (notes, folders) = (notes?, folders?);
        let last = |docs: &[Document]| {
            docs.last().map(|d| (d.id().to_string(), read_timestamp(d.get("syncedAt")).unwrap_or(0)))
        };
        Ok(PullPage {
            cursor: Cursor {
                notes: advance(&cursor.notes, last(&notes)),
                folders: advance(&cursor.folders, last(&folders)),
            },
            has_more: notes.len() == limit || folders.len() == limit,
            notes: notes.iter().filter_map(read_note).collect(),
            folders: folders.iter().filter_map(read_folder).collect(),
        })
    }

    async fn push_versions(&self, versions: &[RemoteVersion]) -> Result<Vec<RemoteVersion>> {
        let mut refused = Vec::new();
        for (i, version) in versions.iter().enumerate() {
            let relative = self.user_path(&format!("notes/{}/versions/{}", version.note_id, version.id));
            let charge = charged_bytes(counted_note_bytes(Some(&version.meta), Some(&version.body)));
            let write = |reset: bool| {
                let mut fields = Fields::new();
                fields.insert("v".into(), int(SCHEMA_VERSION));
                fields.insert("meta".into(), bytes(&version.meta));
                fields.insert("body".into(), bytes(&version.body));
                fields.insert("at".into(), int(version.at));
                fields.insert("reason".into(), string(version.reason.as_str()));
                fields.insert("expireAt".into(), timestamp(version.expire_at));
                let change = MeterChange {
                    history: charge,
                    reset,
                    ..MeterChange::new(format!("notes/{}/versions/{}", version.note_id, version.id))
                };
                vec![document_write(&self.name(&relative), fields, &["createdAt"]), self.meter(&change)]
            };
            if self.client.commit(write(false)).await? {
                continue;
            }
            // Already there — another device with the same sealing, or a retry
            // whose acknowledgement never arrived.
            if self.client.get(&relative).await?.is_some() {
                continue;
            }
            let usage = self.usage().await.ok();
            if usage.is_some_and(|u| u.history_this_month + charge > HISTORY_BYTES_PER_MONTH) {
                refused.extend_from_slice(&versions[i..]);
                return Ok(refused);
            }
            if let Some(usage) = usage.filter(Usage::push_window_full) {
                if self.client.commit(write(true)).await? {
                    continue;
                }
                return Err(Error::PushLimit { retry_at: usage.window_retry_at() });
            }
            refused.push(version.clone());
        }
        Ok(refused)
    }

    async fn list_versions(&self, note_id: &str) -> Result<Vec<RemoteVersion>> {
        let query = json!({
            "from": [{ "collectionId": "versions" }],
            "orderBy": [
                { "field": { "fieldPath": "createdAt" }, "direction": "DESCENDING" },
                { "field": { "fieldPath": "__name__" }, "direction": "DESCENDING" },
            ],
            "limit": 200,
        });
        let docs = self.client.run_query(&self.user_path(&format!("notes/{note_id}")), query).await?;
        let now = chrono::Utc::now().timestamp_millis();
        // The TTL sweep lags expiry by up to a day; an expired version is gone
        // as far as any reader is concerned.
        Ok(docs.iter().filter_map(|d| read_version(note_id, d)).filter(|v| v.expire_at > now).collect())
    }

    async fn publish_share(&self, share: &RemoteShare, index: &ShareIndexEntry) -> Result<()> {
        let write = |reset: bool| {
            let mut private = Fields::new();
            private.insert("v".into(), int(SCHEMA_VERSION));
            private.insert("noteId".into(), string(&index.note_id));
            private.insert("key".into(), bytes(&index.key));
            private.insert("expireAt".into(), timestamp(share.expire_at));
            let mut public = Fields::new();
            public.insert("v".into(), int(SCHEMA_VERSION));
            public.insert("payload".into(), bytes(&share.payload));
            public.insert("expireAt".into(), timestamp(share.expire_at));
            let change = MeterChange { shares: 1, reset, ..MeterChange::new(format!("shares/{}", share.id)) };
            vec![
                document_write(&self.name(&self.user_path(&format!("shares/{}", share.id))), private, &["createdAt"]),
                document_write(&self.name(&format!("shares/{}", share.id)), public, &["createdAt"]),
                self.meter(&change),
            ]
        };
        if self.client.commit(write(false)).await? {
            return Ok(());
        }
        let usage = self.usage().await?;
        if usage.shares_this_month >= SHARES_PER_MONTH {
            return Err(Error::ShareLimit);
        }
        if usage.push_window_full() {
            if self.client.commit(write(true)).await? {
                return Ok(());
            }
            return Err(Error::PushLimit { retry_at: usage.window_retry_at() });
        }
        Err(Error::WriteRefused("the share was refused".into()))
    }

    async fn revoke_share(&self, id: &str) -> Result<()> {
        // Public blob first: deleting the private record first would remove the
        // proof the public delete rule needs. A refusal here means the record
        // is already gone, i.e. it was already revoked.
        let public = json!({ "delete": self.name(&format!("shares/{id}")) });
        let _ = self.client.commit(vec![public]).await?;
        let private = json!({ "delete": self.name(&self.user_path(&format!("shares/{id}"))) });
        if self.client.commit(vec![private]).await? {
            Ok(())
        } else {
            Err(self.client.refused().await)
        }
    }

    async fn list_shares(&self) -> Result<Vec<ShareIndexEntry>> {
        let query = json!({
            "from": [{ "collectionId": "shares" }],
            "orderBy": [{ "field": { "fieldPath": "createdAt" }, "direction": "DESCENDING" }],
        });
        let docs = self.client.run_query(&format!("users/{}", self.uid), query).await?;
        let now = chrono::Utc::now().timestamp_millis();
        Ok(docs
            .iter()
            .filter_map(|d| {
                Some(ShareIndexEntry {
                    id: d.id().to_string(),
                    note_id: read_string(d.get("noteId")).unwrap_or_default(),
                    key: read_bytes(d.get("key"))?,
                    expire_at: read_timestamp(d.get("expireAt")).unwrap_or(0),
                    created_at: read_timestamp(d.get("createdAt")).unwrap_or(0),
                })
            })
            .filter(|s| s.expire_at > now)
            .collect())
    }

    async fn usage(&self) -> Result<Usage> {
        let (meter_path, plan_path) = (self.user_path(USAGE_DOC), format!("entitlements/{}", self.uid));
        let (meter, plan) = tokio::join!(self.client.get(&meter_path), self.client.get(&plan_path));
        let meter = meter?;
        let plan = plan.ok().flatten();
        let month = usage_month(chrono::Utc::now().timestamp_millis());
        let field = |name: &str| meter.as_ref().and_then(|m| m.get(name));
        Ok(Usage {
            bytes: read_int(field("bytes")).unwrap_or(0),
            quota: plan
                .as_ref()
                .and_then(|p| read_int(p.get("storageBytes")))
                .filter(|q| *q > 0)
                .unwrap_or(BASE_STORAGE_BYTES),
            history_this_month: read_map_int(field("hist"), &month).unwrap_or(0),
            shares_this_month: read_map_int(field("shares"), &month).unwrap_or(0),
            window_start: read_timestamp(field("ws")),
            pushes_in_window: read_int(field("pn")).unwrap_or(0),
        })
    }

    async fn probe_pause(&self) -> PauseVerdict {
        self.client.probe_pause().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_meter_write_matches_the_documented_wire_shape() {
        let change = MeterChange { bytes: Some(1024), month: "202610".into(), ..MeterChange::new("notes/n1") };
        assert_eq!(
            usage_write("projects/p/databases/(default)/documents/users/u/usage/current", &change),
            json!({
                "update": { "name": "projects/p/databases/(default)/documents/users/u/usage/current",
                            "fields": { "lastDoc": { "stringValue": "notes/n1" } } },
                "updateMask": { "fieldPaths": ["lastDoc"] },
                "updateTransforms": [
                    { "fieldPath": "bytes", "increment": { "integerValue": "1024" } },
                    { "fieldPath": "pn", "increment": { "integerValue": "1" } },
                    { "fieldPath": "pushes.`202610`", "increment": { "integerValue": "1" } }
                ]
            })
        );
    }

    #[test]
    fn a_reset_sets_the_window_from_the_server_clock() {
        let change = MeterChange {
            reset: true,
            history: 2048,
            month: "202610".into(),
            ..MeterChange::new("notes/n/versions/v")
        };
        let write = usage_write("u", &change);
        assert_eq!(write["updateMask"]["fieldPaths"], json!(["lastDoc", "pn"]));
        assert_eq!(write["update"]["fields"]["pn"], json!({ "integerValue": "1" }));
        let transforms = write["updateTransforms"].as_array().unwrap();
        assert!(transforms.iter().any(|t| t["fieldPath"] == "ws" && t["setToServerValue"] == "REQUEST_TIME"));
        assert!(transforms.iter().all(|t| t["fieldPath"] != "pn" && t["fieldPath"] != "bytes"));
        assert!(transforms
            .iter()
            .any(|t| t["fieldPath"] == "hist.`202610`" && t["increment"]["integerValue"] == "2048"));
    }

    #[test]
    fn note_documents_are_full_replaces_with_a_server_time_stamp() {
        let note = RemoteNote {
            id: "n".into(),
            meta: vec![1],
            body: None,
            revision: 3,
            updated_at: 9,
            deleted: true,
            synced_at: 0,
        };
        let write = document_write("name", note_fields(&note), &["syncedAt"]);
        assert!(write.get("updateMask").is_none());
        assert!(write["update"]["fields"].get("body").is_none(), "a tombstone has no body");
        assert_eq!(write["update"]["fields"]["revision"], json!({ "integerValue": "3" }));
        assert_eq!(
            write["updateTransforms"][0],
            json!({ "fieldPath": "syncedAt", "setToServerValue": "REQUEST_TIME" })
        );
    }

    #[test]
    fn documents_read_back() {
        let note = RemoteNote {
            id: "n".into(),
            meta: vec![1, 2],
            body: Some(vec![3]),
            revision: 2,
            updated_at: 5,
            deleted: false,
            synced_at: 0,
        };
        let mut fields = note_fields(&note);
        fields.insert("syncedAt".into(), timestamp(77));
        let doc = Document { name: "projects/p/databases/(default)/documents/users/u/notes/n".into(), fields };
        assert_eq!(read_note(&doc), Some(RemoteNote { synced_at: 77, ..note }));
        let folder =
            RemoteFolder { id: "f".into(), data: vec![9], created_at: 1, updated_at: 2, deleted: false, synced_at: 0 };
        let doc = Document { name: "x/folders/f".into(), fields: folder_fields(&folder) };
        assert_eq!(read_folder(&doc), Some(folder));
    }

    #[test]
    fn pull_queries_resume_strictly_after_the_last_document() {
        let config = Config::default();
        let start = pull_query(&config, "u", "notes", &Position::default(), 200);
        assert!(start.get("startAt").is_none() && start.get("where").is_none());
        let after = pull_query(&config, "u", "notes", &Position { at: 5, id: "n9".into() }, 200);
        assert_eq!(after["startAt"]["before"], json!(false));
        assert_eq!(
            after["startAt"]["values"][1]["referenceValue"],
            json!("projects/hushjot/databases/(default)/documents/users/u/notes/n9")
        );
        let at_only = pull_query(&config, "u", "folders", &Position { at: 5, id: String::new() }, 200);
        assert_eq!(at_only["where"]["fieldFilter"]["op"], json!("GREATER_THAN_OR_EQUAL"));
    }

    #[test]
    fn the_cursor_never_moves_backwards() {
        let from = Position { at: 10, id: "b".into() };
        assert_eq!(advance(&from, Some(("c".into(), 12))), Position { at: 12, id: "c".into() });
        assert_eq!(advance(&from, Some(("a".into(), 9))), from);
        assert_eq!(advance(&from, None), from);
    }
}
