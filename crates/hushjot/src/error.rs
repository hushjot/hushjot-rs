//! One error type for the whole SDK.
//!
//! Each variant is a different instruction to the embedding app, which is why
//! there are several that look alike. `NotApproved` and `Paused` arrive from the
//! server as the same refusal; telling a paying user their plan lapsed during a
//! maintenance pause would send them off to buy a second one, so the SDK makes
//! one extra request to tell the two apart before it reports either.

use hushjot_core::key_record::UnlockError;
use hushjot_core::share::ShareError;
use hushjot_core::{DecryptError, FormatError};

/// Where to send someone whose account has no plan.
pub const PLANS_URL: &str = "https://hushjot.com/account";

/// Everything that can go wrong.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Email and password do not match an account, or the account is disabled
    /// or rate-limited. The message is safe to show.
    #[error("{0}")]
    SignIn(String),
    /// The saved session is no longer valid; ask for the password again.
    #[error("The saved session is no longer valid. Sign in again.")]
    SessionExpired,
    /// The passphrase or recovery code is wrong.
    #[error("The passphrase or recovery code is incorrect.")]
    WrongSecret,
    /// The account has no vault yet. Vaults are created in the HushJot app,
    /// which issues the recovery code.
    #[error("This account has no HushJot vault yet. Open the HushJot app once to create it, then sign in here.")]
    NoVault,
    /// The account was reset elsewhere and now has a different vault. Notes in
    /// this local store were sealed under the old key and must not be pushed;
    /// wipe the store (`HushJot::wipe_local_data`) and sign in again.
    #[error("This HushJot account was reset on another device. Local data here belongs to the old vault.")]
    VaultReset,
    /// The local store was bound to a different account.
    #[error("This local store belongs to a different HushJot account.")]
    StoreBelongsToAnotherAccount,
    /// The account is valid but has no plan or invitation to sync. Point the
    /// user at [`PLANS_URL`].
    #[error("This account doesn't have HushJot Sync. Get a plan at {PLANS_URL}.")]
    NotApproved,
    /// The service is refusing every client for now (maintenance). Nothing is
    /// wrong with the account; work stays queued.
    #[error("HushJot Sync is temporarily paused. Your changes are kept and will sync when it resumes.")]
    Paused,
    /// Another device changed the note first.
    #[error("Note {note_id} changed on another device.")]
    RevisionConflict {
        /// The note.
        note_id: String,
    },
    /// The account has used its daily pushes. Retry after `retry_at`.
    #[error(
        "This account has synced the most changes a day allows. Changes are kept and sync once the day rolls over."
    )]
    PushLimit {
        /// When a new window may begin (epoch ms), if known.
        retry_at: Option<i64>,
    },
    /// The account's storage is full.
    #[error("Your HushJot storage is full.")]
    QuotaExceeded {
        /// The change in charged bytes the refused write would have made.
        delta: i64,
        /// Charged bytes in use, if read.
        used: Option<i64>,
        /// The allowance, if read.
        quota: Option<i64>,
    },
    /// This month's share links are used up.
    #[error("You have published the most links a month allows. Links already shared keep working.")]
    ShareLimit,
    /// One note is over the 900 KiB sealed limit.
    #[error("Note {note_id} is too large to sync ({bytes} bytes sealed).")]
    PayloadTooLarge {
        /// The note.
        note_id: String,
        /// Its sealed size.
        bytes: usize,
    },
    /// The network or the service is unavailable. Retry later.
    #[error("Could not reach HushJot Sync: {0}")]
    Transient(String),
    /// The server refused a write for a reason other than the above.
    #[error("The server refused the write: {0}")]
    WriteRefused(String),
    /// No note or folder with that id.
    #[error("Not found: {0}")]
    NotFound(String),
    /// The request is not valid (an empty name, a move into its own subtree…).
    #[error("{0}")]
    Invalid(String),
    /// Stored data could not be decrypted.
    #[error("Stored data could not be decrypted.")]
    Decrypt,
    /// Stored data is not in the expected format.
    #[error(transparent)]
    Format(#[from] FormatError),
    /// The local store failed.
    #[error("Local store error: {0}")]
    Store(String),
    /// The OS secret store failed.
    #[error("Secret store error: {0}")]
    Secrets(String),
    /// An unexpected response from the service.
    #[error("Unexpected response from HushJot Sync: {0}")]
    Protocol(String),
    /// The vault was locked or sync was stopped.
    #[error("The vault is locked.")]
    Locked,
}

impl Error {
    /// Whether retrying later, unchanged, might succeed.
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Transient(_) | Error::Paused | Error::PushLimit { .. })
    }
}

impl From<DecryptError> for Error {
    fn from(_: DecryptError) -> Self {
        Error::Decrypt
    }
}

impl From<UnlockError> for Error {
    fn from(error: UnlockError) -> Self {
        match error {
            UnlockError::WrongSecret(_) => Error::WrongSecret,
            UnlockError::Format(f) => Error::Format(f),
        }
    }
}

impl From<ShareError> for Error {
    fn from(error: ShareError) -> Self {
        match error {
            ShareError::TooLarge(bytes) => Error::PayloadTooLarge { note_id: "share".into(), bytes },
            ShareError::Decrypt(_) => Error::Decrypt,
            ShareError::Format(f) => Error::Format(f),
        }
    }
}

/// The SDK's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
