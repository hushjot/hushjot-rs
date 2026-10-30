//! Firebase Authentication over REST, without a Firebase SDK.
//!
//! HushJot accounts sign in with **email and password only** — the only
//! provider the project enables — and accounts are created on hushjot.com,
//! never by a client. Sign-in returns a short-lived ID token (an hour) and a
//! refresh token; [`TokenSource`] keeps the ID token fresh and the refresh
//! token is what an app saves to stay signed in. The password is never stored.

use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::error::{Error, Result};

/// A signed-in identity.
#[derive(Clone)]
pub struct Session {
    /// The Firebase uid.
    pub uid: String,
    /// The account email.
    pub email: String,
    /// The bearer token for Firestore.
    pub id_token: String,
    /// The long-lived refresh token. Treat as a secret.
    pub refresh_token: String,
    expires_at: Instant,
}

impl Session {
    /// A session rebuilt from the saved refresh token without the network.
    /// It is already expired, so [`TokenSource`] renews it on the first
    /// request; offline, that request fails as transient and sync says Offline.
    pub fn offline(uid: String, email: String, refresh_token: String) -> Self {
        Self { uid, email, id_token: String::new(), refresh_token, expires_at: Instant::now() }
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("uid", &self.uid).field("email", &self.email).finish_non_exhaustive()
    }
}

/// Identity Toolkit reports a wrong password and an unknown account with the
/// same code once email-enumeration protection is on, so the message names
/// both rather than guessing.
fn readable(code: &str) -> Error {
    match code {
        "INVALID_LOGIN_CREDENTIALS" | "EMAIL_NOT_FOUND" | "INVALID_PASSWORD" => {
            Error::SignIn("That email and password do not match an account.".into())
        }
        "USER_DISABLED" => Error::SignIn("That account has been disabled.".into()),
        c if c.starts_with("TOO_MANY_ATTEMPTS_TRY_LATER") => {
            Error::SignIn("Too many attempts. Try again later.".into())
        }
        "TOKEN_EXPIRED" | "USER_NOT_FOUND" | "INVALID_REFRESH_TOKEN" => Error::SessionExpired,
        other => Error::SignIn(other.to_string()),
    }
}

#[derive(Deserialize)]
struct IdentityError {
    error: Option<IdentityErrorBody>,
}

#[derive(Deserialize)]
struct IdentityErrorBody {
    message: Option<String>,
}

async fn failure(response: reqwest::Response) -> Error {
    let status = response.status();
    if status.is_server_error() || status.as_u16() == 429 {
        return Error::Transient(format!("sign-in service HTTP {status}"));
    }
    let code = response
        .json::<IdentityError>()
        .await
        .ok()
        .and_then(|e| e.error)
        .and_then(|e| e.message)
        .unwrap_or_else(|| format!("HTTP_{}", status.as_u16()));
    readable(&code)
}

fn expiry(expires_in: Option<&str>) -> Instant {
    let seconds = expires_in.and_then(|s| s.parse::<u64>().ok()).unwrap_or(3600);
    // Refresh five minutes early so a request never races the expiry.
    Instant::now() + Duration::from_secs(seconds.saturating_sub(300))
}

/// Exchange an email and password for a session.
pub async fn sign_in_with_password(
    http: &reqwest::Client,
    config: &Config,
    email: &str,
    password: &str,
) -> Result<Session> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Reply {
        id_token: String,
        refresh_token: String,
        local_id: String,
        email: Option<String>,
        expires_in: Option<String>,
    }
    let url =
        format!("{}/accounts:signInWithPassword?key={}", config.identity_toolkit, urlencoding::encode(&config.api_key));
    let response = http
        .post(url)
        .json(&serde_json::json!({ "email": email, "password": password, "returnSecureToken": true }))
        .send()
        .await
        .map_err(|e| Error::Transient(format!("could not reach the sign-in service: {e}")))?;
    if !response.status().is_success() {
        return Err(failure(response).await);
    }
    let reply: Reply = response
        .json()
        .await
        .map_err(|_| Error::Protocol("the sign-in service returned an unexpected response".into()))?;
    Ok(Session {
        uid: reply.local_id,
        email: reply.email.unwrap_or_else(|| email.to_string()),
        id_token: reply.id_token,
        refresh_token: reply.refresh_token,
        expires_at: expiry(reply.expires_in.as_deref()),
    })
}

/// Exchange a refresh token for a fresh ID token.
pub async fn refresh(http: &reqwest::Client, config: &Config, email: &str, refresh_token: &str) -> Result<Session> {
    #[derive(Deserialize)]
    struct Reply {
        id_token: String,
        user_id: String,
        refresh_token: Option<String>,
        expires_in: Option<String>,
    }
    let url = format!("{}/token?key={}", config.secure_token, urlencoding::encode(&config.api_key));
    let body = format!("grant_type=refresh_token&refresh_token={}", urlencoding::encode(refresh_token));
    let response = http
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| Error::Transient(format!("could not reach the token service: {e}")))?;
    if !response.status().is_success() {
        return Err(match failure(response).await {
            Error::SignIn(_) => Error::SessionExpired,
            other => other,
        });
    }
    let reply: Reply = response
        .json()
        .await
        .map_err(|_| Error::Protocol("the token service returned an unexpected response".into()))?;
    Ok(Session {
        uid: reply.user_id,
        email: email.to_string(),
        id_token: reply.id_token,
        refresh_token: reply.refresh_token.unwrap_or_else(|| refresh_token.to_string()),
        expires_at: expiry(reply.expires_in.as_deref()),
    })
}

/// Hands out a valid ID token, refreshing it when it is near expiry or when a
/// request was refused as unauthenticated.
pub struct TokenSource {
    http: reqwest::Client,
    config: Config,
    session: Mutex<Session>,
    on_refresh: Box<dyn Fn(&str) + Send + Sync>,
}

impl TokenSource {
    /// Wrap a session. `on_refresh` receives each new refresh token, so the
    /// caller can save it when Google rotates it.
    pub fn new(
        http: reqwest::Client,
        config: Config,
        session: Session,
        on_refresh: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self { http, config, session: Mutex::new(session), on_refresh: Box::new(on_refresh) }
    }

    /// The account's uid.
    pub async fn uid(&self) -> String {
        self.session.lock().await.uid.clone()
    }

    /// A token that is not about to expire.
    pub async fn token(&self) -> Result<String> {
        let mut session = self.session.lock().await;
        if Instant::now() >= session.expires_at {
            self.renew(&mut session).await?;
        }
        Ok(session.id_token.clone())
    }

    /// Force a refresh after a 401, unless another caller already did since
    /// `stale` was handed out.
    pub async fn refresh_after_401(&self, stale: &str) -> Result<String> {
        let mut session = self.session.lock().await;
        if session.id_token == stale {
            self.renew(&mut session).await?;
        }
        Ok(session.id_token.clone())
    }

    async fn renew(&self, session: &mut Session) -> Result<()> {
        let fresh = refresh(&self.http, &self.config, &session.email, &session.refresh_token).await?;
        if fresh.refresh_token != session.refresh_token {
            (self.on_refresh)(&fresh.refresh_token);
        }
        *session = fresh;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_failures_are_reported_without_guessing_which_half_was_wrong() {
        for code in ["INVALID_LOGIN_CREDENTIALS", "EMAIL_NOT_FOUND", "INVALID_PASSWORD"] {
            assert_eq!(readable(code).to_string(), "That email and password do not match an account.");
        }
        assert!(matches!(readable("INVALID_REFRESH_TOKEN"), Error::SessionExpired));
        assert!(matches!(readable("TOO_MANY_ATTEMPTS_TRY_LATER : later"), Error::SignIn(_)));
    }

    #[test]
    fn sessions_do_not_print_tokens() {
        let session = Session {
            uid: "u".into(),
            email: "e@x".into(),
            id_token: "SECRET1".into(),
            refresh_token: "SECRET2".into(),
            expires_at: Instant::now(),
        };
        let printed = format!("{session:?}");
        assert!(!printed.contains("SECRET"));
    }
}
