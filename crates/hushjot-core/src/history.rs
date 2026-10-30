//! Note history: version ids, capture policy and retention (the app's
//! `src/core/history/record.ts`, `policy.ts` and `retention.ts`).
//!
//! A version is a byte-for-byte copy of a note's stored `meta` and `body`
//! ciphertext, taken just before something replaced it — never re-sealed, so
//! taking one needs no key. Its id is the body's nonce ([`seal_id_of`]): two
//! devices capturing the same stored sealing produce the same id and collapse
//! to one record without reading the plaintext.
//!
//! **When**: the first save after [`SESSION_GAP_MS`] of quiet keeps the text it
//! overwrites, and a long session is cut every [`CHECKPOINT_MS`]
//! ([`CaptureTracker`]). Deletes and restores are always captured.
//!
//! **How long**: decided at capture ([`tier_for`]) — the first capture in a UTC
//! day is kept 30 days, the first in a UTC hour 7 days, anything else 24 hours;
//! a delete's final text always 30 days. The server enforces at most 31 days.

use std::collections::HashMap;

use crate::bytes::to_hex;
use crate::envelope::EMPTY_SEAL_BYTES;

/// An hour, in ms.
pub const HOUR_MS: i64 = 60 * 60_000;
/// A day, in ms.
pub const DAY_MS: i64 = 24 * HOUR_MS;
/// The shortest retention tier.
pub const TIER_DAY_MS: i64 = DAY_MS;
/// The middle retention tier.
pub const TIER_WEEK_MS: i64 = 7 * DAY_MS;
/// The longest retention tier.
pub const TIER_MONTH_MS: i64 = 30 * DAY_MS;
/// Quiet time that starts a new editing session.
pub const SESSION_GAP_MS: i64 = 5 * 60_000;
/// How often a long session is checkpointed.
pub const CHECKPOINT_MS: i64 = 10 * 60_000;

/// Why a version was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VersionReason {
    /// Before an edit replaced the text.
    Edit,
    /// The note's final text before it was deleted.
    Delete,
    /// Before a restore replaced the text.
    Restore,
}

impl VersionReason {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            VersionReason::Edit => "edit",
            VersionReason::Delete => "delete",
            VersionReason::Restore => "restore",
        }
    }

    /// Parse the stored spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "edit" => Some(VersionReason::Edit),
            "delete" => Some(VersionReason::Delete),
            "restore" => Some(VersionReason::Restore),
            _ => None,
        }
    }
}

/// The body's nonce as 24 lowercase hex characters, or `None` for something
/// that is not an envelope.
pub fn seal_id_of(body: &[u8]) -> Option<String> {
    (body.len() >= EMPTY_SEAL_BYTES).then(|| to_hex(&body[2..14]))
}

/// Start of the UTC hour containing `at`.
pub fn utc_hour_start(at: i64) -> i64 {
    at.div_euclid(HOUR_MS) * HOUR_MS
}

/// Start of the UTC day containing `at`.
pub fn utc_day_start(at: i64) -> i64 {
    at.div_euclid(DAY_MS) * DAY_MS
}

/// How long to keep a version, given whether this note already has a capture
/// in the same UTC day and the same UTC hour.
pub fn tier_for(reason: VersionReason, has_in_day: bool, has_in_hour: bool) -> i64 {
    if reason == VersionReason::Delete || !has_in_day {
        TIER_MONTH_MS
    } else if !has_in_hour {
        TIER_WEEK_MS
    } else {
        TIER_DAY_MS
    }
}

#[derive(Debug, Clone, Copy)]
struct Seen {
    last_persist_at: i64,
    last_capture_at: i64,
}

/// Decides, per save, whether the text being replaced should be kept.
#[derive(Debug, Clone)]
pub struct CaptureTracker {
    seen: HashMap<String, Seen>,
    session_gap_ms: i64,
    checkpoint_ms: i64,
}

impl Default for CaptureTracker {
    fn default() -> Self {
        Self::new(SESSION_GAP_MS, CHECKPOINT_MS)
    }
}

impl CaptureTracker {
    /// A tracker with custom timings.
    pub fn new(session_gap_ms: i64, checkpoint_ms: i64) -> Self {
        Self { seen: HashMap::new(), session_gap_ms, checkpoint_ms }
    }

    /// Record a save of `note_id` at `now`; true when the replaced text should
    /// be kept as a version.
    pub fn observe(&mut self, note_id: &str, now: i64) -> bool {
        let seen = self.seen.get(note_id).copied();
        let capture = match seen {
            None => true,
            Some(s) => {
                now - s.last_persist_at >= self.session_gap_ms
                    || now - s.last_capture_at >= self.checkpoint_ms
                    // A clock that went backwards: start over rather than never capture again.
                    || now < s.last_persist_at
            }
        };
        let last_capture_at = match seen {
            Some(s) if !capture => s.last_capture_at,
            _ => now,
        };
        self.seen.insert(note_id.to_string(), Seen { last_persist_at: now, last_capture_at });
        capture
    }

    /// Forget everything, e.g. on lock.
    pub fn clear(&mut self) {
        self.seen.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_ids_are_the_nonce_in_hex() {
        let mut body = vec![2u8, 1];
        body.extend(0u8..12);
        body.extend([0u8; 16]);
        assert_eq!(seal_id_of(&body).unwrap(), "000102030405060708090a0b");
        assert_eq!(seal_id_of(&body[..29]), None);
    }

    #[test]
    fn tiers_follow_the_first_capture_in_each_bucket() {
        assert_eq!(tier_for(VersionReason::Delete, true, true), TIER_MONTH_MS);
        assert_eq!(tier_for(VersionReason::Edit, false, false), TIER_MONTH_MS);
        assert_eq!(tier_for(VersionReason::Edit, true, false), TIER_WEEK_MS);
        assert_eq!(tier_for(VersionReason::Restore, true, true), TIER_DAY_MS);
        assert_eq!(utc_hour_start(HOUR_MS + 5), HOUR_MS);
        assert_eq!(utc_day_start(DAY_MS * 3 + 7), DAY_MS * 3);
    }

    #[test]
    fn sessions_and_checkpoints() {
        let mut t = CaptureTracker::default();
        assert!(t.observe("n", 0));
        assert!(!t.observe("n", 60_000));
        assert!(!t.observe("n", 4 * 60_000));
        assert!(t.observe("n", 10 * 60_000)); // checkpoint
        assert!(!t.observe("n", 11 * 60_000));
        assert!(t.observe("n", 17 * 60_000)); // session gap
        assert!(t.observe("n", 16 * 60_000)); // clock went backwards
        assert!(t.observe("other", 16 * 60_000));
        t.clear();
        assert!(t.observe("n", 16 * 60_000 + 1));
    }
}
