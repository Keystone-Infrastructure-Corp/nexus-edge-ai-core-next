//! Go-dark insurance: prove the cloud can still reach us, and undo the
//! release that took that away.
//!
//! ## The failure this exists for
//!
//! Every remote-access story in this product runs over one outbound
//! tunnel. An update that starts cleanly, passes its own health checks,
//! serves the local UI, records clips, and evaluates rules — but cannot
//! re-establish that tunnel — leaves an appliance that is *working* and
//! *unreachable*. Nothing else in the OTA path notices, because from the
//! engine's point of view nothing is wrong. The box is simply gone, and
//! the only remaining recovery is somebody physically visiting it.
//!
//! That is the single most expensive outcome this system can produce, and
//! it is silent. So it gets its own signal and its own watchdog rather
//! than being folded into the general health gate.
//!
//! ## Two independent mitigations
//!
//! 1. **OTA gating** — a freshly-installed release is not declared
//!    `success` until the tunnel has completed an authenticated connect
//!    and at least one heartbeat round-trip on the *new* binary. Handled
//!    by [`TunnelLiveness::heartbeat_since_boot`], consumed in
//!    `cloud_update`.
//!
//! 2. **A watchdog independent of OTA** — if the tunnel has been dead for
//!    longer than the configured window, reflip to the previously-good
//!    release even though no update is in flight. This catches the cases
//!    OTA gating structurally cannot: a client certificate that expired,
//!    a rotation bug, a cloud-side change the edge cannot tolerate. Those
//!    do not arrive on an update boundary, so no update-scoped check will
//!    ever see them.
//!
//! ## Why the watchdog fires at most once per boot
//!
//! A customer's WAN can be down for a week. If the watchdog reflipped on
//! every window it would walk the appliance backwards through its release
//! history and then thrash, turning somebody else's outage into our
//! corruption. One reflip per boot means the worst case is: we go back one
//! release, we stay there, and we log loudly. If that release also cannot
//! reach the cloud, the honest conclusion is that the problem is not the
//! release — and reflipping again would be a guess dressed up as a
//! remedy.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nexus_store::Store;
use tokio::time::Instant;
use tracing::{error, info, warn};

/// How often the watchdog wakes up. Small relative to the window it is
/// enforcing; the cost is one atomic load.
const WATCHDOG_TICK: Duration = Duration::from_secs(60);

/// Grace period before the watchdog will consider firing at all, measured
/// from process start. A box that boots into a WAN that is not up yet
/// must not immediately conclude its own release is broken.
const WATCHDOG_GRACE: Duration = Duration::from_secs(15 * 60);

/// Shared, lock-free view of whether the cloud tunnel is actually alive.
///
/// Deliberately not a `watch` channel: every consumer here is a poller on
/// a minutes-scale cadence, and the producer is on the heartbeat path
/// where an extra wakeup per 30 s tick buys nothing.
///
/// The signal owns its clock. It used to take the elapsed time from the
/// caller, and the two callers disagreed about the origin: the heartbeat
/// pump measured from the current *connection*, the watchdog from process
/// start. A long-lived process that reconnected late therefore looked dark
/// for the age of the process and could reflip a perfectly healthy
/// appliance (BUG-133).
#[derive(Debug)]
pub struct TunnelLiveness {
    /// Origin for every duration below. `tokio::time::Instant` so tests
    /// can drive the watchdog on a virtual clock, and monotonic so an NTP
    /// step on a freshly-booted appliance cannot fabricate hours of
    /// apparent silence.
    created: Instant,
    /// Milliseconds since [`Self::created`] at the last acknowledged
    /// heartbeat. Zero means "never".
    last_ack_ms: AtomicU64,
    /// Set once the tunnel has authenticated at least once in this
    /// process. Never cleared — its question is "did this binary ever
    /// work", not "is it working right now".
    authenticated: AtomicBool,
    /// Set when the watchdog has already spent its one reflip.
    reflipped: AtomicBool,
}

impl Default for TunnelLiveness {
    fn default() -> Self {
        Self::new()
    }
}

impl TunnelLiveness {
    /// Fresh signal for a process that has not connected yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            created: Instant::now(),
            last_ack_ms: AtomicU64::new(0),
            authenticated: AtomicBool::new(false),
            reflipped: AtomicBool::new(false),
        }
    }

    /// Record that the tunnel authenticated. Called by the supervisor on
    /// every successful connect.
    pub fn mark_authenticated(&self) {
        self.authenticated.store(true, Ordering::Relaxed);
    }

    /// Record that the cloud **answered** a heartbeat.
    ///
    /// Called only from the inbound `heartbeat_ack` path. A send that the
    /// tunnel accepted proves the local channel had room, not that anyone
    /// received it: a half-open socket accepts writes indefinitely.
    pub fn mark_ack(&self) {
        let elapsed = u64::try_from(self.created.elapsed().as_millis()).unwrap_or(u64::MAX);
        // Floor at 1 ms: zero is the "never acknowledged" sentinel, so an
        // ack inside the opening millisecond must not read as never.
        self.last_ack_ms.store(elapsed.max(1), Ordering::Relaxed);
    }

    /// Has this process ever completed an authenticated connect *and* a
    /// heartbeat round-trip? This is the OTA success gate.
    #[must_use]
    pub fn heartbeat_since_boot(&self) -> bool {
        self.authenticated.load(Ordering::Relaxed) && self.last_ack_ms.load(Ordering::Relaxed) > 0
    }

    /// Milliseconds since the cloud last answered — or since this signal
    /// was created, if it never has. Never having connected counts against
    /// us from boot, otherwise a release that cannot authenticate at all —
    /// the worst case — is the one case the watchdog ignores.
    #[must_use]
    pub fn dark_ms(&self) -> u64 {
        let now = u64::try_from(self.created.elapsed().as_millis()).unwrap_or(u64::MAX);
        match self.last_ack_ms.load(Ordering::Relaxed) {
            0 => now,
            last => now.saturating_sub(last),
        }
    }
}

/// Watchdog loop. Runs for the life of the process.
///
/// `window` is the tolerated dark period. `None` disables the watchdog —
/// which is the correct setting for an appliance that was never enrolled
/// with the cloud at all, since "the tunnel is down" is then not a fault.
pub async fn run_cloud_liveness_watchdog(
    store: Arc<Store>,
    liveness: Arc<TunnelLiveness>,
    window: Option<Duration>,
) {
    let Some(window) = window else {
        info!("cloud-liveness watchdog disabled (no window configured)");
        return;
    };

    let started = std::time::Instant::now();
    let mut ticker = tokio::time::interval(WATCHDOG_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    info!(
        window_s = window.as_secs(),
        "cloud-liveness watchdog armed; will reflip at most once per boot"
    );

    loop {
        ticker.tick().await;
        let elapsed = started.elapsed();
        if elapsed < WATCHDOG_GRACE {
            continue;
        }
        if liveness.reflipped.load(Ordering::Relaxed) {
            continue;
        }
        // An appliance that has never been enrolled has no cloud to be
        // dark from. Re-read each tick so enrolling mid-life arms the
        // watchdog without a restart.
        match store.get_cloud_enrollment().await {
            Ok(Some(_)) => {}
            Ok(None) => continue,
            Err(e) => {
                warn!(error = %e, "cloud-liveness watchdog could not read enrollment");
                continue;
            }
        }

        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        let dark_ms = liveness.dark_ms();
        if dark_ms < window_ms {
            continue;
        }

        let Some(previous) = crate::cloud_update::previous_good_version(&store).await else {
            error!(
                signal = "cloud_dark_no_rollback_target",
                dark_s = dark_ms / 1000,
                "the cloud tunnel has been dark past its window and there is no \
                 previous-good release to fall back to; this appliance needs \
                 local intervention"
            );
            // Latch so this pages once rather than every minute forever.
            liveness.reflipped.store(true, Ordering::Relaxed);
            continue;
        };

        error!(
            signal = "cloud_dark_reflip",
            dark_s = dark_ms / 1000,
            window_s = window.as_secs(),
            previous_good = %previous,
            "the cloud tunnel has been dark past its window; reflipping to the \
             previous-good release. This is the go-dark watchdog, not an OTA \
             failure — no update was in flight."
        );
        liveness.reflipped.store(true, Ordering::Relaxed);
        crate::cloud_update::reflip_to(&store, &previous).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::TunnelLiveness;

    #[tokio::test(start_paused = true)]
    async fn a_fresh_signal_has_not_proved_anything() {
        let l = TunnelLiveness::new();
        assert!(!l.heartbeat_since_boot());
    }

    #[tokio::test(start_paused = true)]
    async fn authentication_alone_is_not_enough() {
        let l = TunnelLiveness::new();
        l.mark_authenticated();
        assert!(
            !l.heartbeat_since_boot(),
            "connecting proves the socket opened, not that the cloud answered"
        );
        l.mark_ack();
        assert!(l.heartbeat_since_boot());
    }

    /// BUG-133 — the whole point of the signal. A half-open socket accepts
    /// every heartbeat, so darkness has to be measured from the last reply
    /// the cloud actually sent.
    #[tokio::test(start_paused = true)]
    async fn dark_time_is_measured_from_the_last_acknowledgement() {
        let l = TunnelLiveness::new();
        l.mark_ack();
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        // Not exactly 30_000: an ack in the opening millisecond is floored
        // to 1 ms so it cannot read as "never acknowledged".
        let dark = l.dark_ms();
        assert!((29_000..31_000).contains(&dark), "{dark}");

        l.mark_ack();
        assert!(l.dark_ms() < 1_000, "a fresh ack resets the dark clock");
    }

    /// A binary that never completes a round-trip is the worst case, so it
    /// must count as dark from boot rather than being exempt.
    #[tokio::test(start_paused = true)]
    async fn never_acknowledged_counts_as_dark_from_creation() {
        let l = TunnelLiveness::new();
        tokio::time::advance(std::time::Duration::from_secs(600)).await;
        assert!(
            l.dark_ms() >= 600_000,
            "never having been answered must count against us: {}",
            l.dark_ms()
        );
    }
}
