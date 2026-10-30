//! Where the session and the cached vault key are kept between runs.
//!
//! Two secrets, both per account:
//!
//! - `refresh` — the Firebase refresh token, so the app can stay signed in.
//! - `dek:<fingerprint>` — the raw vault key, **only if the user chose to stay
//!   unlocked on this device**. Argon2id takes ~100 ms; caching saves that and
//!   the passphrase prompt. The name carries a fingerprint of the key record's
//!   passphrase wrap, so changing the passphrase on any device makes the cache
//!   miss by itself.
//!
//! The account password is never stored. When the refresh token dies the app
//! gets [`crate::Error::SessionExpired`] and asks again.
//!
//! A cached key is exactly as safe as the OS secret store: it protects against
//! another user on the machine and against a copied disk, not against someone
//! already using this user's unlocked session. The integration guide says so.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::Result;

/// A place to keep small secrets. Implementations must be thread-safe.
pub trait SecretStore: Send + Sync {
    /// Read a secret.
    fn get(&self, account: &str, name: &str) -> Result<Option<String>>;
    /// Write a secret.
    fn set(&self, account: &str, name: &str, value: &str) -> Result<()>;
    /// Delete a secret; deleting a missing one is not an error.
    fn delete(&self, account: &str, name: &str) -> Result<()>;
}

/// Secrets in process memory: nothing survives a restart. The default, and
/// the right choice when the app should ask for the passphrase every launch.
#[derive(Default)]
pub struct MemorySecrets {
    inner: Mutex<HashMap<(String, String), String>>,
}

impl MemorySecrets {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for MemorySecrets {
    fn get(&self, account: &str, name: &str) -> Result<Option<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(account.into(), name.into()))
            .cloned())
    }

    fn set(&self, account: &str, name: &str, value: &str) -> Result<()> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((account.into(), name.into()), value.into());
        Ok(())
    }

    fn delete(&self, account: &str, name: &str) -> Result<()> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&(account.into(), name.into()));
        Ok(())
    }
}

/// The OS secret store: macOS Keychain, Windows Credential Manager, or the
/// Secret Service on Linux (via the `keyring` crate).
#[cfg(feature = "keyring")]
pub struct KeyringSecrets {
    service: String,
}

#[cfg(feature = "keyring")]
impl KeyringSecrets {
    /// Secrets under `service` — use your app's identifier, e.g.
    /// `"com.example.editor.hushjot"`, so they are attributed to your app.
    pub fn new(service: impl Into<String>) -> Self {
        Self { service: service.into() }
    }

    fn entry(&self, account: &str, name: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, &format!("{account}:{name}"))
            .map_err(|e| crate::Error::Secrets(e.to_string()))
    }
}

#[cfg(feature = "keyring")]
impl SecretStore for KeyringSecrets {
    fn get(&self, account: &str, name: &str) -> Result<Option<String>> {
        match self.entry(account, name)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(crate::Error::Secrets(e.to_string())),
        }
    }

    fn set(&self, account: &str, name: &str, value: &str) -> Result<()> {
        self.entry(account, name)?.set_password(value).map_err(|e| crate::Error::Secrets(e.to_string()))
    }

    fn delete(&self, account: &str, name: &str) -> Result<()> {
        match self.entry(account, name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(crate::Error::Secrets(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_secrets_are_per_account_and_name() {
        let s = MemorySecrets::new();
        s.set("a@x", "refresh", "1").unwrap();
        s.set("b@x", "refresh", "2").unwrap();
        assert_eq!(s.get("a@x", "refresh").unwrap().as_deref(), Some("1"));
        assert_eq!(s.get("a@x", "dek:f").unwrap(), None);
        s.delete("a@x", "refresh").unwrap();
        s.delete("a@x", "refresh").unwrap();
        assert_eq!(s.get("a@x", "refresh").unwrap(), None);
        assert_eq!(s.get("b@x", "refresh").unwrap().as_deref(), Some("2"));
    }
}
