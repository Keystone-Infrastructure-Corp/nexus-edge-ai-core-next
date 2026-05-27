//! Bounded byte-identical response cache for `rpc_call` replay.
//!
//! Closes the engine half of Phase 1.16: when the cloud-side
//! `Idempotency-Key` middleware retries an `rpc_call` (same
//! `request_id`, possibly a freshly-minted `actor_token`), the engine
//! MUST replay the **byte-identical** original response rather than
//! re-running the handler or rejecting the retry. The cloud's
//! `idempotent_responses` table stores `bytea` for the same reason
//! (`jsonb` would canonicalise key order on read and break the
//! byte-identical contract — see the cloud-side note in
//! [`api-gateway/src/idempotency.rs`](../../../nexus-cloud-console/services/api-gateway/src/idempotency.rs)).
//!
//! ## Storage shape
//!
//! `Vec<u8>` keyed by the composite `(jti, request_id)` string, FIFO
//! eviction once capacity is reached. The 10 000-entry default
//! mirrors [`JtiReplayCache`] and is sized for the same ~30 s × ~300
//! mutating-RPC/s worst case.
//!
//! ## Coexistence with [`JtiReplayCache`]
//!
//! When a [`crate::dispatcher::RpcDispatcher`] is configured with a
//! response cache (via [`crate::dispatcher::RpcDispatcher::with_response_cache`]),
//! the dispatcher uses the response cache for replay decisions on
//! `rpc_call`s that carry a `request_id`. The [`JtiReplayCache`]
//! remains the authoritative dedup guard for evictions: if the
//! response was evicted before the retry arrived, the dispatcher
//! refuses the retry as a [`crate::error::InvalidReason::Replay`]
//! rather than re-running the handler. Under realistic edge workloads
//! this race is vanishingly rare (cache holds ~30 s of history,
//! `actor_token` TTL is also ~30 s).
//!
//! [`JtiReplayCache`]: crate::jti_cache::JtiReplayCache

use std::collections::{HashMap, VecDeque};

use parking_lot::Mutex;

/// Default capacity. Sized to match [`crate::jti_cache::JtiReplayCache`]
/// so the two structures admit and evict in lockstep.
pub const DEFAULT_CAPACITY: usize = 10_000;

/// Bounded `rpc_response` body cache. Cheap to clone the `Arc` and
/// share across handler threads — internal mutex is short-held
/// (microseconds; entries are at most a few KiB).
#[derive(Debug)]
pub struct RpcResponseCache {
    inner: Mutex<Inner>,
    capacity: usize,
}

#[derive(Debug)]
struct Inner {
    bodies: HashMap<String, Vec<u8>>,
    order: VecDeque<String>,
}

impl RpcResponseCache {
    /// Build a fresh cache at [`DEFAULT_CAPACITY`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Build a cache with the given capacity. Capacity 0 disables
    /// caching entirely ([`Self::get`] always returns `None`,
    /// [`Self::insert`] is a no-op); useful in tests.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                bodies: HashMap::with_capacity(capacity.max(1)),
                order: VecDeque::with_capacity(capacity.max(1)),
            }),
            capacity,
        }
    }

    /// Cache capacity supplied at construction.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the number of cached responses.
    pub fn len(&self) -> usize {
        self.inner.lock().order.len()
    }

    /// Returns `true` if no responses are cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Look up the cached body for `(jti, request_id)`. Returns
    /// `Some(Vec<u8>)` (a clone of the stored bytes) on a hit, or
    /// `None` if the tuple has never been seen or has been evicted.
    ///
    /// We clone rather than handing out a borrow so the caller doesn't
    /// hold the mutex across handler work.
    pub fn get(&self, jti: &str, request_id: &str) -> Option<Vec<u8>> {
        if self.capacity == 0 {
            return None;
        }
        let key = compose_key(jti, request_id);
        self.inner.lock().bodies.get(&key).cloned()
    }

    /// Insert the response body for `(jti, request_id)`. When the
    /// cache hits `capacity`, the oldest entry is evicted FIFO.
    ///
    /// Inserting against a key that already exists overwrites the
    /// body but does NOT re-order eviction (the key keeps its
    /// original FIFO slot). This matches the "byte-identical replay"
    /// contract: under normal flow the second call hits
    /// [`Self::get`] and never reaches this method.
    pub fn insert(&self, jti: &str, request_id: &str, body: Vec<u8>) {
        if self.capacity == 0 {
            return;
        }
        let key = compose_key(jti, request_id);
        let mut guard = self.inner.lock();
        if let std::collections::hash_map::Entry::Occupied(mut e) = guard.bodies.entry(key.clone())
        {
            e.insert(body);
            return;
        }
        if guard.order.len() == self.capacity {
            if let Some(evicted) = guard.order.pop_front() {
                guard.bodies.remove(&evicted);
            }
        }
        guard.bodies.insert(key.clone(), body);
        guard.order.push_back(key);
    }
}

/// Build the composite key. Mirrors
/// [`crate::jti_cache::JtiReplayCache`]'s separator so logs that
/// surface both keys read the same way.
fn compose_key(jti: &str, request_id: &str) -> String {
    let mut s = String::with_capacity(jti.len() + 1 + request_id.len());
    s.push_str(jti);
    s.push('|');
    s.push_str(request_id);
    s
}

impl Default for RpcResponseCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn miss_then_hit() {
        let cache = RpcResponseCache::new();
        assert!(cache.get("j", "r").is_none());
        cache.insert("j", "r", b"hello".to_vec());
        assert_eq!(cache.get("j", "r").as_deref(), Some(b"hello" as &[u8]));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn replay_is_byte_identical() {
        let cache = RpcResponseCache::new();
        let body = br#"{"id":42,"name":"alpha"}"#.to_vec();
        cache.insert("j", "r", body.clone());
        // Multiple lookups must each return identical bytes.
        for _ in 0..5 {
            assert_eq!(cache.get("j", "r"), Some(body.clone()));
        }
    }

    #[test]
    fn different_request_ids_are_distinct() {
        let cache = RpcResponseCache::new();
        cache.insert("j", "r1", b"first".to_vec());
        cache.insert("j", "r2", b"second".to_vec());
        assert_eq!(cache.get("j", "r1").as_deref(), Some(b"first" as &[u8]));
        assert_eq!(cache.get("j", "r2").as_deref(), Some(b"second" as &[u8]));
    }

    #[test]
    fn eviction_drops_oldest() {
        let cache = RpcResponseCache::with_capacity(2);
        cache.insert("j", "r1", b"a".to_vec());
        cache.insert("j", "r2", b"b".to_vec());
        cache.insert("j", "r3", b"c".to_vec());
        assert!(cache.get("j", "r1").is_none());
        assert_eq!(cache.get("j", "r2").as_deref(), Some(b"b" as &[u8]));
        assert_eq!(cache.get("j", "r3").as_deref(), Some(b"c" as &[u8]));
    }

    #[test]
    fn zero_capacity_disables_caching() {
        let cache = RpcResponseCache::with_capacity(0);
        cache.insert("j", "r", b"x".to_vec());
        assert!(cache.get("j", "r").is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn overwrite_keeps_fifo_slot() {
        let cache = RpcResponseCache::with_capacity(2);
        cache.insert("j", "r1", b"first".to_vec());
        cache.insert("j", "r2", b"second".to_vec());
        // Overwriting r1 must NOT make it the youngest slot —
        // a subsequent insert should still evict r1.
        cache.insert("j", "r1", b"first-v2".to_vec());
        cache.insert("j", "r3", b"third".to_vec());
        assert!(cache.get("j", "r1").is_none());
        assert_eq!(cache.get("j", "r2").as_deref(), Some(b"second" as &[u8]));
        assert_eq!(cache.get("j", "r3").as_deref(), Some(b"third" as &[u8]));
    }
}
