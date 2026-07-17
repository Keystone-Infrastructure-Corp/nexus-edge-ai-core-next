//! M7 Phase 1 Step 3 — alert-sink dispatcher.
//!
//! Single background task that drains `alert_sink_outbox`. One per
//! engine process; serializes outbound HTTP traffic to keep per-
//! sink retry queues coherent (no thundering herd when a flapping
//! endpoint comes back online).
//!
//! State machine — for every pending row:
//!
//! ```text
//!   pending ──policy──Suppressed──>  suppressed   (terminal)
//!       │
//!       ├──policy──Deliver──> sink.deliver()
//!       │                          ├─Ok ─────────> sent     (terminal)
//!       │                          └─Err
//!       │                              ├─attempts < MAX ─> failed→pending
//!       │                              └─attempts ≥ MAX ─> dead   (terminal)
//!       │
//!       └──registry-missing (sink deleted out from under a stale row)
//!                              └────────────────> dead   (terminal)
//! ```
//!
//! `event_missing` (the events row vanished — most likely because
//! a clip eviction cascaded through `events.clip_id`) is also
//! terminal-dead, on the principle that an alert with no payload
//! cannot be delivered and is not worth retrying.
//!
//! The delivery-policy evaluation is gated through the
//! [`DeliveryPolicy`] trait; M7 Phase 1 Step 3 ships
//! [`AllowAllPolicy`] (the default) and Step 4 will swap in the
//! real `delivery_settings` + per-rule cascade. Keeping the
//! decision point isolated behind the trait lets us add the
//! schedule + global-toggle logic without touching the dispatcher
//! loop.
//!
//! Tests live in `crates/nexus-engine/tests/dispatcher.rs` (they
//! need a live `Store`, a tempdir, and a tokio runtime, so they
//! belong in `tests/`, not `mod tests {}`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use nexus_store::{OutboxRow, OutboxStatus, Store, SuppressionReason};
use nexus_types::AlertEvent;
use tracing::{debug, info, warn};

use crate::{backoff_for, SinkError, SinkId, SinkRegistry};

/// Default time between drain sweeps when the outbox is quiet. The
/// dispatcher also wakes immediately when [`SinkDispatcher::notify`]
/// is signalled (boot kick; eventually a bus subscription on alert
/// publish — wired in Step 5).
pub const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Max outbox rows pulled per sweep. Keeps a single pass bounded so
/// a large backfill (e.g. delivery globally re-enabled after a long
/// outage) doesn't monopolise the task — the next tick picks up the
/// remainder.
pub const BATCH_SIZE: i64 = 64;

/// Verdict from a [`DeliveryPolicy`]. The dispatcher branches on
/// this AFTER reading the outbox row but BEFORE resolving the sink
/// — a `Suppressed` verdict short-circuits HTTP entirely and writes
/// the suppression row directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryVerdict {
    Deliver,
    Suppressed(SuppressionReason),
}

/// Decision point the dispatcher consults for every outbox row.
///
/// Step 3 shipped [`AllowAllPolicy`]; Step 5 replaces it in
/// production with `nexus_sinks::policy::CascadingPolicy` (full
/// `delivery_settings` + per-rule `delivery_policy_json` cascade,
/// see `docs/M7_DELIVERY.md`). Anything that needs to evaluate
/// "should this alert × sink leave the box right now" goes
/// through this trait — there is no other gate.
///
/// The dispatcher fetches the [`AlertEvent`] for the row *before*
/// invoking the policy so the impl can branch on `event.rule_id`
/// (the per-rule policy lookup) and `event.severity` (future
/// extension). The cost is one extra `events` read on the
/// suppression-short-circuit path — acceptable because
/// suppression is the exception, not the norm, and the happy
/// path needs the event anyway for `sink.deliver()`.
#[async_trait]
pub trait DeliveryPolicy: Send + Sync {
    /// Resolve the verdict for one outbox row. Pure read; never
    /// mutates state. The dispatcher passes `now` so unit tests
    /// can pin time, and the already-hydrated `event` so the
    /// impl doesn't have to re-read the store.
    async fn evaluate(
        &self,
        row: &OutboxRow,
        event: &AlertEvent,
        now: chrono::DateTime<Utc>,
    ) -> DeliveryVerdict;
}

/// No-op policy used as the default in M7 Step 3 and as a test
/// fixture. Every row resolves to `Deliver`; the real cascade
/// lives in `nexus_sinks::policy::CascadingPolicy` (Step 5).
pub struct AllowAllPolicy;

#[async_trait]
impl DeliveryPolicy for AllowAllPolicy {
    async fn evaluate(
        &self,
        _row: &OutboxRow,
        _event: &AlertEvent,
        _now: chrono::DateTime<Utc>,
    ) -> DeliveryVerdict {
        DeliveryVerdict::Deliver
    }
}

/// Owned configuration for the dispatcher task. All fields are
/// `Arc`/`Clone` so the spawn site can move into the task body.
#[derive(Clone)]
pub struct SinkDispatcherConfig {
    pub tick_interval: Duration,
    pub batch_size: i64,
    /// Root directory `motion_clips.hot_path` values are relative to.
    /// Threaded through so the dispatcher can resolve a linked clip's
    /// on-disk MP4 path for sinks that attach it (see
    /// [`crate::AlertSink::wants_clip`]). `None` disables clip
    /// resolution — clip-attaching sinks then behave as if the clip
    /// were unavailable.
    pub clips_dir: Option<std::path::PathBuf>,
    /// Directory holding per-event alert snapshots
    /// (`<snapshots_dir>/<event_id>.jpg`). The snapshot is a shared
    /// resource: several sinks (the cloud audit sink that uploads it,
    /// the SureView email sink that attaches it) point at the same
    /// file via `event.artifacts.snapshot`. Reclaim is centralised
    /// here — the file is deleted only once every outbox row for the
    /// event has reached a terminal state, so a slow sink (e.g. an
    /// email sink that defers until the motion clip finalises) never
    /// has the JPEG deleted out from under it. `None` disables
    /// reclaim (the file is simply left on disk).
    pub snapshots_dir: Option<std::path::PathBuf>,
}

impl Default for SinkDispatcherConfig {
    fn default() -> Self {
        Self {
            tick_interval: TICK_INTERVAL,
            batch_size: BATCH_SIZE,
            clips_dir: None,
            snapshots_dir: None,
        }
    }
}

/// Run the dispatcher loop until `shutdown` resolves.
///
/// Shape mirrors `cold_replicator::run_cold_replicator` — a
/// `select!` over the polling timer and the shutdown future, with a
/// boot kick that runs one immediate tick so any backlog from a
/// previous engine run gets attention without waiting for the first
/// poll interval.
pub async fn run_dispatcher(
    cfg: SinkDispatcherConfig,
    store: Arc<Store>,
    registry: Arc<SinkRegistry>,
    policy: Arc<dyn DeliveryPolicy>,
    shutdown: impl std::future::Future<Output = ()>,
) {
    info!(
        tick_ms = cfg.tick_interval.as_millis() as u64,
        batch_size = cfg.batch_size,
        registered_sinks = registry.len(),
        "sink dispatcher starting"
    );

    let mut interval = tokio::time::interval(cfg.tick_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick so the boot pass below is the
    // actual first action (the tokio default fires `tick()` at t=0).
    interval.tick().await;

    // Boot kick — one tick before entering the select loop.
    tick(&cfg, &store, &registry, &*policy).await;

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!("sink dispatcher: shutdown requested");
                return;
            }
            _ = interval.tick() => {
                tick(&cfg, &store, &registry, &*policy).await;
            }
        }
    }
}

/// One drain attempt. Returns nothing — the helper updates the
/// outbox row directly and any error is logged. Each row is
/// independent; we keep ticking even after a malformed row so a
/// single poison-pill can't wedge the queue.
async fn tick(
    cfg: &SinkDispatcherConfig,
    store: &Arc<Store>,
    registry: &Arc<SinkRegistry>,
    policy: &dyn DeliveryPolicy,
) {
    let rows = match store.outbox_pending(cfg.batch_size).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "sink dispatcher: outbox_pending failed");
            return;
        }
    };

    debug!(n = rows.len(), "sink dispatcher: draining batch");

    for row in rows {
        process_row(
            store,
            registry,
            policy,
            cfg.clips_dir.as_deref(),
            cfg.snapshots_dir.as_deref(),
            row,
        )
        .await;
    }
}

/// Reclaim the shared alert snapshot once every sink for this event has
/// reached a terminal outcome.
///
/// The JPEG at `<snapshots_dir>/<event_id>.jpg` is shared by every sink
/// that reads it (the cloud audit sink uploads it; the SureView email
/// sink attaches it). Deleting it per individual delivery races a slower
/// sink — e.g. an email sink that defers until the motion clip finalises
/// would find the file gone. So reclaim is centralised here and only
/// fires when no `pending` rows remain for the event. `None`
/// `snapshots_dir` disables reclaim entirely.
async fn reclaim_snapshot_if_drained(
    store: &Arc<Store>,
    snapshots_dir: Option<&std::path::Path>,
    event_id: &str,
) {
    let Some(dir) = snapshots_dir else {
        return;
    };
    match store.outbox_for_event(event_id).await {
        Ok(rows) => {
            if rows.iter().any(|r| r.status == OutboxStatus::Pending) {
                // Another sink still needs the snapshot; leave it.
                return;
            }
            let path = dir.join(format!("{event_id}.jpg"));
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    debug!(event = %event_id, "reclaimed alert snapshot");
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    debug!(event = %event_id, error = %e, "reclaim snapshot: remove_file failed");
                }
            }
        }
        Err(e) => {
            debug!(event = %event_id, error = %e, "reclaim snapshot: outbox_for_event failed");
        }
    }
}

/// Process a single outbox row. Public so the test crate can drive
/// the state machine without booting `run_dispatcher`'s timer loop.
///
/// Order matters:
///
///   1. Re-hydrate the alert payload from `events.payload_json`.
///      A missing event row means a clip eviction cascaded —
///      terminal-dead, see module doc. Done first because the
///      policy needs `event.rule_id` and the sink needs the
///      payload to deliver; doing it once up front halves the
///      store traffic vs. fetching in two branches.
///   2. Policy gate. Suppressed verdict short-circuits HTTP
///      entirely and writes the suppression row directly.
///   3. Resolve the sink. A missing sink id is treated as `dead`:
///      the row points at a sink the operator has since deleted,
///      and retrying buys nothing.
///   4. Actual delivery.
pub async fn process_row(
    store: &Arc<Store>,
    registry: &Arc<SinkRegistry>,
    policy: &dyn DeliveryPolicy,
    clips_dir: Option<&std::path::Path>,
    snapshots_dir: Option<&std::path::Path>,
    row: OutboxRow,
) {
    // Belt-and-suspenders: outbox_pending should already filter
    // these out, but a custom DeliveryPolicy or future scheduler
    // tweak shouldn't be able to land a non-pending row here.
    if row.status != OutboxStatus::Pending {
        warn!(
            outbox_id = row.id,
            status = ?row.status,
            "sink dispatcher: skipping non-pending row"
        );
        return;
    }

    let now = Utc::now();

    // (1) Re-hydrate the event. Missing → terminal-dead.
    let mut event = match store.get_event(&row.event_id).await {
        Ok(Some(ev)) => ev,
        Ok(None) => {
            mark_dead(
                store,
                snapshots_dir,
                &row,
                format!("event {} missing (likely clip-evicted)", row.event_id),
            )
            .await;
            return;
        }
        Err(e) => {
            warn!(
                outbox_id = row.id,
                error = %e,
                "sink dispatcher: get_event failed; will retry"
            );
            schedule_retry(store, snapshots_dir, &row, &format!("store error: {e}")).await;
            return;
        }
    };

    // (2) Policy gate. Uses event.rule_id for the per-rule cascade.
    if let DeliveryVerdict::Suppressed(reason) = policy.evaluate(&row, &event, now).await {
        if let Err(e) = store.outbox_mark_suppressed(row.id, reason).await {
            warn!(
                outbox_id = row.id,
                error = %e,
                "sink dispatcher: outbox_mark_suppressed failed"
            );
        }
        reclaim_snapshot_if_drained(store, snapshots_dir, &row.event_id).await;
        return;
    }

    // (3) Resolve the sink.
    let sink_id = match SinkId::parse(&row.sink_id) {
        Some(id) => id,
        None => {
            mark_dead(
                store,
                snapshots_dir,
                &row,
                format!("malformed sink_id: {:?}", row.sink_id),
            )
            .await;
            return;
        }
    };
    let sink = match registry.get(&sink_id) {
        Some(s) => s,
        None => {
            mark_dead(
                store,
                snapshots_dir,
                &row,
                format!("no sink registered for {}", row.sink_id),
            )
            .await;
            return;
        }
    };

    // (3.5) Clip resolution. Sinks that attach the surrounding motion
    // clip (e.g. SureView email with `attach_clip`) need the MP4 on
    // disk. The clip is linked to the event AFTER the alert fires and
    // only finishes recording once the post-roll window closes —
    // seconds after the outbox row was enqueued. So for a
    // clip-attaching sink we:
    //   * look up the event's linked clip_id;
    //   * if the clip is still recording (`ended_at IS NULL`),
    //     schedule a transient retry so we wait for the file to
    //     finalise rather than sending a clip-less alarm;
    //   * once closed, stamp the resolved absolute path onto
    //     `event.artifacts.clip` so the sink attaches it.
    // Events with no linked clip fall straight through (nothing to
    // wait for). `payload_json` never carried the clip path because
    // the event is serialised before the clip link, which is exactly
    // why this resolution has to happen here at delivery time.
    if sink.wants_clip() && event.artifacts.clip.is_none() {
        match store.get_event_clip_id(&row.event_id).await {
            Ok(Some(clip_id)) => match store.get_clip(clip_id).await {
                Ok(Some(clip)) => {
                    if clip.ended_at.is_none() {
                        schedule_retry(
                            store,
                            snapshots_dir,
                            &row,
                            &format!("clip {clip_id} still recording; awaiting post-roll close"),
                        )
                        .await;
                        return;
                    }
                    match (clips_dir, clip.hot_path.as_deref()) {
                        (Some(dir), Some(rel)) => {
                            let abs = dir.join(rel);
                            event.artifacts.clip = Some(abs.to_string_lossy().into_owned());
                        }
                        _ => {
                            // Clip closed but no hot copy (soft-evicted to
                            // cold, or clips_dir unset). Deliver without it
                            // rather than blocking the alarm.
                            debug!(
                                outbox_id = row.id,
                                clip_id, "clip has no hot path; delivering without clip"
                            );
                        }
                    }
                }
                Ok(None) => {
                    debug!(
                        outbox_id = row.id,
                        clip_id, "linked clip row missing; delivering without clip"
                    );
                }
                Err(e) => {
                    warn!(
                        outbox_id = row.id,
                        error = %e,
                        "sink dispatcher: get_clip failed; will retry"
                    );
                    schedule_retry(store, snapshots_dir, &row, &format!("get_clip error: {e}"))
                        .await;
                    return;
                }
            },
            Ok(None) => {
                // No clip linked to this alert — nothing to attach.
            }
            Err(e) => {
                warn!(
                    outbox_id = row.id,
                    error = %e,
                    "sink dispatcher: get_event_clip_id failed; will retry"
                );
                schedule_retry(store, snapshots_dir, &row, &format!("clip lookup error: {e}"))
                    .await;
                return;
            }
        }
    }

    // (4) Actual delivery.
    match sink.deliver(&event).await {
        Ok(()) => {
            if let Err(e) = store.outbox_mark_sent(row.id).await {
                warn!(
                    outbox_id = row.id,
                    error = %e,
                    "sink dispatcher: outbox_mark_sent failed"
                );
            }
            reclaim_snapshot_if_drained(store, snapshots_dir, &row.event_id).await;
        }
        Err(SinkError::Permanent(msg)) => {
            // 4xx-class: don't burn retries on something that
            // will never succeed.
            mark_dead(store, snapshots_dir, &row, format!("permanent: {msg}")).await;
        }
        Err(SinkError::Transient(msg)) => {
            schedule_retry(store, snapshots_dir, &row, &format!("transient: {msg}")).await;
        }
    }
}

async fn schedule_retry(
    store: &Arc<Store>,
    snapshots_dir: Option<&std::path::Path>,
    row: &OutboxRow,
    msg: &str,
) {
    // `attempts + 1` is what the column will hold after
    // `outbox_mark_failed` bumps it; backoff_for takes the
    // post-bump value to schedule the next-try delay.
    let next_attempts = (row.attempts as u32).saturating_add(1);
    match backoff_for(next_attempts) {
        Some(delay) => {
            let next_at = Utc::now()
                + chrono::Duration::from_std(delay)
                    .unwrap_or_else(|_| chrono::Duration::seconds(60));
            if let Err(e) = store.outbox_mark_failed(row.id, msg, next_at).await {
                warn!(
                    outbox_id = row.id,
                    error = %e,
                    "sink dispatcher: outbox_mark_failed failed"
                );
            }
        }
        None => {
            // Retries exhausted.
            mark_dead(
                store,
                snapshots_dir,
                row,
                format!("max retries exceeded ({msg})"),
            )
            .await;
        }
    }
}

async fn mark_dead(
    store: &Arc<Store>,
    snapshots_dir: Option<&std::path::Path>,
    row: &OutboxRow,
    msg: String,
) {
    if let Err(e) = store.outbox_mark_dead(row.id, &msg).await {
        warn!(
            outbox_id = row.id,
            error = %e,
            "sink dispatcher: outbox_mark_dead failed"
        );
    }
    reclaim_snapshot_if_drained(store, snapshots_dir, &row.event_id).await;
}
