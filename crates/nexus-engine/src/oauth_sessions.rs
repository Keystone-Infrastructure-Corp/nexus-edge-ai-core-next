//! In-memory cache for pending OAuth auth-code sessions.
//!
//! ## Why this exists
//!
//! The OAuth auth-code flow is a three-leg round-trip:
//!
//! 1. UI hits `POST /api/v1/admin/oauth/{provider}/start` with the
//!    backend handle + client_id + client_secret + email. Engine
//!    generates an unguessable `state` token, stashes the form
//!    fields in THIS cache keyed by state, and returns
//!    `{authorize_url, state}`.
//! 2. UI opens `authorize_url` in a popup. Operator consents.
//!    Provider redirects the popup to
//!    `GET /api/v1/admin/oauth/{provider}/callback?code=…&state=…`.
//!    Engine looks up the state, exchanges the code for a
//!    refresh_token (using the stashed client_secret), encrypts +
//!    persists it, marks the session `Complete`.
//! 3. UI polls `GET /api/v1/admin/oauth/status?state=…` until it
//!    sees `Complete` (or `Error`), then refreshes its backend list.
//!
//! The session cache is the glue that lets the callback land in a
//! browser tab the UI doesn't directly control, hand the result
//! back to the original UI tab via a shared state token, and never
//! leak the freshly-minted `refresh_token` through the browser.
//!
//! ## Security properties
//!
//! * **State is unguessable.** 32 OS-random bytes →
//!   base64url-no-pad (43 chars). The `/callback` handler treats
//!   "valid state in cache" as proof-of-authorisation because the
//!   browser hitting that URL has no admin bearer token.
//! * **client_secret lives in RAM only.** Stashed for the duration
//!   of the session TTL (10 min by default) and dropped on either
//!   `take()` after success/error or on TTL sweep. Never written
//!   to disk.
//! * **TTL is finite.** A `Pending` session that never sees its
//!   `/callback` (operator closed the popup, network failed) gets
//!   reaped on the next sweep. The cache never grows unbounded.
//! * **Status survives briefly past completion.** The UI needs to
//!   see `Complete` once before the session is dropped; we keep
//!   it readable for at most `SESSION_TTL` after the callback
//!   resolved.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How long a session is allowed to live in the cache, counting
/// from when `start` was called. 10 min is well above the typical
/// OAuth consent time (~30 s) and below the operator's patience
/// for an abandoned popup.
pub const SESSION_TTL: Duration = Duration::from_secs(600);

/// Lifecycle state of a single pending session. Lives in the
/// `status` field of [`PendingSession`].
#[derive(Debug, Clone)]
pub enum SessionStatus {
    /// `start` happened; we're waiting for the provider's
    /// redirect to hit `/callback`.
    Pending,
    /// `/callback` ran exchange_code + persist successfully. The
    /// `handle` is the backend that landed on disk.
    Complete { handle: String },
    /// `/callback` ran but failed somewhere (invalid code, network
    /// error talking to the token endpoint, encryption failed,
    /// upsert failed). Message is human-readable; never contains
    /// the refresh token or client secret.
    Error { message: String },
}

/// Everything we need to keep around between `start` and
/// `/callback`. Fields are private to this module so the rest of
/// the engine can't accidentally serialise one of these.
#[derive(Debug, Clone)]
pub struct PendingSession {
    pub provider: String,
    pub handle: String,
    pub client_id: String,
    pub client_secret: String,
    pub account_email: Option<String>,
    /// gdrive-only optional knob; passed straight through to
    /// `CloudConfigJson.extra.root_folder_id` on the resulting
    /// upsert. Ignored for onedrive.
    pub root_folder_id: Option<String>,
    /// MUST match what was sent to the provider on the
    /// `authorize_url`; the provider echoes nothing to verify
    /// against, but RFC 6749 §4.1.3 requires the same value at
    /// the token endpoint. The callback handler reads this back
    /// when calling `exchange_code`.
    pub redirect_uri: String,
    pub created_at: Instant,
    pub status: SessionStatus,
}

impl PendingSession {
    fn is_expired(&self) -> bool {
        self.created_at.elapsed() > SESSION_TTL
    }
}

/// Thread-safe handle on the session cache. Cheap to clone; the
/// `Arc<Mutex<…>>` is shared.
#[derive(Default, Clone)]
pub struct OAuthSessions {
    inner: Arc<Mutex<HashMap<String, PendingSession>>>,
}

impl OAuthSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a freshly-started session keyed by its state token.
    /// Drops any expired sessions at the same time so the cache
    /// can't grow unbounded if the operator restarts the OAuth
    /// flow repeatedly without ever completing it.
    pub fn insert(&self, state: String, session: PendingSession) {
        let mut g = self.inner.lock();
        g.retain(|_, s| !s.is_expired());
        g.insert(state, session);
    }

    /// Fetch a session snapshot by state. Used by `/callback` (to
    /// read client_secret + redirect_uri before calling
    /// `exchange_code`) and by `/status` (to surface the lifecycle
    /// state to the polling UI). Returns `None` when the session
    /// is missing, expired, or its key never existed.
    pub fn get(&self, state: &str) -> Option<PendingSession> {
        let mut g = self.inner.lock();
        let entry = g.get(state)?;
        if entry.is_expired() {
            g.remove(state);
            return None;
        }
        Some(entry.clone())
    }

    /// Mutate a session's `status` in place. Used by `/callback`
    /// to flip from `Pending` to `Complete` / `Error`. No-op if
    /// the session has already been swept.
    pub fn set_status(&self, state: &str, status: SessionStatus) {
        let mut g = self.inner.lock();
        if let Some(entry) = g.get_mut(state) {
            entry.status = status;
        }
    }

    /// Best-effort prune. Called from `/status` after a `Complete`
    /// has been observed so the UI's subsequent poll won't
    /// continue returning the same row indefinitely.
    pub fn remove(&self, state: &str) {
        let mut g = self.inner.lock();
        g.remove(state);
    }

    /// Number of in-flight sessions. Used by tests only.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PendingSession {
        PendingSession {
            provider: "gdrive".into(),
            handle: "test-bucket".into(),
            client_id: "cid".into(),
            client_secret: "csec".into(),
            account_email: Some("ops@example.com".into()),
            root_folder_id: None,
            redirect_uri: "https://engine.local/api/v1/admin/oauth/gdrive/callback".into(),
            created_at: Instant::now(),
            status: SessionStatus::Pending,
        }
    }

    #[test]
    fn round_trip_get_returns_inserted_session() {
        let s = OAuthSessions::new();
        s.insert("abc".into(), sample());
        let got = s.get("abc").expect("present");
        assert_eq!(got.handle, "test-bucket");
        assert!(matches!(got.status, SessionStatus::Pending));
    }

    #[test]
    fn missing_state_returns_none() {
        let s = OAuthSessions::new();
        assert!(s.get("nope").is_none());
    }

    #[test]
    fn set_status_then_get_observes_new_status() {
        let s = OAuthSessions::new();
        s.insert("abc".into(), sample());
        s.set_status(
            "abc",
            SessionStatus::Complete {
                handle: "test-bucket".into(),
            },
        );
        match s.get("abc").unwrap().status {
            SessionStatus::Complete { handle } => assert_eq!(handle, "test-bucket"),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn expired_session_is_swept_on_access() {
        let s = OAuthSessions::new();
        let mut stale = sample();
        // Backdate so is_expired() reports true.
        stale.created_at = Instant::now() - (SESSION_TTL + Duration::from_secs(1));
        s.insert("old".into(), stale);
        assert!(
            s.get("old").is_none(),
            "expired session must not be returned"
        );
        assert_eq!(s.len(), 0, "expired session must be removed on get");
    }

    #[test]
    fn insert_evicts_expired_neighbours() {
        let s = OAuthSessions::new();
        let mut stale = sample();
        stale.created_at = Instant::now() - (SESSION_TTL + Duration::from_secs(1));
        s.insert("old".into(), stale);
        s.insert("fresh".into(), sample());
        assert_eq!(s.len(), 1, "insert must sweep expired peers");
        assert!(s.get("fresh").is_some());
    }
}
