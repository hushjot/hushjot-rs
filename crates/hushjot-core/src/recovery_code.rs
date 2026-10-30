//! Recovery codes (the app's `src/core/crypto/recoveryCode.ts`).
//!
//! 160 bits from a CSPRNG in Crockford base32, shown as eight groups of four.
//! This crate never issues one — the app does, once, when it creates a vault —
//! but a client that offers "unlock with recovery code" wants to validate what
//! the user typed before spending a round trip on it.

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const ENTROPY_BYTES: usize = 20;
const GROUP: usize = 4;

fn confusable(c: char) -> Option<char> {
    match c {
        'O' | 'o' => Some('0'),
        'I' | 'i' | 'L' | 'l' => Some('1'),
        'U' | 'u' => Some('V'),
        _ => None,
    }
}

/// Canonical form for *validation and display*: separators dropped,
/// confusable letters mapped to the characters they look like, uppercased.
///
/// Not the form the key is derived from — that is
/// [`crate::kdf::normalize_code`], which maps no confusables.
pub fn canonicalize(code: &str) -> String {
    let mut out = String::new();
    for c in code.chars() {
        if matches!(c, '-' | ' ' | '\t' | '\n') {
            continue;
        }
        match confusable(c) {
            Some(mapped) => out.push(mapped),
            None => out.extend(c.to_uppercase()),
        }
    }
    out
}

/// Group a code as `XXXX-XXXX-…`.
pub fn format(code: &str) -> String {
    let clean: Vec<char> = canonicalize(code).chars().collect();
    clean.chunks(GROUP).map(|chunk| chunk.iter().collect::<String>()).collect::<Vec<_>>().join("-")
}

/// Whether `code` has the shape of a recovery code: 32 characters of the
/// alphabet once canonicalised.
pub fn looks_like_recovery_code(code: &str) -> bool {
    let clean = canonicalize(code);
    clean.chars().count() == (ENTROPY_BYTES * 8).div_ceil(5) && clean.bytes().all(|b| ALPHABET.contains(&b))
}

/// Encode bytes as the app's MSB-first Crockford base32. Exposed for tests.
#[doc(hidden)]
pub fn encode_base32(bytes: &[u8]) -> String {
    let mut bits = 0u32;
    let mut value = 0u32;
    let mut out = String::new();
    for &byte in bytes {
        value = (value << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            out.push(ALPHABET[((value >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
        value &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ALPHABET[((value << (5 - bits)) & 31) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contract_code_looks_like_a_code() {
        assert!(looks_like_recovery_code("K7Q4-9XMV-2ZWR-8DHC-6BTN-4YFJ-3GKP-1SAE"));
        assert!(looks_like_recovery_code("k7q4 9xmv 2zwr 8dhc 6btn 4yfj 3gkp 1sae"));
        assert!(!looks_like_recovery_code("K7Q4-9XMV"));
        assert!(!looks_like_recovery_code("K7Q4-9XMV-2ZWR-8DHC-6BTN-4YFJ-3GKP-1SA!"));
    }

    #[test]
    fn confusables_map_for_display_only() {
        assert_eq!(canonicalize("o-I-l-u"), "011V");
        assert_eq!(format("abcdefgh"), "ABCD-EFGH");
    }

    #[test]
    fn base32_matches_the_apps_encoder() {
        assert_eq!(encode_base32(&[0xff]), "ZW");
        assert_eq!(encode_base32(&[0; 20]).len(), 32);
        assert_eq!(encode_base32(&[]), "");
    }
}
