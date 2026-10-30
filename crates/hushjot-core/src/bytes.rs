//! Base64 in the two spellings the format uses, and randomness.
//!
//! Firestore's `bytesValue` and the key record use **standard base64 with
//! padding** (the app's `toBase64`). Share ids and share keys travel in a URL
//! fragment, so they use **base64url without padding** (`toBase64Url`). Mixing
//! the two up produces strings that look right and decode to nothing.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;

use crate::error::FormatError;

/// Standard, padded base64.
pub fn to_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Decode standard, padded base64.
pub fn from_base64(text: &str) -> Result<Vec<u8>, FormatError> {
    STANDARD.decode(text).map_err(|_| FormatError::new("not standard base64"))
}

/// base64url without padding.
pub fn to_base64_url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode base64url. Padding is tolerated, as the app's decoder tolerates it.
pub fn from_base64_url(text: &str) -> Result<Vec<u8>, FormatError> {
    URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).map_err(|_| FormatError::new("not base64url"))
}

/// `n` bytes from the operating system's CSPRNG.
///
/// # Panics
///
/// If the operating system cannot provide randomness. There is no safe
/// fallback for a nonce or a key, so this does not try to find one.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    getrandom::getrandom(&mut out).expect("the operating system's random number generator failed");
    out
}

/// Lowercase hex.
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_in_both_spellings() {
        let bytes = [0xfbu8, 0xff, 0x00, 0x10];
        assert_eq!(to_base64(&bytes), "+/8AEA==");
        assert_eq!(to_base64_url(&bytes), "-_8AEA");
        assert_eq!(from_base64("+/8AEA==").unwrap(), bytes);
        assert_eq!(from_base64_url("-_8AEA").unwrap(), bytes);
        assert_eq!(from_base64_url("-_8AEA==").unwrap(), bytes);
        assert!(from_base64("-_8AEA").is_err());
    }

    #[test]
    fn random_bytes_are_the_requested_length_and_vary() {
        assert_eq!(random_bytes(32).len(), 32);
        assert_ne!(random_bytes(16), random_bytes(16));
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(to_hex(&[0, 15, 255]), "000fff");
    }
}
