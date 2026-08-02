//! Remote shell — the edge half of the org-admin-initiated support session.
//!
//! An org admin turns remote shell on for their organization, issues a
//! time-boxed grant naming a recipient, and hands that recipient a
//! one-time link. Only when the recipient actually redeems the link does
//! the cloud send this engine a `shell_session_open`. An unclaimed or
//! abandoned grant therefore never touches the appliance at all.
//!
//! ## What this module refuses to do
//!
//! * **It will not accept a destination from the cloud.** The pipe
//!   terminates at `[remote_access] target` in `nexus.toml` and nowhere
//!   else. The wire payload has no `target` field, so a compromised
//!   control plane cannot use a core as a pivot into its owner's LAN.
//! * **It will not dial a second host.** The side channel must resolve
//!   to the same authority the control tunnel already uses. The
//!   operator's firewall exception was granted for one destination; a
//!   mismatch is refused as `bad_side_channel_host` and logged loudly,
//!   because the only way to see that field differ is for something to
//!   have gone wrong upstream.
//! * **It will not open anything while `enabled = false`.** The org
//!   admin's grant and the box owner's opt-in are both required. Neither
//!   party can act alone.
//! * **It will not outlive its budget.** The session ends at the earlier
//!   of the cloud's `expires_at` and the locally configured
//!   `max_session_secs`, so a wedged broker cannot hold a shell open.
//!
//! Every outcome — including every refusal — reports back as a
//! `shell_session_closed` so the console can close out the row rather
//! than leaving an operator staring at a session that appears live.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use nexus_cloud_client::{
    pump_shell, RejectReason, ShellStop, TunnelClient, TunnelOutbox, Verifier,
};
use nexus_cloud_protocol::v1::{
    Envelope, EnvelopeBody, EnvelopeMeta, ShellSessionClosePayload, ShellSessionClosedPayload,
    ShellSessionOpenPayload,
};
use nexus_config::RemoteAccessConfig;
use parking_lot::Mutex;
use tokio::net::TcpStream;
use tracing::{info, warn};
use uuid::Uuid;

/// Reasons the engine reports on `shell_session_closed`. String
/// constants rather than an enum because the wire type is a plain
/// `String` and the cloud's set is authoritative.
mod reason {
    pub const RECIPIENT_DISCONNECT: &str = "recipient_disconnect";
    pub const OPERATOR_KILL: &str = "operator_kill";
    pub const EXPIRED: &str = "expired";
    pub const BYTE_LIMIT: &str = "byte_limit";
    pub const DISABLED_ON_CORE: &str = "disabled_on_core";
    pub const ACTOR_TOKEN_INVALID: &str = "actor_token_invalid";
    pub const BAD_SIDE_CHANNEL_HOST: &str = "bad_side_channel_host";
    pub const SSHD_UNREACHABLE: &str = "sshd_unreachable";
    pub const SIDE_CHANNEL_FAILED: &str = "side_channel_failed";
    pub const CORE_SHUTDOWN: &str = "core_shutdown";
}

/// How a live session was asked to stop.
#[derive(Debug, Clone, Copy)]
enum Stop {
    /// Cloud kill switch.
    Operator,
    /// Engine shutting down or tunnel gone.
    Shutdown,
}

/// Owns every live remote-shell session on this engine.
pub struct RemoteShellManager {
    cfg: RemoteAccessConfig,
    tunnel: TunnelClient,
    verifier: Option<Verifier>,
    outbox: Arc<TunnelOutbox>,
    live: Mutex<HashMap<String, LiveSession>>,
}

/// Cancellation handle plus the reason the cancel was requested. The
/// channel itself carries no payload so the pump in `nexus-cloud-client`
/// stays transport-only; the engine remembers *why* on this side.
struct LiveSession {
    cancel: tokio::sync::mpsc::Sender<()>,
    stop: Arc<Mutex<Option<Stop>>>,
}

impl std::fmt::Debug for RemoteShellManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteShellManager")
            .field("enabled", &self.cfg.enabled)
            .field("live", &self.live.lock().len())
            .finish()
    }
}

impl RemoteShellManager {
    /// Build a manager. `verifier` is `None` in heartbeat-only mode
    /// (no enrollment signing key); every open is then refused, because
    /// an unverifiable grant is indistinguishable from a forged one.
    #[must_use]
    pub fn new(
        cfg: RemoteAccessConfig,
        tunnel: TunnelClient,
        verifier: Option<Verifier>,
        outbox: Arc<TunnelOutbox>,
    ) -> Self {
        if cfg.enabled {
            info!(
                target = %cfg.target,
                max_session_secs = cfg.max_session_secs,
                "remote shell is ENABLED on this appliance",
            );
        }
        Self {
            cfg,
            tunnel,
            verifier,
            outbox,
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Whether the box owner opted this appliance in. Read by the tunnel
    /// supervisor to decide whether to adopt the cloud's SSH CA at all — a
    /// core with remote access off never installs `TrustedUserCAKeys`, so
    /// the capability is absent rather than merely unused.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Push the current live-session set to sshd's `AuthorizedPrincipalsFile`.
    ///
    /// The certificate the cloud mints for a session carries that session's
    /// UUID as its only principal, so this file is the revocation surface:
    /// dropping a UUID here refuses the next authentication attempt even
    /// though the certificate is still cryptographically valid and unexpired.
    /// Called on every open and every close, including refusals, so a crashed
    /// or force-killed session can never leave a usable principal behind.
    ///
    /// Fire-and-forget on purpose. The byte pump must not wait on a `sudo`
    /// round trip, and a failure here is fail-*closed* for new logins in the
    /// open direction (no principal → no login) while an already-established
    /// TCP session is still killed by the pump's own cancellation path.
    fn resync_principals(&self) {
        let live: std::collections::BTreeSet<String> = self.live.lock().keys().cloned().collect();
        tokio::spawn(async move {
            if let Err(e) = crate::ssh_ca::sync_principals(live).await {
                warn!(
                    reason = e.code(),
                    "remote access: could not sync session principals"
                );
            }
        });
    }

    /// Handle an inbound `shell_session_open`.
    ///
    /// Returns immediately; the session itself runs on a detached task
    /// because it outlives the 30-second token that authorised it.
    pub fn on_open(self: &Arc<Self>, payload: ShellSessionOpenPayload) {
        let session_id = payload.session_id.clone();

        if let Err((why, detail)) = self.admit(&payload, &session_id) {
            warn!(
                session_id = %session_id,
                reason = why,
                detail = %detail,
                "remote shell session refused",
            );
            let this = Arc::clone(self);
            let sid = session_id.clone();
            tokio::spawn(async move {
                this.report_closed(&sid, why, 0, 0, Some(detail)).await;
            });
            return;
        }

        let (cancel_tx, cancel_rx) = tokio::sync::mpsc::channel::<()>(1);
        let stop_slot = Arc::new(Mutex::new(None));
        {
            let mut live = self.live.lock();
            if live.contains_key(&session_id) {
                // Duplicate open for a session already running. Ignore
                // rather than tear the good one down.
                warn!(session_id = %session_id, "duplicate shell_session_open ignored");
                return;
            }
            live.insert(
                session_id.clone(),
                LiveSession {
                    cancel: cancel_tx,
                    stop: Arc::clone(&stop_slot),
                },
            );
        }
        // Authorise this session's principal BEFORE announcing the session as
        // open. The recipient still has to paste an ssh command, so the
        // applier round trip is never on a latency-sensitive path.
        self.resync_principals();

        info!(
            session_id = %session_id,
            target = %self.cfg.target,
            "remote shell session opening",
        );

        let this = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = this.run(&payload, cancel_rx, &stop_slot).await;
            this.live.lock().remove(&outcome.session_id);
            this.resync_principals();
            info!(
                session_id = %outcome.session_id,
                reason = outcome.reason,
                bytes_up = outcome.bytes_up,
                bytes_down = outcome.bytes_down,
                "remote shell session closed",
            );
            this.report_closed(
                &outcome.session_id,
                outcome.reason,
                outcome.bytes_up,
                outcome.bytes_down,
                outcome.detail,
            )
            .await;
        });
    }

    /// Handle an inbound `shell_session_close` (the console's kill
    /// switch). Idempotent: an unknown session is a no-op, because the
    /// operator pressing "End now" twice should not produce an error.
    pub fn on_close(&self, payload: &ShellSessionClosePayload) {
        self.stop_session(&payload.session_id, Stop::Operator);
    }

    /// Tear down everything. Called when the control tunnel drops or the
    /// engine is shutting down: a shell whose control plane has gone
    /// away can no longer be revoked, so it does not get to keep
    /// running.
    pub fn close_all(&self) {
        let ids: Vec<String> = self.live.lock().keys().cloned().collect();
        for id in ids {
            self.stop_session(&id, Stop::Shutdown);
        }
    }

    fn stop_session(&self, session_id: &str, why: Stop) {
        let Some((cancel, slot)) = self
            .live
            .lock()
            .get(session_id)
            .map(|s| (s.cancel.clone(), Arc::clone(&s.stop)))
        else {
            return;
        };
        *slot.lock() = Some(why);
        let _ = cancel.try_send(());
    }

    /// Every check that must pass before a pipe is opened. Returns the
    /// wire `close_reason` and an operator-facing detail on refusal.
    fn admit(
        &self,
        payload: &ShellSessionOpenPayload,
        session_id: &str,
    ) -> Result<(), (&'static str, String)> {
        if !self.cfg.enabled {
            return Err((
                reason::DISABLED_ON_CORE,
                "[remote_access] enabled = false on this appliance".to_string(),
            ));
        }

        let Some(verifier) = self.verifier.as_ref() else {
            return Err((
                reason::ACTOR_TOKEN_INVALID,
                "no enrollment signing key; cannot verify the grant".to_string(),
            ));
        };
        // Verify BEFORE the host check so a forged envelope never learns
        // anything about our configuration from the error it gets back.
        if let Err(e) = verifier.verify_shell(&payload.actor_token, session_id) {
            let detail = match e {
                RejectReason::Invalid(r) => format!("actor_token invalid: {r:?}"),
                other => format!("actor_token rejected: {other}"),
            };
            return Err((reason::ACTOR_TOKEN_INVALID, detail));
        }

        // Refuse a second host outright rather than dialling it and
        // seeing what happens.
        let expected = self.tunnel.gateway_authority().unwrap_or_default();
        let actual = payload
            .side_channel_url
            .split_once("://")
            .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if actual.is_empty() || actual != expected {
            return Err((
                reason::BAD_SIDE_CHANNEL_HOST,
                format!("side channel host `{actual}` is not the control tunnel's"),
            ));
        }

        Ok(())
    }

    /// Run one session to completion.
    async fn run(
        &self,
        payload: &ShellSessionOpenPayload,
        mut cancel_rx: tokio::sync::mpsc::Receiver<()>,
        stop_slot: &Mutex<Option<Stop>>,
    ) -> Outcome {
        let session_id = payload.session_id.clone();
        let deadline = self.deadline(payload.expires_at.as_str());

        let ws = match self
            .tunnel
            .connect_side_channel(&payload.side_channel_url)
            .await
        {
            Ok(ws) => ws,
            Err(e) => {
                return Outcome::failed(
                    session_id,
                    reason::SIDE_CHANNEL_FAILED,
                    format!("side channel dial failed: {e}"),
                )
            }
        };

        let tcp = match TcpStream::connect(&self.cfg.target).await {
            Ok(s) => s,
            Err(e) => {
                return Outcome::failed(
                    session_id,
                    reason::SSHD_UNREACHABLE,
                    format!("cannot reach {}: {}", self.cfg.target, e.kind()),
                )
            }
        };

        let tally = pump_shell(ws, tcp, deadline, payload.max_bytes, &mut cancel_rx).await;

        let reason = match tally.stop {
            ShellStop::PeerClosed => reason::RECIPIENT_DISCONNECT,
            ShellStop::Expired => reason::EXPIRED,
            ShellStop::ByteLimit => reason::BYTE_LIMIT,
            ShellStop::LocalIoError => reason::SSHD_UNREACHABLE,
            ShellStop::Cancelled => match *stop_slot.lock() {
                Some(Stop::Shutdown) => reason::CORE_SHUTDOWN,
                Some(Stop::Operator) | None => reason::OPERATOR_KILL,
            },
        };

        Outcome {
            session_id,
            reason,
            bytes_up: tally.bytes_up,
            bytes_down: tally.bytes_down,
            detail: None,
        }
    }

    /// The instant this session must end: the earlier of the cloud's
    /// deadline and the locally configured ceiling. An unparseable
    /// cloud timestamp falls back to the local ceiling rather than to
    /// "forever".
    fn deadline(&self, expires_at: &str) -> tokio::time::Instant {
        let local_cap =
            tokio::time::Instant::now() + Duration::from_secs(self.cfg.max_session_secs);
        let Ok(cloud) = DateTime::parse_from_rfc3339(expires_at) else {
            return local_cap;
        };
        let remaining = cloud.with_timezone(&Utc) - Utc::now();
        let Ok(remaining) = remaining.to_std() else {
            // Already past — end as soon as the loop runs.
            return tokio::time::Instant::now();
        };
        local_cap.min(tokio::time::Instant::now() + remaining)
    }

    /// Emit the terminal `shell_session_closed`. Best-effort: if the
    /// tunnel is down the cloud reconciles the row on its own expiry
    /// sweep, and there is nothing useful to retry against.
    async fn report_closed(
        &self,
        session_id: &str,
        reason: &str,
        bytes_up: u64,
        bytes_down: u64,
        error_message: Option<String>,
    ) {
        let env = Envelope {
            meta: EnvelopeMeta {
                v: 1,
                id: Uuid::now_v7().to_string(),
                ts: Utc::now().to_rfc3339(),
                in_reply_to: None,
                seq: None,
                trace: None,
            },
            body: EnvelopeBody::ShellSessionClosed(ShellSessionClosedPayload {
                bytes_down: Some(bytes_down),
                bytes_up: Some(bytes_up),
                error_message,
                reason: reason.to_string(),
                session_id: session_id.to_string(),
            }),
        };
        if let Err(e) = self.outbox.send(env).await {
            warn!(session_id = %session_id, error = %e, "shell_session_closed not delivered");
        }
    }
}

/// How a session ended.
struct Outcome {
    session_id: String,
    reason: &'static str,
    bytes_up: u64,
    bytes_down: u64,
    detail: Option<String>,
}

impl Outcome {
    fn failed(session_id: String, reason: &'static str, detail: String) -> Self {
        Self {
            session_id,
            reason,
            bytes_up: 0,
            bytes_down: 0,
            detail: Some(detail),
        }
    }
}

/// Response body for `POST /api/v1/admin/remote-access/restart-sshd`.
#[derive(Debug, serde::Serialize)]
pub struct RestartSshdResponse {
    /// Always `true` on the success path — present so the console can
    /// distinguish "we restarted it" from a 2xx with no effect.
    pub restarted: bool,
}

/// `POST /v1/admin/remote-access/restart-sshd` — operator recovery for a
/// wedged sshd, reached from the cloud console as an `rpc_call`.
///
/// This is intentionally the *only* sshd control surface exposed to the
/// cloud. It cannot change configuration, cannot enable remote access, and
/// cannot create logins; the privileged applier validates the config with
/// `sshd -t` and refuses to restart a broken one, so the worst outcome of a
/// spurious call is a few seconds of dropped SSH connectivity — never a
/// lockout.
///
/// # Errors
/// * `403` when remote access is disabled on this appliance. A core whose
///   owner never opted in does not expose an sshd control at all.
/// * `500` when the privileged applier is missing or the restart fails.
pub async fn post_admin_restart_sshd(
    axum::extract::State(s): axum::extract::State<crate::api::ApiState>,
    headers: axum::http::HeaderMap,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    session: Option<crate::auth::require_role::SessionContext>,
) -> Result<axum::Json<RestartSshdResponse>, crate::api::ApiError> {
    let enabled = s.remote_access_enabled;
    let result = if enabled {
        crate::ssh_ca::restart_sshd().await.map_err(|e| {
            crate::api::ApiError(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("sshd restart failed: {}", e.code()),
            )
        })
    } else {
        Err(crate::api::ApiError(
            axum::http::StatusCode::FORBIDDEN,
            "remote access is disabled on this appliance".to_string(),
        ))
    };
    let outcome = if result.is_ok() {
        nexus_store::audit::AuditOutcome::Success
    } else {
        nexus_store::audit::AuditOutcome::Failure
    };
    crate::auth::admin_audit::audit_admin_action(
        &s.store,
        session.as_ref(),
        &headers,
        peer.ip(),
        "remote_access.sshd.restart",
        "remote_access",
        None,
        outcome,
        None,
        None,
    )
    .await;
    result?;
    info!("remote access: sshd restarted on operator request");
    Ok(axum::Json(RestartSshdResponse { restarted: true }))
}
