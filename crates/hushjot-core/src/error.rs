//! The format's error types.
//!
//! Deliberately few and deliberately vague where it matters. A wrong key, a
//! tampered envelope and a record moved into another record's slot all surface
//! as the same [`DecryptError`], because telling them apart would hand an
//! attacker an oracle and gives a user nothing they can act on.

use std::fmt;

/// An envelope failed to open: wrong key, tampered bytes, wrong record
/// context, or not an envelope at all. Indistinguishable by design.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("authentication failed")]
pub struct DecryptError;

/// The passphrase or recovery code did not unwrap the vault key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("The passphrase or recovery code is incorrect.")]
pub struct WrongSecretError;

/// Bytes or JSON that are not in the shape the format promises.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed: {message}")]
pub struct FormatError {
    message: String,
}

impl FormatError {
    /// A format error with a short, non-secret description.
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    /// The description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Debug for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FormatError({:?})", self.message)
    }
}
