//! The storage and push meter (the app's `src/core/sync/usage.ts`).
//!
//! Every write to a note, folder, version or share is committed **together
//! with** a merge into `users/{uid}/usage/current`, and the server's rules
//! refuse the commit unless the meter moved by exactly the right amount:
//!
//! - `bytes` moves by [`meter_delta`]`(new, old)` — the change in stored
//!   ciphertext, each document charged at least [`MIN_CHARGED_BYTES`];
//! - `pn` counts the commit against [`DAILY_PUSH_LIMIT`] pushes in a window the
//!   **server's** clock opens (`ws`);
//! - `hist.{YYYYMM}` and `shares.{YYYYMM}` meter versions and share links per
//!   calendar month, because those expire by TTL and could only ever grow a
//!   stored total.
//!
//! A client never decides from its own clock that a day has passed. It always
//! increments `pn`; only after a refusal whose meter read shows the window full
//! does it send a reset, which the server takes or refuses.

use chrono::{DateTime, Datelike};

/// The meter document, relative to `users/{uid}`.
pub const USAGE_DOC: &str = "usage/current";
/// The storage every plan includes, and what the rules assume when the
/// entitlement names none.
pub const BASE_STORAGE_BYTES: i64 = 1024 * 1024 * 1024;
/// History uploads allowed per calendar month, in charged bytes.
pub const HISTORY_BYTES_PER_MONTH: i64 = 500 * 1000 * 1000;
/// Share links that may be published per calendar month.
pub const SHARES_PER_MONTH: i64 = 100;
/// The least one stored document counts for.
pub const MIN_CHARGED_BYTES: i64 = 1024;
/// Metered commits allowed per push window.
pub const DAILY_PUSH_LIMIT: i64 = 20_000;
/// The shortest a push window lasts, by the server's clock.
pub const PUSH_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
/// The largest sealed note (meta plus body) the rules accept, exclusive.
pub const MAX_PAYLOAD_BYTES: usize = 900 * 1024;
/// The largest sealed folder the rules accept, exclusive.
pub const MAX_FOLDER_BYTES: usize = 16 * 1024;

/// The monthly bucket a write falls in: `YYYYMM`, UTC.
pub fn usage_month(ms: i64) -> String {
    let date = DateTime::from_timestamp_millis(ms).unwrap_or_default();
    format!("{:04}{:02}", date.year(), date.month())
}

/// A note's raw stored size: `meta` plus `body`.
pub fn counted_note_bytes(meta: Option<&[u8]>, body: Option<&[u8]>) -> i64 {
    (meta.map_or(0, <[u8]>::len) + body.map_or(0, <[u8]>::len)) as i64
}

/// What a document of `size` raw bytes is charged; 0 means not stored.
pub fn charged_bytes(size: i64) -> i64 {
    if size <= 0 {
        0
    } else {
        size.max(MIN_CHARGED_BYTES)
    }
}

/// How far a write moves `bytes`: the new charge minus the replaced one.
pub fn meter_delta(new_size: i64, old_size: i64) -> i64 {
    charged_bytes(new_size) - charged_bytes(old_size)
}

/// The account's meter, as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Charged bytes stored now.
    pub bytes: i64,
    /// The allowance.
    pub quota: i64,
    /// History uploaded this month, charged bytes.
    pub history_this_month: i64,
    /// Links published this month.
    pub shares_this_month: i64,
    /// When the current push window began (server clock, ms).
    pub window_start: Option<i64>,
    /// Pushes made in that window.
    pub pushes_in_window: i64,
}

impl Usage {
    /// Whether a refusal is explained by the push window being full.
    pub fn push_window_full(&self) -> bool {
        self.pushes_in_window >= DAILY_PUSH_LIMIT
    }

    /// When a new window may begin, if the meter says.
    pub fn window_retry_at(&self) -> Option<i64> {
        self.window_start.map(|start| start + PUSH_WINDOW_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_have_a_floor_and_nothing_is_free() {
        assert_eq!(charged_bytes(0), 0);
        assert_eq!(charged_bytes(1), 1024);
        assert_eq!(charged_bytes(5000), 5000);
        assert_eq!(meter_delta(10, 0), 1024);
        assert_eq!(meter_delta(2000, 10), 976);
        assert_eq!(meter_delta(10, 2000), -976);
        assert_eq!(meter_delta(10, 20), 0);
    }

    #[test]
    fn months_are_utc() {
        assert_eq!(usage_month(0), "197001");
        // 2026-10-31T23:30:00Z is still October in UTC whatever the local zone.
        assert_eq!(usage_month(1_793_489_400_000), "202610");
    }

    #[test]
    fn note_sizes_count_meta_and_body() {
        assert_eq!(counted_note_bytes(Some(&[0; 40]), None), 40);
        assert_eq!(counted_note_bytes(Some(&[0; 40]), Some(&[0; 30])), 70);
    }

    #[test]
    fn windows_report_fullness_and_retry_time() {
        let usage = Usage { pushes_in_window: DAILY_PUSH_LIMIT, window_start: Some(10), ..Usage::default() };
        assert!(usage.push_window_full());
        assert_eq!(usage.window_retry_at(), Some(10 + PUSH_WINDOW_MS));
    }
}
