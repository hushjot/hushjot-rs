//! # hushjot-core
//!
//! The HushJot note format, with no I/O: everything two HushJot clients must
//! agree on byte for byte, ported from the app's `src/core/` and checked
//! against the app's frozen contract
//! ([`contract/format-vectors.json`](https://github.com/hushjot/hushjot-rs/blob/main/contract/format-vectors.json)).
//!
//! Most applications want the [`hushjot`](https://docs.rs/hushjot) crate, which
//! adds sign-in, sync and a local store on top of this one. Reach for this crate
//! directly when you need the format alone — to open an exported blob, to build
//! a tool over the wire format, or to test against it.
//!
//! ## Why a port must be exact
//!
//! A second implementation of `deriveTitle` or of the meta JSON would work
//! perfectly and disagree silently: the same note would get two titles on two
//! devices, or a re-sealed meta would drop a field another client relies on.
//! So every derivation here is a line-for-line port, JavaScript string
//! semantics included (UTF-16 lengths, JS whitespace), and the contract tests
//! in `tests/contract.rs` fail the build if any of it drifts.
//!
//! ## Modules
//!
//! - [`envelope`] — AES-256-GCM sealing with the versioned header and record binding.
//! - [`kdf`], [`key_record`], [`recovery_code`] — how a passphrase or recovery
//!   code reaches the vault's data-encryption key.
//! - [`meta`] — the sealed JSON shapes of a note's metadata, a folder and a share.
//! - [`notes`], [`folders`], [`tags`] — titles, excerpts, list order, the folder
//!   tree and inline `#tags`.
//! - [`search`] — the query grammar and the in-memory index.
//! - [`usage`], [`history`], [`share`] — metering arithmetic, version retention
//!   and share links.
#![deny(missing_docs, unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod bytes;
pub mod collate;
pub mod envelope;
pub mod error;
pub mod folders;
pub mod history;
pub mod js;
pub mod kdf;
pub mod key_record;
pub mod meta;
pub mod notes;
pub mod recovery_code;
pub mod search;
pub mod share;
pub mod tags;
pub mod usage;

pub use envelope::{open, open_text, record_context, seal, seal_text, Key, RecordKind};
pub use error::{DecryptError, FormatError, WrongSecretError};
pub use key_record::{KeyRecord, SerializedKeyRecord};
pub use meta::{FolderPayload, NoteMetaPayload};

/// The stored-byte format this crate reads and writes. Moves only when the
/// app's `FORMAT_VERSION` does, which is a coordinated release of every client.
pub const FORMAT_VERSION: u32 = 2;

/// The app version whose `src/core/` this crate was last checked against.
pub const PORTED_FROM_CORE_VERSION: &str = "0.4.0";
