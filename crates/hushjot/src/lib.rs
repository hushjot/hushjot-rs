//! # hushjot
//!
//! End-to-end encrypted [HushJot](https://hushjot.com) Sync for Rust apps.
//!
//! Your users sign in with their HushJot account, unlock with their HushJot
//! passphrase, and their notes stay in step with the HushJot web app, their
//! phone and every other HushJot client. Notes are sealed with AES-256-GCM on
//! the device; the server only ever stores ciphertext it cannot read.
//!
//! ```no_run
//! # #[cfg(feature = "sqlite-store")]
//! # async fn demo() -> hushjot::Result<()> {
//! use hushjot::{HushJot, SqliteStore, SyncOptions, UnlockOptions};
//!
//! let hj = HushJot::builder()
//!     .store(SqliteStore::open("hushjot.db")?)   // ciphertext only
//!     .build()?;
//!
//! let account = hj.sign_in("me@example.com", "account password").await?;
//! let vault = account.unlock("my passphrase", UnlockOptions::default()).await?;
//! vault.start_sync(SyncOptions::default());
//!
//! let id = vault.notes().create("# Groceries\n- oat milk", None)?;
//! vault.notes().update(&id, "# Groceries\n- oat milk\n- coffee")?;
//! for note in vault.notes().list()? {
//!     println!("{}  {}", note.id, note.title);
//! }
//! # Ok(()) }
//! ```
//!
//! ## What to read next
//!
//! - [The integration guide](https://github.com/hushjot/hushjot-rs/blob/main/docs/integration-guide.md) — the screens and
//!   messages an app needs (passphrase prompt, "needs a plan", conflict copies,
//!   pauses) and the rules for handling plaintext.
//! - [The mini-editor guide](https://github.com/hushjot/hushjot-rs/blob/main/docs/mini-editor.md) — a runnable terminal
//!   editor that exercises the whole SDK, with the call behind each command.
//! - [The protocol](https://github.com/hushjot/hushjot-rs/blob/main/docs/protocol.md) — the wire format and invariants, for
//!   contributors.
//! - [`Vault`] — everything an editor does with notes.
//! - [`SyncEvent`] — what to react to while syncing.
//!
//! ## Feature flags
//!
//! - `sqlite-store` (default) — [`SqliteStore`].
//! - `keyring` (default) — [`KeyringSecrets`], the OS keychain.
#![deny(missing_docs, unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod auth;
pub mod backend;
mod client;
pub mod codec;
pub mod config;
mod engine;
mod error;
pub mod firestore;
pub mod secrets;
pub mod store;
mod vault;

pub use client::{Account, HushJot, HushJotBuilder, UnlockOptions};
pub use config::Config;
pub use engine::{ceiling_for, SyncEvent, SyncOptions, SyncStatus};
pub use error::{Error, Result, PLANS_URL};
pub use secrets::{MemorySecrets, SecretStore};
pub use store::{MemoryStore, Store};
pub use vault::{
    DeletedNote, Folders, History, Note, NoteSummary, NoteVersion, Notes, PublishedShare, SearchResult, Shares, Vault,
    VersionInfo,
};

#[cfg(feature = "keyring")]
pub use secrets::KeyringSecrets;
#[cfg(feature = "sqlite-store")]
pub use store::SqliteStore;

/// The format crate, re-exported.
pub use hushjot_core as core;
pub use hushjot_core::folders::{Folder, FolderNode};
pub use hushjot_core::notes::{NoteSort, SortKey};
pub use hushjot_core::share::ShareExpiry;
