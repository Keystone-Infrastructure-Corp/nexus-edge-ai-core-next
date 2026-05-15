//! Token-bucket rate limiter used by the cold replicator to cap
//! upload bytes/sec.
//!
//! The replicator can have a 60 GiB backlog when a previously-down
//! cold link comes back; without a throttle the burst would
//! saturate the operator's WAN link and starve every other engine
//! task. We honour the `storage_cold_replica.throttle_bps` setting
//! by `acquire(n_bytes).await`-ing before each `put`, and the
//! bucket refills at the configured rate.
//!
//! Implementation notes:
//!
//!   * The bucket holds *credit* in bytes, not tokens. `capacity`
//!     defaults to one second's worth (`bps`) so a perfectly-paced
//!     uploader never blocks while a burst smooths over a single
//!     beat.
//!   * Refill is computed lazily from `Instant::now()` on each
//!     `acquire`. No background task; nothing to drop.
//!   * Concurrent acquisitions are serialised through a `Mutex`.
//!     A single replicator task means there's only ever one
//!     waiter, but the lock keeps the math correct if we ever
//!     fan out to a worker pool.
//!   * `bps` + `capacity` live INSIDE `BucketState` (not next to
//!     the `Arc<Mutex<…>>`) so a single bucket can outlive
//!     replicator ticks: [`Self::set_rate`] mutates them under the
//!     lock when admin config changes, preserving any accumulated
//!     credit instead of dropping it on every tick.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// Token bucket. Cheap to clone (it's just an `Arc<Mutex<…>>`).
#[derive(Clone)]
pub struct TokenBucket {
    inner: Arc<Mutex<BucketState>>,
}

struct BucketState {
    /// Configured rate in bytes/sec. `0` disables throttling.
    bps: u64,
    /// Burst capacity (= one second's worth of `bps` by default).
    capacity: u64,
    /// Bytes currently available. Float because the refill rate is
    /// fractional bytes-per-tick and rounding to integers would
    /// permanently leak budget on slow refills.
    bytes: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// Create a bucket that hands out `bytes_per_sec` bytes per
    /// second on average and absorbs bursts up to `bytes_per_sec`
    /// (one-second smoothing window). A `bytes_per_sec` of zero
    /// disables throttling — `acquire` is then instant.
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BucketState {
                bps: bytes_per_sec,
                capacity: bytes_per_sec,
                bytes: bytes_per_sec as f64,
                last_refill: Instant::now(),
            })),
        }
    }

    /// The configured rate. Surfaced on the admin API.
    pub async fn rate_bps(&self) -> u64 {
        self.inner.lock().await.bps
    }

    /// Reconfigure the bucket rate live, preserving any
    /// accumulated credit. Used by the cold replicator at the top
    /// of every tick so an admin throttle change (`PUT
    /// /v1/admin/storage/cold`) takes effect on the very next
    /// upload without dropping the burst budget earned during a
    /// quiet period.
    ///
    /// If the new rate is smaller than the previous one, the
    /// available credit is clamped to the new capacity (otherwise
    /// the very next `acquire(n)` could honour a burst larger than
    /// `bytes_per_sec` ever should). If the new rate is zero
    /// throttling is disabled — subsequent `acquire` calls return
    /// immediately.
    pub async fn set_rate(&self, bytes_per_sec: u64) {
        let mut s = self.inner.lock().await;
        // Refill against the OLD rate first so the in-flight
        // credit reflects the real elapsed time at the previous
        // throttle.
        let now = Instant::now();
        let elapsed = now.duration_since(s.last_refill).as_secs_f64();
        s.bytes = (s.bytes + elapsed * s.bps as f64).min(s.capacity as f64);
        s.last_refill = now;
        // Apply the new rate; clamp accumulated credit to the new
        // capacity if it shrank.
        s.bps = bytes_per_sec;
        s.capacity = bytes_per_sec;
        if s.bytes > s.capacity as f64 {
            s.bytes = s.capacity as f64;
        }
    }

    /// Block until at least `n` bytes of credit are available, then
    /// deduct them. Returns immediately if throttling is disabled.
    ///
    /// **Oversized requests.** A single `n` larger than the bucket
    /// `capacity` (one-second smoothing window) is allowed: the
    /// refill ceiling on this iteration is raised to `max(capacity,
    /// n)` so the credit can earn its way up to the request size
    /// instead of perpetually clamping at `capacity`. Without this,
    /// a 50 MiB clip uploaded under a 50 MiB/s throttle (capacity =
    /// 50 MiB) just barely passes, but a 60 MiB clip would block
    /// the replicator forever — a regression that lurked in the
    /// pre-M2.2-perf code path. Subsequent normal-sized requests
    /// see the usual `capacity` cap; oversized ones do not earn
    /// idle credit beyond their own `n`.
    pub async fn acquire(&self, n: u64) {
        loop {
            let wait = {
                let mut s = self.inner.lock().await;
                if s.bps == 0 {
                    return;
                }
                let now = Instant::now();
                let elapsed = now.duration_since(s.last_refill).as_secs_f64();
                let cap = (s.capacity as f64).max(n as f64);
                s.bytes = (s.bytes + elapsed * s.bps as f64).min(cap);
                s.last_refill = now;

                if s.bytes >= n as f64 {
                    s.bytes -= n as f64;
                    return;
                }
                // Compute time-to-refill. f64 is fine — we only need
                // millisecond precision.
                let deficit = n as f64 - s.bytes;
                let secs = deficit / s.bps as f64;
                // Cap the per-loop sleep so a misconfigured
                // throttle (1 byte/sec) doesn't lock the task for
                // hours; we'll just re-check more often.
                Duration::from_secs_f64(secs.min(0.250))
            };
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn zero_bps_is_instant() {
        let bucket = TokenBucket::new(0);
        let start = Instant::now();
        bucket.acquire(1_000_000).await;
        assert!(start.elapsed() < Duration::from_millis(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_acquire_inside_bucket_is_instant() {
        let bucket = TokenBucket::new(1_000);
        let start = Instant::now();
        bucket.acquire(500).await;
        assert!(start.elapsed() < Duration::from_millis(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn second_acquire_blocks_for_refill() {
        let bucket = TokenBucket::new(1_000);
        // Drain the initial credit.
        bucket.acquire(1_000).await;
        let start = Instant::now();
        // Asking for another 500 bytes at 1000 bps should take ~0.5 s.
        bucket.acquire(500).await;
        let elapsed = start.elapsed();
        // Allow some slack for the 250 ms cap inside acquire's sleep loop.
        assert!(
            elapsed >= Duration::from_millis(450),
            "acquire returned in {elapsed:?}; expected >= 450 ms"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_rate_preserves_credit_when_rate_grows() {
        // Earn some credit at 1000 bps, then bump to 2000 bps and
        // confirm we can immediately spend more than the OLD
        // capacity (i.e. carry-over survived).
        let bucket = TokenBucket::new(1_000);
        bucket.set_rate(2_000).await;
        let start = Instant::now();
        // We had 1000 credit + 0 elapsed; new capacity is 2000 so
        // we can spend up to 1000 immediately.
        bucket.acquire(900).await;
        assert!(start.elapsed() < Duration::from_millis(5));
        assert_eq!(bucket.rate_bps().await, 2_000);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_rate_clamps_credit_when_rate_shrinks() {
        // 1000 credit at 1000 bps; shrink to 100 bps. New capacity
        // is 100 → credit clamps down to 100. A 200-byte acquire
        // should block for ~1 s (100 bytes already + 100 bytes / 100 bps).
        let bucket = TokenBucket::new(1_000);
        bucket.set_rate(100).await;
        let start = Instant::now();
        bucket.acquire(200).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(900),
            "acquire returned in {elapsed:?}; expected >= 900 ms (credit must have been clamped)"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn set_rate_to_zero_disables_throttle() {
        let bucket = TokenBucket::new(1_000);
        bucket.acquire(1_000).await; // drain
        bucket.set_rate(0).await;
        let start = Instant::now();
        bucket.acquire(10_000_000).await;
        assert!(start.elapsed() < Duration::from_millis(2));
    }

    /// Pre-M2.2-perf the bucket would block forever on any single
    /// `acquire(n)` where `n > capacity` — `bytes` clamps at
    /// `capacity` so the `bytes >= n` test never passes. Real-world
    /// trigger: a 60 MiB clip uploaded under the default 50 MiB/s
    /// throttle. We now raise the per-iteration refill ceiling to
    /// `max(capacity, n)`, so an oversized request completes after
    /// `(n - initial_credit) / bps` seconds instead of hanging
    /// forever.
    #[tokio::test(flavor = "current_thread")]
    async fn acquire_larger_than_capacity_does_not_hang() {
        let bucket = TokenBucket::new(1_000); // capacity = 1000, bps = 1000
                                              // Drain the initial 1000 bytes of credit so the oversized
                                              // request has to earn the full amount from scratch.
        bucket.acquire(1_000).await;
        let start = Instant::now();
        // 1500 bytes at 1000 bps → ~1.5 s. Cap test at 4 s so a
        // regression of the hang shows as a test failure rather
        // than a CI timeout.
        let acquire = bucket.acquire(1_500);
        let res = tokio::time::timeout(Duration::from_secs(4), acquire).await;
        assert!(
            res.is_ok(),
            "acquire(n > capacity) hung — capacity-clamp regression"
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(1_400),
            "acquire(1500) at 1000 bps with empty bucket returned in {elapsed:?}; expected ~1.5 s"
        );
    }
}
