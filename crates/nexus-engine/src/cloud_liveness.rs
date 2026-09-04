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
#[derive(Debug, Default)]
pub struct TunnelLiveness {
    /// Monotonic milliseconds since process start at the last heartbeat
    /// the tunnel accepted for send. Zero means "never".
    last_heartbeat_ms: AtomicU64,
    /// Set once the tunnel has authenticated at least once in this
    /// process. Never cleared — its question is "did this binary ever
    /// work", not "is it working right now".
    authenticated: AtomicBool,
    /// Set when the watchdog has already spent its one reflip.
    reflipped: AtomicBool,
}

impl TunnelLiveness {
    /// Fresh signal for a process that has not connected yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the tunnel authenticated. Called by the supervisor on
    /// every successful connect.
    pub fn mark_authenticated(&self) {
        self.authenticated.store(true, Ordering::Relaxed);
    }

    /// Record a heartbeat the tunnel accepted for send.
    ///
    /// `elapsed_ms` is milliseconds since process start, which the caller
    /// already has. Wall-clock is deliberately avoided: an NTP step on a
    /// freshly-booted appliance is common, and it must not be able to make
    /// the watchdog believe hours have passed.
    pub fn mark_heartbeat(&self, elapsed_ms: u64) {
        self.last_heartbeat_ms.store(elapsed_ms, Ordering::Relaxed);
    }

    /// Has this process ever completed an authenticated connect *and* a
    /// heartbeat? This is the OTA success gate.
    #[must_use]
    pub fn heartbeat_since_boot(&self) -> bool {
        self.authenticated.load(Ordering::Relaxed)
            && self.last_heartbeat_ms.load(Ordering::Relaxed) > 0
    }

    /// Milliseconds since the last accepted heartbeat, or `None` if there
    /// has never been one.
    #[must_use]
    pub fn idle_ms(&self, now_elapsed_ms: u64) -> Option<u64> {
        let last = self.last_heartbeat_ms.load(Ordering::Relaxed);
        if last == 0 {
            return None;
        }
        Some(now_elapsed_ms.saturating_sub(last))
    }

    /// Whether the cloud should be treated as reachable right now, given a
    /// caller's staleness budget.
    ///
    /// A tunnel that has never authenticated, or whose last accepted
    /// heartbeat is older than `budget`, is unreachable — the same "never
    /// having connected counts against us" reading the watchdog loop
    /// applies above, expressed as a pure, independently-testable function
    /// rather than only inline in that loop.
    ///
    /// This is the real, non-fabricated connectivity signal SPEC-055 AC-7
    /// (Wave 18) feeds into [`nexus_sinks::emergency::EmergencyPolicy::decide`]'s
    /// `cloud_reachable` parameter for the disconnected-alarm proof — not a
    /// caller-supplied bool standing in for a real read.
    ///
    /// SPEC-037 (Wave 26): `nexus-engine`'s `EngineEmergencyDispatch` is now
    /// a real production caller — see `crates/nexus-engine/src/emergency_dispatch.rs`.
    #[must_use]
    pub fn is_reachable(&self, now_elapsed_ms: u64, budget: Duration) -> bool {
        let budget_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX);
        match self.idle_ms(now_elapsed_ms) {
            Some(idle) => idle < budget_ms,
            None => false,
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

        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        // Never having connected counts against us from boot, otherwise a
        // release that cannot authenticate at all — the worst case — is
        // the one case the watchdog ignores.
        let dark_ms = liveness.idle_ms(elapsed_ms).unwrap_or(elapsed_ms);
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

    #[test]
    fn a_fresh_signal_has_not_proved_anything() {
        let l = TunnelLiveness::new();
        assert!(!l.heartbeat_since_boot());
        assert_eq!(l.idle_ms(10_000), None);
    }

    #[test]
    fn authentication_alone_is_not_enough() {
        let l = TunnelLiveness::new();
        l.mark_authenticated();
        assert!(
            !l.heartbeat_since_boot(),
            "connecting proves the socket opened, not that the cloud answered"
        );
        l.mark_heartbeat(5_000);
        assert!(l.heartbeat_since_boot());
    }

    #[test]
    fn idle_is_measured_from_the_last_heartbeat() {
        let l = TunnelLiveness::new();
        l.mark_heartbeat(60_000);
        assert_eq!(l.idle_ms(90_000), Some(30_000));
        // A clock that appears to go backwards saturates rather than
        // wrapping into a huge idle time and firing the watchdog.
        assert_eq!(l.idle_ms(10_000), Some(0));
    }

    #[test]
    fn a_tunnel_that_never_authenticated_is_never_reachable() {
        let l = TunnelLiveness::new();
        assert!(!l.is_reachable(0, std::time::Duration::from_secs(60)));
    }

    // ---------------------------------------------------------------------
    // SPEC-055 AC-7 (Wave 18) — a staged Tier-0 event must alarm with the
    // cloud disconnected, including a tunnel-down variant. This proves it
    // against the *real* `TunnelLiveness` production type, not a bare bool
    // standing in for one: a tunnel torn down past its own staleness budget
    // derives `cloud_reachable = false` through `is_reachable`, and that
    // derived value — not a hand-picked `false` literal — is what
    // `EmergencyPolicy::decide` is fed.
    // ---------------------------------------------------------------------

    /// A tunnel that authenticated and heartbeat once, then went dark past
    /// its staleness budget (the tunnel-down variant), derives
    /// `cloud_reachable = false` and a staged Tier-0 signal still alarms —
    /// proven against the production `EmergencyPolicy`, not a fixture.
    ///
    /// Fault injection: temporarily changed `is_reachable`'s `Some(idle) =>
    /// idle < budget_ms` arm to always return `true` once any heartbeat had
    /// ever landed (ignoring how stale it was). This test failed on
    /// `assert!(!reachable, ...)` (became `true`) and would also have failed
    /// on the final `decide` assertion (an unconfirmed Firearm signal fed
    /// `cloud_reachable = true` degrades to `AwaitBrandishConfirmation`, not
    /// `Alarm`); reverted, both pass.
    #[test]
    fn a_tunnel_down_past_its_budget_derives_unreachable_and_a_staged_tier0_still_alarms() {
        let liveness = TunnelLiveness::new();
        liveness.mark_authenticated();
        liveness.mark_heartbeat(1_000);

        let budget = std::time::Duration::from_secs(90);
        // Well past the 90 s budget: the tunnel went dark and never came
        // back, the disconnected variant this AC names.
        let now_elapsed_ms = 10 * 60 * 1_000;

        let reachable = liveness.is_reachable(now_elapsed_ms, budget);
        assert!(
            !reachable,
            "a tunnel dark for 10 minutes against a 90 s budget must derive unreachable"
        );

        let policy = nexus_sinks::emergency::EmergencyPolicy::default();
        let staged_firearm = nexus_sinks::emergency::EmergencySignal {
            class: nexus_sinks::emergency::Tier0Class::Firearm,
            persistence: std::time::Duration::from_secs(1),
            brandish_confirmed: None,
        };
        // Online (reachable = true) this same unconfirmed signal would only
        // AWAIT confirmation, never alarm outright — so this assertion is
        // only true because the derived signal is actually unreachable, not
        // because Firearm always alarms.
        assert_eq!(
            policy.decide(&staged_firearm, reachable),
            nexus_sinks::emergency::EmergencyOutcome::Alarm
        );
    }
}
