//! Share links (the app's `src/core/share/`).
//!
//! Publishing a note mints a **fresh random 256-bit key — never the vault
//! key** — seals a snapshot of the note under it, and stores the ciphertext at
//! a public, unguessable `shares/{id}`. The link is
//! `https://hushjot.app/s/#s1.<id>.<key>`: everything is in the fragment, which
//! browsers never send, so neither the server nor the host's logs ever see the
//! key. Links expire after at most 31 days.

use serde::{Deserialize, Serialize};

use crate::bytes::{from_base64_url, random_bytes, to_base64_url};
use crate::envelope::{open_text, seal_text, Key};
use crate::error::{DecryptError, FormatError};

/// The fragment's version prefix.
pub const SHARE_PREFIX: &str = "s1";
/// Random bytes in a share id.
pub const SHARE_ID_BYTES: usize = 16;
/// Characters in a share id (base64url).
pub const SHARE_ID_CHARS: usize = 22;
/// Characters in a share key (base64url).
pub const SHARE_KEY_CHARS: usize = 43;
/// Where the viewer lives under the app's origin.
pub const VIEWER_PATH: &str = "/s/";
/// The app origin share links point at by default.
pub const DEFAULT_SHARE_ORIGIN: &str = "https://hushjot.app";
/// The share payload format version.
pub const SHARE_PAYLOAD_VERSION: u32 = 1;
/// The largest sealed share.
pub const MAX_SHARE_BYTES: usize = 900 * 1024;
/// A minute, in ms.
pub const MINUTE_MS: i64 = 60_000;
/// The longest a share may live.
pub const SHARE_MAX_MS: i64 = 31 * crate::history::DAY_MS;

/// A share's id and key, as the link carries them.
#[derive(Clone, PartialEq, Eq)]
pub struct ShareRef {
    /// 22 characters of base64url.
    pub id: String,
    /// The raw key, 43 characters of base64url. Never log it.
    pub key: String,
}

impl std::fmt::Debug for ShareRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShareRef").field("id", &self.id).field("key", &"<redacted>").finish()
    }
}

fn is_b64url(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `value` is a well-formed share id.
pub fn is_share_id(value: &str) -> bool {
    is_b64url(value, SHARE_ID_CHARS)
}

/// A fresh share id.
pub fn new_share_id() -> String {
    to_base64_url(&random_bytes(SHARE_ID_BYTES))
}

/// A fresh share key and its encoded form.
pub fn new_share_key() -> (Key, String) {
    let key = Key::generate();
    let encoded = to_base64_url(key.expose_bytes());
    (key, encoded)
}

/// Decode a share key from a link.
pub fn import_share_key(encoded: &str) -> Result<Key, FormatError> {
    let raw = from_base64_url(encoded).map_err(|_| FormatError::new("share key is not base64url"))?;
    Key::from_slice(&raw).ok_or_else(|| FormatError::new("share key is not 256 bits"))
}

/// `s1.<id>.<key>`.
pub fn build_fragment(r: &ShareRef) -> String {
    format!("{SHARE_PREFIX}.{}.{}", r.id, r.key)
}

/// The full link under `origin`.
pub fn build_share_url(origin: &str, r: &ShareRef) -> String {
    format!("{}{VIEWER_PATH}#{}", origin.trim_end_matches('/'), build_fragment(r))
}

/// Parse a fragment (with or without `#`), or a whole URL ending in one.
pub fn parse_fragment(hash: &str) -> Option<ShareRef> {
    let raw = match hash.find('#') {
        Some(i) => &hash[i + 1..],
        None => hash,
    };
    let mut parts = raw.split('.');
    let (prefix, id, key) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || prefix != SHARE_PREFIX {
        return None;
    }
    if !is_share_id(id) || !is_b64url(key, SHARE_KEY_CHARS) {
        return None;
    }
    Some(ShareRef { id: id.to_string(), key: key.to_string() })
}

/// What a share holds: a snapshot of a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharePayload {
    /// The note's title.
    pub title: String,
    /// The note's Markdown.
    pub body: String,
    /// When the note was last edited.
    pub updated_at: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PayloadJson<'a> {
    v: u32,
    title: &'a str,
    body: &'a str,
    updated_at: i64,
}

/// Why a share could not be sealed or opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShareError {
    /// The sealed share would exceed [`MAX_SHARE_BYTES`].
    #[error("a share of {0} bytes exceeds the {MAX_SHARE_BYTES} byte limit")]
    TooLarge(usize),
    /// The key does not open it.
    #[error(transparent)]
    Decrypt(#[from] DecryptError),
    /// It opened but is not a share payload this crate understands.
    #[error(transparent)]
    Format(#[from] FormatError),
}

/// Seal a payload under a share key (an unbound, version-1 envelope).
pub fn seal_share(key: &Key, payload: &SharePayload) -> Result<Vec<u8>, ShareError> {
    let json = serde_json::to_string(&PayloadJson {
        v: SHARE_PAYLOAD_VERSION,
        title: &payload.title,
        body: &payload.body,
        updated_at: payload.updated_at,
    })
    .expect("share serialisation cannot fail");
    let sealed = seal_text(key, &json, None);
    if sealed.len() > MAX_SHARE_BYTES {
        return Err(ShareError::TooLarge(sealed.len()));
    }
    Ok(sealed)
}

/// Open a sealed share.
pub fn open_share(key: &Key, sealed: &[u8]) -> Result<SharePayload, ShareError> {
    let text = open_text(key, sealed, None)?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| FormatError::new("share payload is not JSON"))?;
    if !value.is_object() {
        return Err(FormatError::new("share payload is not an object").into());
    }
    if value.get("v").and_then(serde_json::Value::as_u64) != Some(u64::from(SHARE_PAYLOAD_VERSION)) {
        return Err(FormatError::new("unsupported share payload version").into());
    }
    let body = value
        .get("body")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| FormatError::new("share payload has no body"))?;
    Ok(SharePayload {
        title: value.get("title").and_then(serde_json::Value::as_str).unwrap_or("").to_string(),
        body: body.to_string(),
        updated_at: value
            .get("updatedAt")
            .and_then(serde_json::Value::as_number)
            .and_then(|n| n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)))
            .unwrap_or(0),
    })
}

/// How long a new link lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShareExpiry {
    /// One hour.
    Hour,
    /// One day.
    Day,
    /// Seven days. The default.
    #[default]
    Week,
    /// Thirty days.
    Month,
}

impl ShareExpiry {
    /// The tier's length.
    pub fn duration_ms(self) -> i64 {
        match self {
            ShareExpiry::Hour => crate::history::HOUR_MS,
            ShareExpiry::Day => crate::history::DAY_MS,
            ShareExpiry::Week => 7 * crate::history::DAY_MS,
            ShareExpiry::Month => 30 * crate::history::DAY_MS,
        }
    }

    /// When a link published at `now` expires: never past the server's 31-day
    /// limit, with a minute's margin for clock skew.
    pub fn expire_at(self, now: i64) -> i64 {
        now + self.duration_ms().min(SHARE_MAX_MS - MINUTE_MS)
    }
}

/// Human wording for the time a link has left: `"in 3 days"`, `"expired"`.
pub fn describe_remaining(expire_at: i64, now: i64) -> String {
    let left = expire_at - now;
    let plural = |n: i64, unit: &str| format!("in {n} {unit}{}", if n == 1 { "" } else { "s" });
    let round = |x: i64, unit: i64| ((x as f64) / (unit as f64)).round() as i64;
    if left <= 0 {
        "expired".into()
    } else if left >= crate::history::DAY_MS {
        plural(round(left, crate::history::DAY_MS), "day")
    } else if left >= crate::history::HOUR_MS {
        plural(round(left, crate::history::HOUR_MS), "hour")
    } else {
        plural(round(left, MINUTE_MS).max(1), "minute")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip() {
        let (_, key) = new_share_key();
        let r = ShareRef { id: new_share_id(), key };
        assert_eq!(r.id.len(), SHARE_ID_CHARS);
        assert_eq!(r.key.len(), SHARE_KEY_CHARS);
        let url = build_share_url("https://hushjot.app/", &r);
        assert!(url.starts_with("https://hushjot.app/s/#s1."));
        assert_eq!(parse_fragment(&url), Some(r.clone()));
        assert_eq!(parse_fragment(&format!("#{}", build_fragment(&r))), Some(r.clone()));
        assert_eq!(parse_fragment(&build_fragment(&r)), Some(r));
    }

    #[test]
    fn malformed_fragments_are_refused() {
        assert_eq!(parse_fragment("s2.aaaaaaaaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"), None);
        assert_eq!(parse_fragment("s1.short.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"), None);
        assert_eq!(parse_fragment("s1.aaaaaaaaaaaaaaaaaaaaaa.bbb"), None);
        assert_eq!(parse_fragment("c1.anything"), None);
    }

    #[test]
    fn payloads_round_trip_and_wrong_keys_fail() {
        let (key, encoded) = new_share_key();
        let payload = SharePayload { title: "T".into(), body: "# T\nbody".into(), updated_at: 42 };
        let sealed = seal_share(&key, &payload).unwrap();
        assert_eq!(sealed[0], 1, "share payloads are unbound");
        assert_eq!(open_share(&import_share_key(&encoded).unwrap(), &sealed).unwrap(), payload);
        assert!(matches!(open_share(&Key::generate(), &sealed), Err(ShareError::Decrypt(_))));
    }

    #[test]
    fn oversized_shares_are_refused() {
        let (key, _) = new_share_key();
        let payload = SharePayload { title: String::new(), body: "x".repeat(MAX_SHARE_BYTES), updated_at: 0 };
        assert!(matches!(seal_share(&key, &payload), Err(ShareError::TooLarge(_))));
    }

    #[test]
    fn expiry_is_capped_and_described() {
        assert_eq!(ShareExpiry::Week.expire_at(0), 7 * crate::history::DAY_MS);
        assert_eq!(ShareExpiry::Month.expire_at(0), 30 * crate::history::DAY_MS);
        assert_eq!(describe_remaining(0, 1), "expired");
        assert_eq!(describe_remaining(crate::history::DAY_MS, 0), "in 1 day");
        assert_eq!(describe_remaining(3 * crate::history::HOUR_MS, 0), "in 3 hours");
        assert_eq!(describe_remaining(10_000, 0), "in 1 minute");
    }

    #[test]
    fn share_refs_do_not_print_their_key() {
        let r = ShareRef { id: "i".into(), key: "secret".into() };
        assert!(!format!("{r:?}").contains("secret"));
    }
}
