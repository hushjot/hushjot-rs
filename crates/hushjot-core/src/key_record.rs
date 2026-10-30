//! The key record: how a passphrase and a recovery code each reach the same
//! data-encryption key (the app's `src/core/crypto/keyRecord.ts`).
//!
//! ```text
//! DEK  = 32 random bytes                       random, never derived
//!
//! passphrase ──Argon2id(salt_p)──► KEK_p ──► wrap(DEK) ─┐
//!                                                       ├─► same DEK
//! recovery   ──HKDF(salt_r)─────► KEK_r ──► wrap(DEK) ─┘
//! ```
//!
//! There is no verifier field: a failed GCM tag on unwrap *is* the
//! wrong-secret signal, and a separate verifier would hand an offline attacker
//! a cheaper oracle than the real one.
//!
//! This crate only **reads** key records. Creating a vault mints the DEK and
//! shows the recovery code exactly once, and that stays the HushJot app's job;
//! a third-party client that wrote `keys/primary` could strand an account's
//! notes under a key nobody holds.

use serde::{Deserialize, Serialize};

use crate::bytes::{from_base64, to_base64};
use crate::envelope::{open, Key};
use crate::error::{FormatError, WrongSecretError};
use crate::kdf::{derive_kek_from_passphrase, derive_kek_from_recovery_code, Argon2Params};

/// The only key-record version.
pub const KEY_RECORD_VERSION: u32 = 1;

/// A key record as stored: salts and wrapped keys in standard base64.
///
/// This is the JSON (and Firestore map) shape at `users/{uid}/keys/primary`,
/// field `record`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializedKeyRecord {
    /// Always [`KEY_RECORD_VERSION`].
    pub version: u32,
    /// Argon2id parameters and salt for the passphrase path.
    pub passphrase: SerializedPassphraseParams,
    /// HKDF salt for the recovery-code path.
    pub recovery: SerializedRecoveryParams,
    /// The DEK sealed under the passphrase-derived KEK (version-1 envelope).
    pub wrapped_by_passphrase: String,
    /// The DEK sealed under the recovery-code-derived KEK (version-1 envelope).
    pub wrapped_by_recovery: String,
    /// When the vault was created. Carried through every re-wrap, so two
    /// records with different stamps are two different vaults.
    pub created_at: i64,
}

/// The passphrase half of a serialized key record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializedPassphraseParams {
    /// `"argon2id"`.
    pub name: String,
    /// Memory cost in KiB.
    #[serde(rename = "memoryKiB")]
    pub memory_kib: u32,
    /// Time cost.
    pub iterations: u32,
    /// Lanes.
    pub parallelism: u32,
    /// Base64 salt.
    pub salt: String,
}

/// The recovery half of a serialized key record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedRecoveryParams {
    /// `"HKDF-SHA256"`.
    pub name: String,
    /// Base64 salt.
    pub salt: String,
}

/// A decoded key record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRecord {
    /// Always [`KEY_RECORD_VERSION`].
    pub version: u32,
    /// Argon2id parameters for the passphrase path.
    pub passphrase: Argon2Params,
    /// The passphrase salt.
    pub passphrase_salt: Vec<u8>,
    /// The recovery salt.
    pub recovery_salt: Vec<u8>,
    /// The DEK sealed under the passphrase KEK.
    pub wrapped_by_passphrase: Vec<u8>,
    /// The DEK sealed under the recovery KEK.
    pub wrapped_by_recovery: Vec<u8>,
    /// When the vault was created.
    pub created_at: i64,
}

impl KeyRecord {
    /// Decode a serialized record.
    pub fn from_serialized(raw: &SerializedKeyRecord) -> Result<Self, FormatError> {
        Ok(Self {
            version: raw.version,
            passphrase: Argon2Params {
                name: raw.passphrase.name.clone(),
                memory_kib: raw.passphrase.memory_kib,
                iterations: raw.passphrase.iterations,
                parallelism: raw.passphrase.parallelism,
            },
            passphrase_salt: from_base64(&raw.passphrase.salt)?,
            recovery_salt: from_base64(&raw.recovery.salt)?,
            wrapped_by_passphrase: from_base64(&raw.wrapped_by_passphrase)?,
            wrapped_by_recovery: from_base64(&raw.wrapped_by_recovery)?,
            created_at: raw.created_at,
        })
    }

    /// Parse the record from JSON (or from a Firestore map rendered as JSON).
    pub fn from_json(value: &serde_json::Value) -> Result<Self, FormatError> {
        let raw: SerializedKeyRecord =
            serde_json::from_value(value.clone()).map_err(|_| FormatError::new("malformed key record"))?;
        Self::from_serialized(&raw)
    }

    /// Encode back to the stored shape.
    pub fn to_serialized(&self) -> SerializedKeyRecord {
        SerializedKeyRecord {
            version: self.version,
            passphrase: SerializedPassphraseParams {
                name: self.passphrase.name.clone(),
                memory_kib: self.passphrase.memory_kib,
                iterations: self.passphrase.iterations,
                parallelism: self.passphrase.parallelism,
                salt: to_base64(&self.passphrase_salt),
            },
            recovery: SerializedRecoveryParams { name: "HKDF-SHA256".into(), salt: to_base64(&self.recovery_salt) },
            wrapped_by_passphrase: to_base64(&self.wrapped_by_passphrase),
            wrapped_by_recovery: to_base64(&self.wrapped_by_recovery),
            created_at: self.created_at,
        }
    }

    fn assert_version(&self) -> Result<(), UnlockError> {
        if self.version == KEY_RECORD_VERSION {
            Ok(())
        } else {
            Err(UnlockError::Format(FormatError::new(format!("unsupported key record version {}", self.version))))
        }
    }

    /// Unwrap the DEK with the passphrase. Slow (Argon2id); run it off the UI
    /// thread.
    pub fn unlock_with_passphrase(&self, passphrase: &str) -> Result<Key, UnlockError> {
        self.assert_version()?;
        let kek = derive_kek_from_passphrase(passphrase, &self.passphrase_salt, &self.passphrase)
            .map_err(UnlockError::Format)?;
        unwrap(&kek, &self.wrapped_by_passphrase)
    }

    /// Unwrap the DEK with the recovery code. Fast (HKDF).
    pub fn unlock_with_recovery_code(&self, code: &str) -> Result<Key, UnlockError> {
        self.assert_version()?;
        let kek = derive_kek_from_recovery_code(code, &self.recovery_salt);
        unwrap(&kek, &self.wrapped_by_recovery)
    }

    /// A short fingerprint of the passphrase wrap, for naming a cached DEK.
    ///
    /// Changing the passphrase on *any* device re-randomises the salt and
    /// rewrites `wrappedByPassphrase`, so the fingerprint changes and a cache
    /// keyed on it misses by itself — invalidation falls out of the format
    /// rather than depending on a message reaching this machine. Same scheme
    /// as `@hushjot/cli`: the first 32 hex characters of the SHA-256 of the
    /// base64 string.
    pub fn wrap_fingerprint(&self) -> String {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(to_base64(&self.wrapped_by_passphrase).as_bytes());
        crate::bytes::to_hex(&digest)[..32].to_string()
    }

    /// Whether two records are demonstrably different vaults: their creation
    /// stamps differ. Equal stamps prove nothing.
    pub fn different_vault(&self, other: &KeyRecord) -> bool {
        self.created_at != other.created_at
    }
}

/// Why an unlock failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnlockError {
    /// The secret did not unwrap the key.
    #[error(transparent)]
    WrongSecret(#[from] WrongSecretError),
    /// The record is not one this crate can read.
    #[error(transparent)]
    Format(FormatError),
}

fn unwrap(kek: &Key, wrapped: &[u8]) -> Result<Key, UnlockError> {
    let raw = open(kek, wrapped, None).map_err(|_| UnlockError::WrongSecret(WrongSecretError))?;
    let key = Key::from_slice(&raw).ok_or_else(|| UnlockError::Format(FormatError::new("wrapped key is not 32 bytes")));
    let mut raw = raw;
    zeroize::Zeroize::zeroize(&mut raw);
    key
}

/// Build a key record around `dek`. **For tests and fixtures only**: real
/// vaults are created by the HushJot app, which also shows the recovery code.
#[doc(hidden)]
pub fn create_key_record_for_tests(
    dek: &Key,
    passphrase: &str,
    recovery_code: &str,
    params: &Argon2Params,
    created_at: i64,
) -> KeyRecord {
    let passphrase_salt = crate::bytes::random_bytes(32);
    let recovery_salt = crate::bytes::random_bytes(32);
    let kek_p = derive_kek_from_passphrase(passphrase, &passphrase_salt, params).expect("valid params");
    let kek_r = derive_kek_from_recovery_code(recovery_code, &recovery_salt);
    KeyRecord {
        version: KEY_RECORD_VERSION,
        passphrase: params.clone(),
        passphrase_salt,
        recovery_salt,
        wrapped_by_passphrase: crate::envelope::seal(&kek_p, dek.expose_bytes(), None),
        wrapped_by_recovery: crate::envelope::seal(&kek_r, dek.expose_bytes(), None),
        created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> Argon2Params {
        Argon2Params { memory_kib: 64, iterations: 1, ..Argon2Params::default() }
    }

    #[test]
    fn both_secrets_reach_the_same_key_and_wrong_ones_do_not() {
        let dek = Key::generate();
        let record = create_key_record_for_tests(&dek, "pass", "ABCD-EFGH", &fast(), 1);
        assert_eq!(record.unlock_with_passphrase("pass").unwrap(), dek);
        assert_eq!(record.unlock_with_recovery_code("abcd efgh").unwrap(), dek);
        assert_eq!(record.unlock_with_passphrase("Pass"), Err(UnlockError::WrongSecret(WrongSecretError)));
        assert_eq!(record.unlock_with_recovery_code("ABCD-EFGI"), Err(UnlockError::WrongSecret(WrongSecretError)));
    }

    #[test]
    fn serialization_round_trips_and_uses_the_stored_field_names() {
        let record = create_key_record_for_tests(&Key::generate(), "p", "c", &fast(), 1_790_035_200_000);
        let json = serde_json::to_value(record.to_serialized()).unwrap();
        assert!(json["passphrase"]["memoryKiB"].is_number());
        assert!(json["wrappedByPassphrase"].is_string());
        assert_eq!(KeyRecord::from_json(&json).unwrap(), record);
    }

    #[test]
    fn unknown_versions_are_refused() {
        let mut record = create_key_record_for_tests(&Key::generate(), "p", "c", &fast(), 1);
        record.version = 2;
        assert!(matches!(record.unlock_with_passphrase("p"), Err(UnlockError::Format(_))));
    }

    #[test]
    fn fingerprints_change_when_the_wrap_does() {
        let dek = Key::generate();
        let a = create_key_record_for_tests(&dek, "p", "c", &fast(), 1);
        let b = create_key_record_for_tests(&dek, "p", "c", &fast(), 1);
        assert_eq!(a.wrap_fingerprint().len(), 32);
        assert_ne!(a.wrap_fingerprint(), b.wrap_fingerprint());
        assert!(!a.different_vault(&b));
    }
}
