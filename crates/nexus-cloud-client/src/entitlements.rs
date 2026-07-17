//! Cache for the most recent `entitlement_update` payload.
//!
//! The engine applies entitlement quota (max cameras, max storage,
//! enabled features) from the most recent JWT the cloud-console pushed.
//! Caching it in-process lets the engine start up before the first
//! heartbeat round-trip lands; persistence to the local data dir is the
//! engine's concern (this crate only provides the in-memory cache).

use base64::Engine as _;
use parking_lot::RwLock;
use serde::Deserialize;

/// Latest entitlement JWT. Phase 1.7 stores the compact JWS verbatim;
/// the engine decodes + verifies it against the bundled signing key
/// (same key the [`crate::actor_token::Verifier`] uses, per
/// `WIRE_PROTOCOL.md §11`).
#[derive(Debug, Default)]
pub struct EntitlementCache {
    inner: RwLock<Option<String>>,
}

impl EntitlementCache {
    /// Fresh empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the cached JWT with `jwt`. Returns the previous value
    /// (if any) so callers can compare for change-detection.
    pub fn store(&self, jwt: impl Into<String>) -> Option<String> {
        let mut guard = self.inner.write();
        guard.replace(jwt.into())
    }

    /// Snapshot the current JWT, cloning it. Returns `None` if no
    /// entitlement has been received yet.
    pub fn current(&self) -> Option<String> {
        self.inner.read().clone()
    }

    /// `true` once at least one entitlement update has been stored.
    pub fn is_populated(&self) -> bool {
        self.inner.read().is_some()
    }

    /// `true` when the cached entitlement reports the org is suspended
    /// for non-payment — `plan == "suspended"` OR `max_cameras <= 0`
    /// (the shape cloud `entitlement-svc` mints via
    /// `demote_entitlement_to_zero`; cloud PHASES §3.10 / ARCHITECTURE
    /// §12.4). Consumed by the engine's `CloudAwarePolicy` to suppress
    /// the always-on `cloud:console` audit sink (external sinks stay
    /// unaffected).
    ///
    /// **Fail-open:** returns `false` when no entitlement has landed
    /// yet, or when the cached JWT can't be decoded — a fresh or
    /// offline core keeps delivering to the cloud audit sink rather
    /// than silently going dark (engine Hard Rule 5). Claims are read
    /// WITHOUT signature verification: this is a delivery-gating hint,
    /// not a security boundary (the JWT arrived over the authenticated
    /// mTLS tunnel, and a suspended customer has no incentive to forge
    /// themselves back into service).
    #[must_use]
    pub fn is_suspended(&self) -> bool {
        let Some(jwt) = self.current() else {
            return false;
        };
        decode_entitlement_claims(&jwt).is_some_and(|c| {
            c.plan.as_deref() == Some("suspended") || c.max_cameras.is_some_and(|n| n <= 0)
        })
    }
}

/// Minimal view of the entitlement JWT claims the edge needs to decide
/// cloud-delivery suspension. The full claim set lives cloud-side in
/// `entitlement-svc`; the edge only reads the two fields that decide
/// suspension. Unknown fields are ignored (no `deny_unknown_fields`).
#[derive(Debug, Deserialize)]
struct EntitlementClaimsView {
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    max_cameras: Option<i64>,
}

/// Decode (WITHOUT verifying the signature) the claims segment of a
/// compact JWS `header.payload.signature`. Returns `None` on any
/// malformed input so [`EntitlementCache::is_suspended`] can fail open.
fn decode_entitlement_claims(jwt: &str) -> Option<EntitlementClaimsView> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let signature = parts.next()?;
    // A compact JWS has exactly three non-empty segments.
    if parts.next().is_some() || payload.is_empty() || signature.is_empty() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_returns_previous_value() {
        let cache = EntitlementCache::new();
        assert!(cache.store("v1").is_none());
        assert_eq!(cache.store("v2"), Some("v1".to_string()));
        assert_eq!(cache.current(), Some("v2".to_string()));
        assert!(cache.is_populated());
    }

    /// Build a compact JWS with the given claims JSON in the payload
    /// segment. The header + signature are placeholders —
    /// `decode_entitlement_claims` never verifies them.
    fn jwt_with_claims(claims_json: &str) -> String {
        let header =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"EdDSA\"}");
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims_json.as_bytes());
        format!("{header}.{payload}.c2ln")
    }

    #[test]
    fn not_suspended_when_no_entitlement_cached() {
        let cache = EntitlementCache::new();
        assert!(!cache.is_suspended());
    }

    #[test]
    fn not_suspended_for_active_plan() {
        let cache = EntitlementCache::new();
        cache.store(jwt_with_claims(r#"{"plan":"pro","max_cameras":24}"#));
        assert!(!cache.is_suspended());
    }

    #[test]
    fn suspended_when_plan_is_suspended() {
        let cache = EntitlementCache::new();
        cache.store(jwt_with_claims(r#"{"plan":"suspended","max_cameras":0}"#));
        assert!(cache.is_suspended());
    }

    #[test]
    fn suspended_when_max_cameras_zero() {
        let cache = EntitlementCache::new();
        cache.store(jwt_with_claims(r#"{"plan":"starter","max_cameras":0}"#));
        assert!(cache.is_suspended());
    }

    #[test]
    fn fails_open_on_garbage_jwt() {
        let cache = EntitlementCache::new();
        cache.store("not-a-jwt");
        assert!(!cache.is_suspended());
    }
}
