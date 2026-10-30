//! The entry point: build a [`HushJot`], sign in, unlock.
//!
//! ```text
//! HushJot::builder()…build()   once per app
//!   └─ sign_in(email, password) / resume(email)   → Account   (session)
//!        └─ unlock(passphrase) / unlock_with_recovery_code / unlock_cached
//!                                                → Vault     (key in memory)
//! ```
//!
//! ## Binding the store to the account
//!
//! The local store remembers which account and which vault it belongs to
//! (`app:owner` and `app:vault` in its key-value table). A push into the wrong
//! account is permanent — the server refuses deletes — and notes sealed under
//! a key the account cannot open are just as permanent, so unlocking refuses a
//! store bound to another account, and refuses one bound to a vault the
//! account has since been reset away from.
//!
//! ## Offline launch
//!
//! A saved session and a cached key open the vault without the network, as
//! the web app does. `resume` keeps the uid beside the refresh token (in the
//! secret store) so it can rebuild the session when the token service is
//! unreachable; the first request then renews it, and until that works sync
//! reports Offline. Each successful key-record fetch is copied into the store
//! as `app:key-record`. It is the wrapped key the server already holds, so it
//! sits safely beside the ciphertext. Unlocking falls back to it only on a
//! transient failure and only for the store's own account; an online fetch
//! always wins, so a passphrase change or reset made elsewhere is caught on
//! the next online unlock.

use std::sync::Arc;

use hushjot_core::bytes::{from_base64, to_base64};
use hushjot_core::envelope::Key;
use hushjot_core::key_record::KeyRecord;

use crate::auth::{self, Session, TokenSource};
use crate::backend::{Backend, KeyRecordDoc};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::firestore::{FirestoreBackend, FirestoreClient};
use crate::secrets::{MemorySecrets, SecretStore};
use crate::store::{MemoryStore, Store};
use crate::vault::Vault;

const OWNER_KEY: &str = "app:owner";
const VAULT_KEY: &str = "app:vault";
const KEY_RECORD_KEY: &str = "app:key-record";

/// Builder for [`HushJot`].
pub struct HushJotBuilder {
    config: Config,
    store: Option<Arc<dyn Store>>,
    secrets: Option<Arc<dyn SecretStore>>,
    http: Option<reqwest::Client>,
}

impl HushJotBuilder {
    /// Override the endpoints (tests, emulator).
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Where notes are kept on this device. Defaults to memory (nothing kept).
    pub fn store(mut self, store: impl Store + 'static) -> Self {
        self.store = Some(Arc::new(store));
        self
    }

    /// A shared store.
    pub fn store_arc(mut self, store: Arc<dyn Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// Where the session and (if chosen) the vault key are kept. Defaults to
    /// memory: sign in and unlock every launch.
    pub fn secrets(mut self, secrets: impl SecretStore + 'static) -> Self {
        self.secrets = Some(Arc::new(secrets));
        self
    }

    /// Use your own HTTP client (proxies, timeouts).
    pub fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    /// Finish.
    pub fn build(self) -> Result<HushJot> {
        let http = match self.http {
            Some(h) => h,
            None => reqwest::Client::builder()
                .user_agent(concat!("hushjot-rs/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .map_err(|e| Error::Protocol(e.to_string()))?,
        };
        Ok(HushJot {
            config: self.config,
            store: self.store.unwrap_or_else(|| Arc::new(MemoryStore::new())),
            secrets: self.secrets.unwrap_or_else(|| Arc::new(MemorySecrets::new())),
            http,
        })
    }
}

/// The SDK's entry point. Make one per app.
pub struct HushJot {
    config: Config,
    store: Arc<dyn Store>,
    secrets: Arc<dyn SecretStore>,
    http: reqwest::Client,
}

impl HushJot {
    /// Start building.
    pub fn builder() -> HushJotBuilder {
        HushJotBuilder { config: Config::default(), store: None, secrets: None, http: None }
    }

    /// Sign in with the account's email and password (accounts are created at
    /// hushjot.com). The refresh token is saved so [`HushJot::resume`] works
    /// next launch; the password is not.
    pub async fn sign_in(&self, email: &str, password: &str) -> Result<Account> {
        let email = email.trim().to_lowercase();
        let session = auth::sign_in_with_password(&self.http, &self.config, &email, password).await?;
        self.secrets.set(&email, "refresh", &session.refresh_token)?;
        self.secrets.set(&email, "uid", &session.uid)?;
        Ok(self.account(session))
    }

    /// Sign in with the saved refresh token, if there is one and it still works.
    /// Offline, a session saved by this version resumes anyway and is checked
    /// once the network returns (see the module notes).
    pub async fn resume(&self, email: &str) -> Result<Option<Account>> {
        let email = email.trim().to_lowercase();
        let Some(token) = self.secrets.get(&email, "refresh")? else { return Ok(None) };
        match auth::refresh(&self.http, &self.config, &email, &token).await {
            Ok(session) => {
                if session.refresh_token != token {
                    self.secrets.set(&email, "refresh", &session.refresh_token)?;
                }
                self.secrets.set(&email, "uid", &session.uid)?;
                Ok(Some(self.account(session)))
            }
            Err(Error::Transient(reason)) => match self.secrets.get(&email, "uid")? {
                Some(uid) => Ok(Some(self.account(Session::offline(uid, email, token)))),
                None => Err(Error::Transient(reason)),
            },
            Err(Error::SessionExpired) => {
                self.secrets.delete(&email, "refresh")?;
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn account(&self, session: Session) -> Account {
        let secrets = self.secrets.clone();
        let email = session.email.to_lowercase();
        let uid = session.uid.clone();
        let saver_email = email.clone();
        let tokens = Arc::new(TokenSource::new(self.http.clone(), self.config.clone(), session, move |fresh| {
            let _ = secrets.set(&saver_email, "refresh", fresh);
        }));
        let client = FirestoreClient::new(self.http.clone(), self.config.clone(), tokens);
        Account {
            uid: uid.clone(),
            email,
            backend: Arc::new(FirestoreBackend::new(client, uid)),
            store: self.store.clone(),
            secrets: self.secrets.clone(),
            share_origin: self.config.share_origin.clone(),
        }
    }

    /// An account over a custom [`Backend`] — for tests and for apps that
    /// proxy the protocol. Most apps use [`HushJot::sign_in`].
    pub async fn account_with_backend(&self, email: &str, backend: Arc<dyn Backend>) -> Account {
        Account {
            uid: backend.uid().await,
            email: email.to_lowercase(),
            backend,
            store: self.store.clone(),
            secrets: self.secrets.clone(),
            share_origin: self.config.share_origin.clone(),
        }
    }

    /// Erase this device's copy of the notes (ciphertext), outbox and sync
    /// state. Unsynced changes are lost. Notes on the server are untouched.
    pub fn wipe_local_data(&self) -> Result<()> {
        self.store.wipe()
    }
}

/// A signed-in account, not yet unlocked.
pub struct Account {
    uid: String,
    email: String,
    backend: Arc<dyn Backend>,
    store: Arc<dyn Store>,
    secrets: Arc<dyn SecretStore>,
    share_origin: String,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").field("uid", &self.uid).field("email", &self.email).finish_non_exhaustive()
    }
}

/// How to unlock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnlockOptions {
    /// Keep the vault key in the secret store so the next launch can unlock
    /// without the passphrase ([`Account::unlock_cached`]). Off by default.
    pub stay_unlocked: bool,
}

impl Account {
    /// The Firebase uid.
    pub fn uid(&self) -> &str {
        &self.uid
    }

    /// The account email.
    pub fn email(&self) -> &str {
        &self.email
    }

    /// The account's key record, or why there is none.
    async fn record(&self) -> Result<KeyRecord> {
        match self.backend.key_record().await {
            Ok(KeyRecordDoc::Record(record)) => {
                let json = serde_json::to_string(&record.to_serialized())
                    .map_err(|e| Error::Protocol(format!("could not encode the key record: {e}")))?;
                self.store.set(KEY_RECORD_KEY, Some(&json))?;
                Ok(*record)
            }
            Ok(KeyRecordDoc::Missing) => Err(Error::NoVault),
            Ok(KeyRecordDoc::Tombstone) => Err(Error::VaultReset),
            Err(Error::Transient(reason)) => self.cached_record()?.ok_or(Error::Transient(reason)),
            Err(e) => Err(e),
        }
    }

    /// The last key record fetched for this account, if the store holds one.
    fn cached_record(&self) -> Result<Option<KeyRecord>> {
        if self.store.get(OWNER_KEY)?.as_deref() != Some(self.uid.as_str()) {
            return Ok(None);
        }
        let Some(json) = self.store.get(KEY_RECORD_KEY)? else { return Ok(None) };
        Ok(serde_json::from_str(&json).ok().and_then(|value| KeyRecord::from_json(&value).ok()))
    }

    /// Unlock with the passphrase. Argon2id runs on a blocking thread (~100 ms
    /// on a laptop at the default parameters).
    pub async fn unlock(&self, passphrase: &str, options: UnlockOptions) -> Result<Vault> {
        let record = self.record().await?;
        let passphrase = zeroize::Zeroizing::new(passphrase.to_string());
        let for_kdf = record.clone();
        let key = tokio::task::spawn_blocking(move || for_kdf.unlock_with_passphrase(&passphrase))
            .await
            .map_err(|e| Error::Protocol(format!("key derivation task failed: {e}")))??;
        self.open(record, key, options)
    }

    /// Unlock with the recovery code (fast; no Argon2id).
    pub async fn unlock_with_recovery_code(&self, code: &str, options: UnlockOptions) -> Result<Vault> {
        let record = self.record().await?;
        let key = record.unlock_with_recovery_code(code)?;
        self.open(record, key, options)
    }

    /// Unlock with a key cached by an earlier `stay_unlocked` unlock, if it
    /// still matches the account's key record. `Ok(None)` means: ask for the
    /// passphrase.
    pub async fn unlock_cached(&self) -> Result<Option<Vault>> {
        let record = self.record().await?;
        let name = format!("dek:{}", record.wrap_fingerprint());
        let Some(encoded) = self.secrets.get(&self.email, &name)? else { return Ok(None) };
        let Some(key) = from_base64(&encoded).ok().and_then(|raw| Key::from_slice(&raw)) else {
            self.secrets.delete(&self.email, &name)?;
            return Ok(None);
        };
        self.open(record, key, UnlockOptions { stay_unlocked: true }).map(Some)
    }

    fn open(&self, record: KeyRecord, key: Key, options: UnlockOptions) -> Result<Vault> {
        self.bind_store(&record)?;
        if options.stay_unlocked {
            let name = format!("dek:{}", record.wrap_fingerprint());
            self.secrets.set(&self.email, &name, &to_base64(key.expose_bytes()))?;
            // Remember which name holds it, so signing out can find it after
            // a passphrase change elsewhere has moved the fingerprint on.
            self.secrets.set(&self.email, "dek-name", &name)?;
        }
        Vault::new(
            self.uid.clone(),
            self.email.clone(),
            key,
            self.store.clone(),
            self.backend.clone(),
            self.share_origin.clone(),
        )
    }

    fn bind_store(&self, record: &KeyRecord) -> Result<()> {
        let vault_marker = record.created_at.to_string();
        match self.store.get(OWNER_KEY)? {
            Some(owner) if owner != self.uid => return Err(Error::StoreBelongsToAnotherAccount),
            Some(_) => {
                if let Some(bound) = self.store.get(VAULT_KEY)? {
                    if bound != vault_marker {
                        return Err(Error::VaultReset);
                    }
                }
            }
            None => self.store.set(OWNER_KEY, Some(&self.uid))?,
        }
        self.store.set(VAULT_KEY, Some(&vault_marker))
    }

    /// Forget this device's session and cached key. Local notes stay (as
    /// ciphertext) and nothing on the server changes.
    pub fn sign_out(self) -> Result<()> {
        self.secrets.delete(&self.email, "refresh")?;
        self.secrets.delete(&self.email, "uid")?;
        if let Some(name) = self.secrets.get(&self.email, "dek-name")? {
            self.secrets.delete(&self.email, &name)?;
            self.secrets.delete(&self.email, "dek-name")?;
        }
        Ok(())
    }

    /// Forget a cached vault key without signing out.
    pub async fn forget_cached_key(&self) -> Result<()> {
        let record = self.record().await?;
        self.secrets.delete(&self.email, &format!("dek:{}", record.wrap_fingerprint()))
    }
}
