//! Where HushJot Sync lives.
//!
//! The defaults point at production. The Firebase project id and web API key
//! are **public by design** — the same values ship in the HushJot web app —
//! because authorisation lives entirely in the server's security rules, never
//! in the key. [`Config::emulator`] points everything at a local Firebase
//! emulator for tests.

/// The production Firebase project.
pub const DEFAULT_PROJECT_ID: &str = "hushjot";
/// The production web API key. Public by design; see the module notes.
pub const DEFAULT_API_KEY: &str = "AIzaSyDzR7lXm7zcxpfmTTQDxYs_qXusLKMSdNQ"; // gitleaks:allow (public by design, above)

/// Service endpoints and identifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Firebase project id.
    pub project_id: String,
    /// Firebase web API key.
    pub api_key: String,
    /// Identity Toolkit base, e.g. `https://identitytoolkit.googleapis.com/v1`.
    pub identity_toolkit: String,
    /// Secure Token base, e.g. `https://securetoken.googleapis.com/v1`.
    pub secure_token: String,
    /// Firestore REST root, e.g. `https://firestore.googleapis.com/v1`.
    pub firestore: String,
    /// Origin share links point at.
    pub share_origin: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            project_id: DEFAULT_PROJECT_ID.into(),
            api_key: DEFAULT_API_KEY.into(),
            identity_toolkit: "https://identitytoolkit.googleapis.com/v1".into(),
            secure_token: "https://securetoken.googleapis.com/v1".into(),
            firestore: "https://firestore.googleapis.com/v1".into(),
            share_origin: hushjot_core::share::DEFAULT_SHARE_ORIGIN.into(),
        }
    }
}

impl Config {
    /// A local Firebase emulator: `auth_host` like `127.0.0.1:9099`,
    /// `firestore_host` like `127.0.0.1:8080`.
    pub fn emulator(project_id: &str, auth_host: &str, firestore_host: &str) -> Self {
        Self {
            project_id: project_id.into(),
            api_key: "fake-api-key".into(),
            identity_toolkit: format!("http://{auth_host}/identitytoolkit.googleapis.com/v1"),
            secure_token: format!("http://{auth_host}/securetoken.googleapis.com/v1"),
            firestore: format!("http://{firestore_host}/v1"),
            share_origin: "http://localhost:5173".into(),
        }
    }

    /// The Firestore documents root for this project.
    pub fn documents_url(&self) -> String {
        format!("{}/projects/{}/databases/(default)/documents", self.firestore, self.project_id)
    }

    /// The resource name of a document, for `referenceValue` and writes.
    pub fn document_name(&self, relative: &str) -> String {
        format!("projects/{}/databases/(default)/documents/{relative}", self.project_id)
    }
}
