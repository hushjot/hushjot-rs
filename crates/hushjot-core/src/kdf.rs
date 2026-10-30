//! Key derivation (the app's `src/core/crypto/kdf.ts`).
//!
//! Two derivations, deliberately different, because the inputs are different:
//!
//! - **Passphrase → KEK: Argon2id.** A human-chosen passphrase has low entropy,
//!   so guessing it must be expensive, and memory-hard denies an attacker the
//!   GPU advantage. The parameters live in the key record, never in code, so an
//!   existing vault keeps whatever it was created with.
//! - **Recovery code → KEK: HKDF-SHA-256.** The code is 160 random bits, so
//!   stretching it buys nothing.
//!
//! Argon2id at the default 64 MiB / t=3 costs roughly 100 ms on a laptop. Call
//! [`derive_kek_from_passphrase`] off your UI thread (`spawn_blocking` in
//! tokio); the `hushjot` crate does.

use argon2::{Algorithm, Argon2, Params, Version};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroize;

use crate::envelope::Key;
use crate::error::FormatError;

/// The HKDF `info` label. Frozen at the product's original name: changing it
/// would invalidate every recovery code ever issued.
pub const RECOVERY_KEK_INFO: &str = "teksto/recovery-kek/v1";

/// Argon2id parameters as a key record stores them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argon2Params {
    /// Must be `"argon2id"`; anything else is refused.
    pub name: String,
    /// Memory cost in KiB.
    pub memory_kib: u32,
    /// Time cost.
    pub iterations: u32,
    /// Lanes.
    pub parallelism: u32,
}

impl Default for Argon2Params {
    /// The app's defaults for a new vault: 64 MiB, 3 iterations, 1 lane.
    fn default() -> Self {
        Self { name: "argon2id".into(), memory_kib: 65_536, iterations: 3, parallelism: 1 }
    }
}

/// Derive a key-encryption key from a passphrase.
///
/// The passphrase is used exactly as given — UTF-8, no trimming and no Unicode
/// normalisation — because that is what the app does. Normalising here would
/// lock out anyone whose passphrase is not already in the normalised form.
pub fn derive_kek_from_passphrase(passphrase: &str, salt: &[u8], params: &Argon2Params) -> Result<Key, FormatError> {
    if params.name != "argon2id" {
        return Err(FormatError::new(format!("unsupported KDF \"{}\"", params.name)));
    }
    let argon_params = Params::new(params.memory_kib, params.iterations, params.parallelism, Some(32))
        .map_err(|_| FormatError::new("Argon2id parameters out of range"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);
    let mut out = [0u8; 32];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|_| FormatError::new("Argon2id rejected its inputs"))?;
    let key = Key::from_bytes(out);
    out.zeroize();
    Ok(key)
}

/// Recovery codes are derived in a canonical form: separators and whitespace
/// removed, uppercased.
///
/// This is the app's `normalizeCode`, and it deliberately does **not** apply
/// the confusable mapping (`O`→`0`, `I`/`L`→`1`, `U`→`V`) that
/// [`crate::recovery_code::canonicalize`] applies. Mapping here would derive a
/// different key from the one the app derives for the same typed code.
pub fn normalize_code(code: &str) -> String {
    code.chars().filter(|&c| c != '-' && !crate::js::is_js_whitespace(c)).collect::<String>().to_uppercase()
}

/// Derive a key-encryption key from a recovery code.
pub fn derive_kek_from_recovery_code(code: &str, salt: &[u8]) -> Key {
    let mut material = normalize_code(code).into_bytes();
    let hk = Hkdf::<Sha256>::new(Some(salt), &material);
    let mut out = [0u8; 32];
    hk.expand(RECOVERY_KEK_INFO.as_bytes(), &mut out).expect("32 bytes is a valid HKDF-SHA-256 output length");
    material.zeroize();
    let key = Key::from_bytes(out);
    out.zeroize();
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalising_strips_separators_and_uppercases_without_mapping_confusables() {
        assert_eq!(normalize_code("k7q4-9xmv 2zwr\t8dhc"), "K7Q49XMV2ZWR8DHC");
        assert_eq!(normalize_code("o-i-l-u"), "OILU");
    }

    #[test]
    fn unsupported_kdfs_are_refused() {
        let params = Argon2Params { name: "scrypt".into(), ..Argon2Params::default() };
        assert!(derive_kek_from_passphrase("x", &[0; 32], &params).is_err());
    }

    #[test]
    fn derivations_are_deterministic_and_salted() {
        let params = Argon2Params { memory_kib: 64, iterations: 1, ..Argon2Params::default() };
        let a = derive_kek_from_passphrase("pw", &[1; 16], &params).unwrap();
        let b = derive_kek_from_passphrase("pw", &[1; 16], &params).unwrap();
        let c = derive_kek_from_passphrase("pw", &[2; 16], &params).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(derive_kek_from_recovery_code("ab-cd", &[3; 32]), derive_kek_from_recovery_code("ABCD", &[3; 32]));
    }
}
