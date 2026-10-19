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
//! * **It will not open anything while remote access is off.** The live
//!   switch is [`RemoteAccess`]: `[remote_access] enabled` at boot, folded
//!   with the cloud's persisted toggle. The cloud may switch it on unless
//!   the box owner set `cloud_may_enable = false`, and may always switch
//!   it off. An org admin's per-session grant is required on top.
//! * **It will not pipe to a non-local target.** `[remote_access] target`
//!   is only validated at load when `enabled = true`; a cloud-enabled box
//!   re-checks it on every open.
//! * **It will not outlive its budget.** The session ends at the earlier
//!   of the cloud's `expires_at` and the locally configured
//!   `max_session_secs`, so a wedged broker cannot hold a shell open.
//!
//! Every outcome — including every refusal — reports back as a
//! `shell_session_closed` so the console can close out the row rather
//! than leaving an operator staring at a session that appears live.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
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
    /// Remote access switched off on this core.
    Disabled,
}

/// The wire `close_reason` for a session the engine cancelled itself.
const fn cancelled_reason(stop: Option<Stop>) -> &'static str {
    match stop {
        Some(Stop::Shutdown) => reason::CORE_SHUTDOWN,
        Some(Stop::Disabled) => reason::DISABLED_ON_CORE,
        Some(Stop::Operator) | None => reason::OPERATOR_KILL,
    }
}

/// The live `[remote_access]` switch — the ONE answer to "is remote access
/// on right now?". Built in `main.rs` before either consumer exists and
/// shared by the tunnel's [`RemoteShellManager`], the RPC handler's
/// owner-only toggle, and `ApiState`.
///
/// The manager is only built once the tunnel has an enrollment, so it
/// attaches itself here afterwards ([`Self::attach`]); until then a toggle
/// just flips the flag, which is all there is to apply.
pub struct RemoteAccess {
    cfg: RemoteAccessConfig,
    enabled: AtomicBool,
    attached: Mutex<Option<Attached>>,
    /// Held across persist + [`Self::set_enabled`] by the cloud toggle so
    /// two concurrent PUTs cannot leave the persisted and live values
    /// disagreeing.
    toggle: tokio::sync::Mutex<()>,
}

/// What the tunnel hands over once it has an enrollment.
struct Attached {
    /// Weak: the manager holds an `Arc` to this switch.
    manager: Weak<RemoteShellManager>,
    /// The enrollment's SSH CA public key, if the cloud shipped one.
    ssh_ca: Option<String>,
}

/// Body of `GET /api/v1/admin/remote_access` and of the cloud's
/// `PUT /admin/remote_access` reply.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct RemoteAccessState {
    pub enabled: bool,
    pub cloud_may_enable: bool,
}

impl std::fmt::Debug for RemoteAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteAccess")
            .field("enabled", &self.is_enabled())
            .field("cloud_may_enable", &self.cfg.cloud_may_enable)
            .finish_non_exhaustive()
    }
}

impl RemoteAccess {
    /// `enabled` is the boot-time effective value — see
    /// [`crate::admin_runtime::effective_remote_access_enabled`].
    #[must_use]
    pub fn new(cfg: RemoteAccessConfig, enabled: bool) -> Self {
        if enabled {
            info!(
                target = %cfg.target,
                max_session_secs = cfg.max_session_secs,
                "remote shell is ENABLED on this appliance",
            );
        }
        Self {
            cfg,
            enabled: AtomicBool::new(enabled),
            attached: Mutex::new(None),
            toggle: tokio::sync::Mutex::new(()),
        }
    }

    /// Serialise a toggle: hold the guard across persisting the choice and
    /// applying it with [`Self::set_enabled`].
    pub async fn lock_toggle(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.toggle.lock().await
    }

    /// Whether remote access is on right now.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// `[remote_access] cloud_may_enable` — `false` means the box owner
    /// opted out of the cloud switching this on.
    #[must_use]
    pub const fn cloud_may_enable(&self) -> bool {
        self.cfg.cloud_may_enable
    }

    /// The `nexus.toml` block (target, session ceiling).
    #[must_use]
    pub const fn config(&self) -> &RemoteAccessConfig {
        &self.cfg
    }

    #[must_use]
    pub fn state(&self) -> RemoteAccessState {
        RemoteAccessState {
            enabled: self.is_enabled(),
            cloud_may_enable: self.cfg.cloud_may_enable,
        }
    }

    /// Apply a toggle live. The caller has already authorised it and
    /// enforced `cloud_may_enable`.
    ///
    /// Off tears down every live session. On acts only on the off→on
    /// transition — an enable while already on is a no-op, because the
    /// CA installer truncates the principal file and would revoke a
    /// session still waiting to authenticate. On that transition the
    /// cloud's SSH CA is adopted ([`RemoteShellManager::adopt_ssh_ca`])
    /// BEFORE the switch is published: while it is still off no session
    /// can be admitted, so the installer cannot wipe a live principal.
    /// Adoption failure is logged and the switch is still turned on. With
    /// no manager attached yet only the flag flips; [`Self::attach`]
    /// adopts the CA then.
    pub async fn set_enabled(&self, enabled: bool) {
        let (manager, ssh_ca) = match self.attached.lock().as_ref() {
            Some(a) => (a.manager.upgrade(), a.ssh_ca.clone()),
            None => (None, None),
        };
        if !enabled {
            self.enabled.store(false, Ordering::SeqCst);
            if let Some(m) = manager {
                m.stop_all(Stop::Disabled);
            }
            return;
        }
        if self.is_enabled() {
            return;
        }
        if let Some(m) = manager {
            m.adopt_ssh_ca(ssh_ca).await;
        }
        self.enabled.store(true, Ordering::SeqCst);
    }

    /// Called once by the tunnel after it builds the manager. Adopts the
    /// SSH CA immediately when remote access is already on.
    pub fn attach(&self, manager: &Arc<RemoteShellManager>, ssh_ca: Option<String>) {
        *self.attached.lock() = Some(Attached {
            manager: Arc::downgrade(manager),
            ssh_ca: ssh_ca.clone(),
        });
        if self.is_enabled() {
            let m = Arc::clone(manager);
            tokio::spawn(async move { m.adopt_ssh_ca(ssh_ca).await });
        }
    }
}

/// The two privileged operations that write sshd's principal file. A
/// trait only so tests can observe their order; production is
/// [`HostApplier`].
#[async_trait::async_trait]
trait SshApplier: Send + Sync {
    async fn install_ca(&self, ca: &str) -> Result<(), crate::ssh_ca::SshCaError>;
    async fn sync_principals(
        &self,
        live: std::collections::BTreeSet<String>,
    ) -> Result<(), crate::ssh_ca::SshCaError>;
}

/// [`crate::ssh_ca`] through the root-owned applier.
struct HostApplier;

#[async_trait::async_trait]
impl SshApplier for HostApplier {
    async fn install_ca(&self, ca: &str) -> Result<(), crate::ssh_ca::SshCaError> {
        crate::ssh_ca::install_ca(ca).await
    }
    async fn sync_principals(
        &self,
        live: std::collections::BTreeSet<String>,
    ) -> Result<(), crate::ssh_ca::SshCaError> {
        crate::ssh_ca::sync_principals(live).await
    }
}

/// Owns every live remote-shell session on this engine.
pub struct RemoteShellManager {
    access: Arc<RemoteAccess>,
    tunnel: TunnelClient,
    verifier: Option<Verifier>,
    outbox: Arc<TunnelOutbox>,
    live: Mutex<HashMap<String, LiveSession>>,
    applier: Arc<dyn SshApplier>,
    /// Held by every principal-file writer — CA install and principal
    /// sync — so they never interleave and each sync snapshots `live`
    /// at write time.
    principals_writer: tokio::sync::Mutex<()>,
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
            .field("enabled", &self.access.is_enabled())
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
        access: Arc<RemoteAccess>,
        tunnel: TunnelClient,
        verifier: Option<Verifier>,
        outbox: Arc<TunnelOutbox>,
    ) -> Self {
        Self {
            access,
            tunnel,
            verifier,
            outbox,
            live: Mutex::new(HashMap::new()),
            applier: Arc::new(HostApplier),
            principals_writer: tokio::sync::Mutex::new(()),
        }
    }

    /// Swap the privileged applier for a test double.
    #[cfg(test)]
    fn with_applier(mut self, applier: Arc<dyn SshApplier>) -> Self {
        self.applier = applier;
        self
    }

    /// Whether remote access is on right now — a live read of the shared
    /// [`RemoteAccess`] switch, never a boot snapshot.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.access.is_enabled()
    }

    fn cfg(&self) -> &RemoteAccessConfig {
        self.access.config()
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
    ///
    /// The write runs under `principals_writer` and snapshots the live set
    /// only once it holds it, so whichever write lands last is the current
    /// set — never a stale spawn-time snapshot, never undone by a CA install.
    fn resync_principals(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let _writer = this.principals_writer.lock().await;
            this.write_principals().await;
        });
    }

    /// Write the current live set. Caller holds `principals_writer`.
    async fn write_principals(&self) {
        let live: std::collections::BTreeSet<String> = self.live.lock().keys().cloned().collect();
        if let Err(e) = self.applier.sync_principals(live).await {
            warn!(
                reason = e.code(),
                "remote access: could not sync session principals"
            );
        }
    }

    /// Adopt the cloud's SSH certificate authority. Idempotent, so it also
    /// repairs a drop-in an OS-level sshd reconfiguration removed. The
    /// installer truncates the principal file, so the install and an
    /// authoritative re-write of the live set both run under
    /// `principals_writer`: a session that opened meanwhile keeps its
    /// principal. Failure is logged, never returned — a box with no sshd
    /// must not block the tunnel or the toggle; remote shell just stays
    /// unavailable and every later session open reports the failure.
    async fn adopt_ssh_ca(&self, ssh_ca: Option<String>) {
        let Some(ca) = ssh_ca else {
            warn!(
                "remote access is enabled but this enrollment carries no SSH CA; \
                 re-enroll against a cloud that has one provisioned"
            );
            return;
        };
        let _writer = self.principals_writer.lock().await;
        match self.applier.install_ca(&ca).await {
            Ok(()) => info!("remote access: SSH CA adopted"),
            Err(e) => warn!(
                reason = e.code(),
                "remote access: SSH CA adoption failed; native sessions will be refused"
            ),
        }
        self.write_principals().await;
    }

    /// Handle an inbound `shell_session_open`.
    ///
    /// Returns immediately; the session itself runs on a detached task
    /// because it outlives the 30-second token that authorised it.
    pub fn on_open(self: &Arc<Self>, payload: ShellSessionOpenPayload) {
        let session_id = payload.session_id.clone();

        if let Err((why, detail)) = self.admit(&payload, &session_id) {
            self.refuse(&session_id, why, detail);
            return;
        }
        self.start(payload);
    }

    /// Log a refusal and report it to the cloud.
    fn refuse(self: &Arc<Self>, session_id: &str, why: &'static str, detail: String) {
        warn!(
            session_id = %session_id,
            reason = why,
            detail = %detail,
            "remote shell session refused",
        );
        let this = Arc::clone(self);
        let sid = session_id.to_string();
        tokio::spawn(async move {
            this.report_closed(&sid, why, 0, 0, Some(detail)).await;
        });
    }

    /// Register an admitted session and run it on a detached task.
    fn start(self: &Arc<Self>, payload: ShellSessionOpenPayload) {
        let session_id = payload.session_id.clone();
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
        // A live disable that landed between `admit` and the insert above
        // ran its close before this session existed; catch it here, before
        // any principal is authorised or anything is dialled.
        if !self.is_enabled() {
            self.live.lock().remove(&session_id);
            self.refuse(
                &session_id,
                reason::DISABLED_ON_CORE,
                "remote access was disabled on this appliance while the session was opening"
                    .to_string(),
            );
            return;
        }
        // Authorise this session's principal BEFORE announcing the session as
        // open. The recipient still has to paste an ssh command, so the
        // applier round trip is never on a latency-sensitive path.
        self.resync_principals();

        info!(
            session_id = %session_id,
            target = %self.cfg().target,
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
        self.stop_all(Stop::Shutdown);
    }

    fn stop_all(&self, why: Stop) {
        let ids: Vec<String> = self.live.lock().keys().cloned().collect();
        for id in ids {
            self.stop_session(&id, why);
        }
    }

    /// The `close_reason` a cancelled test session would report.
    #[cfg(test)]
    pub(crate) fn test_stop_reason(&self, session_id: &str) -> Option<&'static str> {
        let slot = Arc::clone(&self.live.lock().get(session_id)?.stop);
        let stop = *slot.lock();
        stop.map(|s| cancelled_reason(Some(s)))
    }

    /// Register a stand-in live session and hand back its cancel receiver,
    /// so a test can observe [`Self::close_all`] without a real tunnel.
    #[cfg(test)]
    pub(crate) fn insert_test_session(&self, session_id: &str) -> tokio::sync::mpsc::Receiver<()> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        self.live.lock().insert(
            session_id.to_string(),
            LiveSession {
                cancel: tx,
                stop: Arc::new(Mutex::new(None)),
            },
        );
        rx
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
        if !self.is_enabled() {
            return Err((
                reason::DISABLED_ON_CORE,
                "remote access is disabled on this appliance".to_string(),
            ));
        }
        // Load-time validation only covers `enabled = true` in nexus.toml;
        // a cloud-enabled box re-checks here so it can never become a relay.
        if !self.cfg().target_is_local() {
            return Err((
                reason::DISABLED_ON_CORE,
                "[remote_access] target is not a loopback or private address".to_string(),
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
        let actual = side_channel_host(&payload.side_channel_url);
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

        let tcp = match TcpStream::connect(&self.cfg().target).await {
            Ok(s) => s,
            Err(e) => {
                return Outcome::failed(
                    session_id,
                    reason::SSHD_UNREACHABLE,
                    format!("cannot reach {}: {}", self.cfg().target, e.kind()),
                )
            }
        };

        let tally = pump_shell(ws, tcp, deadline, payload.max_bytes, &mut cancel_rx).await;

        let reason = match tally.stop {
            ShellStop::PeerClosed => reason::RECIPIENT_DISCONNECT,
            ShellStop::Expired => reason::EXPIRED,
            ShellStop::ByteLimit => reason::BYTE_LIMIT,
            ShellStop::LocalIoError => reason::SSHD_UNREACHABLE,
            ShellStop::Cancelled => cancelled_reason(*stop_slot.lock()),
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
            tokio::time::Instant::now() + Duration::from_secs(self.cfg().max_session_secs);
        let Ok(remaining) = remaining_until(expires_at, Utc::now()) else {
            return local_cap;
        };
        let Some(remaining) = remaining else {
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
    let enabled = s.remote_access.is_enabled();
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

/// `GET /v1/admin/remote_access` — the live switch and the box owner's
/// `cloud_may_enable` opt-out. Read-only by design: the only setter is the
/// owner-role `PUT /admin/remote_access` arm in
/// [`crate::engine_rpc::EngineRpcHandler`], reachable solely through the
/// verified cloud RPC tunnel, so there is no local HTTP route for it.
pub async fn get_admin_remote_access(
    axum::extract::State(s): axum::extract::State<crate::api::ApiState>,
) -> axum::Json<RemoteAccessState> {
    axum::Json(s.remote_access.state())
}

/// The authority (`host[:port]`) a side-channel URL would dial, lower-cased.
///
/// Pulled out of [`ShellSupervisor::admit`] because it is the whole of the
/// "no second host" invariant: the engine opens exactly one outbound
/// destination, and a `shell_session_open` that names any other one is
/// refused rather than followed. Returns an empty string for anything it
/// cannot read as a URL, which `admit` treats as a refusal — an
/// unparseable destination is not a safe destination.
fn side_channel_host(url: &str) -> String {
    url.split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// How much of the cloud's deadline is left.
///
/// `Err(())` means the timestamp could not be read at all; the caller
/// falls back to the local ceiling rather than to "forever". `Ok(None)`
/// means the deadline has already passed.
fn remaining_until(expires_at: &str, now: DateTime<Utc>) -> Result<Option<Duration>, ()> {
    let cloud = DateTime::parse_from_rfc3339(expires_at).map_err(|_| ())?;
    Ok((cloud.with_timezone(&Utc) - now).to_std().ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod tests {
    use super::{remaining_until, side_channel_host};
    use super::{RemoteAccess, RemoteShellManager};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A manager over `access`, wired as the tunnel wires it.
    pub(crate) fn attached_manager(access: &Arc<RemoteAccess>) -> Arc<RemoteShellManager> {
        manager_with_outbox(access, Arc::new(nexus_cloud_client::TunnelOutbox::new()))
    }

    fn manager_with_outbox(
        access: &Arc<RemoteAccess>,
        outbox: Arc<nexus_cloud_client::TunnelOutbox>,
    ) -> Arc<RemoteShellManager> {
        let manager = Arc::new(RemoteShellManager::new(
            Arc::clone(access),
            nexus_cloud_client::tunnel::TunnelClient::new(
                "wss://cloud.invalid/v1/tunnel",
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
            None,
            outbox,
        ));
        access.attach(&manager, None);
        manager
    }

    /// Records every envelope the manager reports.
    #[derive(Default)]
    struct CapturingTunnel {
        sent: parking_lot::Mutex<Vec<nexus_cloud_protocol::v1::Envelope>>,
    }
    #[async_trait::async_trait]
    impl nexus_cloud_client::TunnelHandle for CapturingTunnel {
        async fn send(
            &self,
            envelope: nexus_cloud_protocol::v1::Envelope,
        ) -> Result<(), nexus_cloud_client::TunnelError> {
            self.sent.lock().push(envelope);
            Ok(())
        }
    }

    /// The switch is live: the manager, the RPC toggle and the local API
    /// all read the same flag, so a toggle is visible everywhere at once
    /// without a restart.
    #[tokio::test]
    async fn remote_access_live_toggle_flips_the_manager() {
        let access = Arc::new(RemoteAccess::new(
            nexus_config::RemoteAccessConfig::default(),
            false,
        ));
        let manager = attached_manager(&access);
        assert!(!manager.is_enabled());

        access.set_enabled(true).await;
        assert!(manager.is_enabled());
        assert!(access.state().enabled);
        assert!(access.state().cloud_may_enable);

        access.set_enabled(false).await;
        assert!(!manager.is_enabled());
    }

    /// Switching off is a kill switch, not just a gate on the next open: a
    /// session already running is torn down.
    #[tokio::test]
    async fn remote_access_disable_closes_live_sessions() {
        let access = Arc::new(RemoteAccess::new(
            nexus_config::RemoteAccessConfig::default(),
            true,
        ));
        let manager = attached_manager(&access);
        let mut cancel = manager.insert_test_session("s-1");

        access.set_enabled(false).await;
        assert!(
            cancel.try_recv().is_ok(),
            "disabling must cancel the live session"
        );
        assert_eq!(
            manager.test_stop_reason("s-1"),
            Some(super::reason::DISABLED_ON_CORE),
            "a live disable reports disabled_on_core, not core_shutdown"
        );
    }

    /// The tunnel going away is still a shutdown, not a disable.
    #[tokio::test]
    async fn remote_access_tunnel_close_all_reports_core_shutdown() {
        let access = Arc::new(RemoteAccess::new(
            nexus_config::RemoteAccessConfig::default(),
            true,
        ));
        let manager = attached_manager(&access);
        let mut cancel = manager.insert_test_session("s-1");

        manager.close_all();
        assert!(cancel.try_recv().is_ok());
        assert_eq!(
            manager.test_stop_reason("s-1"),
            Some(super::reason::CORE_SHUTDOWN)
        );
    }

    /// A disable that lands between `admit` and registration: the session
    /// is dropped and reported as `disabled_on_core` before anything is
    /// authorised or dialled.
    #[tokio::test]
    async fn remote_access_disabled_after_admit_stops_before_dialing() {
        let access = Arc::new(RemoteAccess::new(
            nexus_config::RemoteAccessConfig::default(),
            false,
        ));
        let tunnel = Arc::new(CapturingTunnel::default());
        let outbox = Arc::new(nexus_cloud_client::TunnelOutbox::new());
        outbox.set_handle(Some(tunnel.clone()));
        let manager = manager_with_outbox(&access, outbox);

        manager.start(nexus_cloud_protocol::v1::ShellSessionOpenPayload {
            actor_token: String::new(),
            expires_at: "2099-01-01T00:00:00Z".to_string(),
            max_bytes: None,
            session_id: "s-race".to_string(),
            side_channel_url: "wss://cloud.invalid/shell/s-race".to_string(),
        });

        assert!(
            manager.live.lock().is_empty(),
            "session not left registered"
        );
        // Only the refusal report runs; give it a chance to land.
        for _ in 0..100 {
            if !tunnel.sent.lock().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let sent = tunnel.sent.lock();
        assert_eq!(sent.len(), 1, "exactly one report, no session run");
        match &sent[0].body {
            nexus_cloud_protocol::v1::EnvelopeBody::ShellSessionClosed(p) => {
                assert_eq!(p.session_id, "s-race");
                assert_eq!(p.reason, super::reason::DISABLED_ON_CORE);
                assert_eq!(p.bytes_up, Some(0));
            }
            other => panic!("unexpected envelope: {other:?}"),
        }
    }

    /// Stands in for the root-owned applier. Models sshd's principal file
    /// — the CA installer truncates it, a sync replaces it — and records
    /// whether the switch was already on while each install ran.
    #[derive(Default)]
    struct FakeApplier {
        access: parking_lot::Mutex<Option<std::sync::Weak<RemoteAccess>>>,
        installs: parking_lot::Mutex<Vec<bool>>,
        file: parking_lot::Mutex<BTreeSet<String>>,
        syncs: AtomicUsize,
        /// When set, the next sync parks on this gate until released.
        hold_next_sync: parking_lot::Mutex<Option<Arc<tokio::sync::Semaphore>>>,
        sync_parked: AtomicBool,
    }

    #[async_trait::async_trait]
    impl super::SshApplier for FakeApplier {
        async fn install_ca(&self, _ca: &str) -> Result<(), crate::ssh_ca::SshCaError> {
            let on = self
                .access
                .lock()
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .is_some_and(|a| a.is_enabled());
            // A real install is a `sudo` round trip: let anything racing it run.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            self.installs.lock().push(on);
            self.file.lock().clear();
            Ok(())
        }
        async fn sync_principals(
            &self,
            live: BTreeSet<String>,
        ) -> Result<(), crate::ssh_ca::SshCaError> {
            let gate = self.hold_next_sync.lock().take();
            if let Some(gate) = gate {
                self.sync_parked.store(true, Ordering::SeqCst);
                drop(gate.acquire().await);
            }
            *self.file.lock() = live;
            self.syncs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// A manager over `access` whose privileged ops go to a [`FakeApplier`],
    /// attached with an SSH CA as the tunnel attaches it.
    fn manager_with_fake(
        access: &Arc<RemoteAccess>,
    ) -> (Arc<RemoteShellManager>, Arc<FakeApplier>) {
        let fake = Arc::new(FakeApplier::default());
        *fake.access.lock() = Some(Arc::downgrade(access));
        let manager = Arc::new(
            RemoteShellManager::new(
                Arc::clone(access),
                nexus_cloud_client::tunnel::TunnelClient::new(
                    "wss://cloud.invalid/v1/tunnel",
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                ),
                None,
                Arc::new(nexus_cloud_client::TunnelOutbox::new()),
            )
            .with_applier(fake.clone()),
        );
        access.attach(&manager, Some("ssh-ed25519 AAAAfake nexus-ca".to_string()));
        (manager, fake)
    }

    /// Let every spawned task run to completion (or to a parked gate).
    async fn settle() {
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    /// An enable while already on is a no-op: the CA installer truncates
    /// the principal file, so re-running it would revoke a session that is
    /// waiting to authenticate.
    #[tokio::test]
    async fn remote_access_repeated_enable_does_not_reinstall_the_ca() {
        let access = Arc::new(RemoteAccess::new(
            nexus_config::RemoteAccessConfig::default(),
            false,
        ));
        let (_manager, fake) = manager_with_fake(&access);

        access.set_enabled(true).await;
        settle().await;
        assert_eq!(fake.installs.lock().len(), 1, "off→on adopts the CA");

        access.set_enabled(true).await;
        settle().await;
        assert_eq!(
            fake.installs.lock().len(),
            1,
            "on→on must not run the installer again"
        );
    }

    /// The off→on transition installs the CA while the switch is still
    /// off, so no session can be admitted underneath the truncate.
    #[tokio::test]
    async fn remote_access_enable_installs_the_ca_before_switching_on() {
        let access = Arc::new(RemoteAccess::new(
            nexus_config::RemoteAccessConfig::default(),
            false,
        ));
        let (_manager, fake) = manager_with_fake(&access);

        access.set_enabled(true).await;
        settle().await;
        assert_eq!(
            *fake.installs.lock(),
            vec![false],
            "the installer ran while remote access was still off"
        );
        assert!(access.is_enabled());
    }

    /// A session opened the moment the enable returns keeps its principal:
    /// the install has finished, so the session's sync is the last write.
    #[tokio::test]
    async fn remote_access_session_opened_right_after_enable_keeps_its_principal() {
        let access = Arc::new(RemoteAccess::new(
            nexus_config::RemoteAccessConfig::default(),
            false,
        ));
        let (manager, fake) = manager_with_fake(&access);

        access.set_enabled(true).await;
        // What `start` does once `admit` passes.
        let _cancel = manager.insert_test_session("s-1");
        manager.resync_principals();
        settle().await;

        assert_eq!(fake.installs.lock().len(), 1);
        assert_eq!(*fake.file.lock(), set(&["s-1"]));
    }

    /// Two syncs racing: the second waits for the first and snapshots the
    /// live set when it writes, so the last write is the current set even
    /// when the first write is slow.
    #[tokio::test]
    async fn remote_access_principal_syncs_write_the_set_current_at_write_time() {
        let access = Arc::new(RemoteAccess::new(
            nexus_config::RemoteAccessConfig::default(),
            false,
        ));
        let (manager, fake) = manager_with_fake(&access);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *fake.hold_next_sync.lock() = Some(Arc::clone(&gate));

        let _s1 = manager.insert_test_session("s-1");
        manager.resync_principals();
        for _ in 0..200 {
            if fake.sync_parked.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            fake.sync_parked.load(Ordering::SeqCst),
            "first sync is in flight"
        );

        let _s2 = manager.insert_test_session("s-2");
        manager.live.lock().remove("s-1");
        manager.resync_principals();
        settle().await;
        gate.add_permits(1);
        settle().await;

        assert_eq!(fake.syncs.load(Ordering::SeqCst), 2);
        assert_eq!(
            *fake.file.lock(),
            set(&["s-2"]),
            "last write is the live set"
        );
    }
    use chrono::{DateTime, Duration as ChronoDuration, Utc};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn the_authority_is_read_out_of_a_normal_url() {
        assert_eq!(
            side_channel_host("wss://gw.nexus.example/shell/abc?t=1"),
            "gw.nexus.example"
        );
        assert_eq!(
            side_channel_host("wss://gw.nexus.example"),
            "gw.nexus.example"
        );
        assert_eq!(
            side_channel_host("wss://gw.nexus.example:8443/shell"),
            "gw.nexus.example:8443"
        );
    }

    /// The comparison in `admit` is case-sensitive, so the parse has to
    /// normalise. Otherwise `WSS://GW.…` would read as a different host
    /// from the tunnel's and a legitimate session would be refused.
    #[test]
    fn case_is_normalised_so_a_real_host_is_not_refused_for_its_spelling() {
        assert_eq!(
            side_channel_host("WSS://GW.Nexus.Example/shell"),
            "gw.nexus.example"
        );
    }

    /// A forged envelope's most valuable move is to name its own host.
    /// These must all parse to something that will not equal the
    /// tunnel's authority.
    #[test]
    fn another_host_never_parses_as_ours() {
        let ours = "gw.nexus.example";
        for url in [
            "wss://evil.example/shell",
            "wss://gw.nexus.example.evil.example/shell",
            "wss://evil.example/wss://gw.nexus.example",
            "wss://evil.example#gw.nexus.example",
            "wss://evil.example?h=gw.nexus.example",
        ] {
            assert_ne!(side_channel_host(url), ours, "{url}");
        }
    }

    /// Userinfo is deliberately *not* stripped: `user@gw.nexus.example`
    /// is a different authority string from `gw.nexus.example`, so it
    /// fails the equality check and the session is refused. That is the
    /// safe direction — the alternative is a parser that helpfully
    /// discards the part of the URL an attacker controls.
    #[test]
    fn a_userinfo_prefix_is_refused_rather_than_stripped() {
        assert_ne!(
            side_channel_host("wss://evil.example@gw.nexus.example/shell"),
            "gw.nexus.example"
        );
    }

    /// Empty means "cannot tell", and `admit` refuses on empty.
    #[test]
    fn an_unreadable_url_yields_nothing_to_match_against() {
        for url in ["", "gw.nexus.example/shell", "://gw", "not a url"] {
            let host = side_channel_host(url);
            assert!(
                host.is_empty() || host != "gw.nexus.example",
                "{url} -> {host}"
            );
        }
        assert!(side_channel_host("").is_empty());
        assert!(side_channel_host("gw.nexus.example/shell").is_empty());
    }

    #[test]
    fn a_future_deadline_reports_what_is_left() {
        let left = remaining_until("2026-01-01T00:15:00Z", now())
            .expect("parses")
            .expect("still ahead");
        assert_eq!(left.as_secs(), 900);
    }

    /// A cloud clock ahead of ours must not extend the session past the
    /// local ceiling — the caller takes the minimum of the two, and this
    /// is the input that makes that clamp matter.
    #[test]
    fn a_far_future_deadline_is_still_just_a_number_for_the_caller_to_clamp() {
        let left = remaining_until("2030-01-01T00:00:00Z", now())
            .expect("parses")
            .expect("still ahead");
        assert!(left.as_secs() > 60 * 60 * 24 * 365);
    }

    /// Already expired means "end immediately" — never "no limit". The
    /// exactly-now boundary is a zero duration rather than `None`, which
    /// the caller turns into the same instant; both paths end the
    /// session on the next loop.
    #[test]
    fn a_past_or_present_deadline_leaves_nothing() {
        assert!(remaining_until("2025-12-31T23:59:00Z", now())
            .expect("parses")
            .is_none());
        assert_eq!(
            remaining_until("2026-01-01T00:00:00Z", now())
                .expect("parses")
                .expect("zero, not absent")
                .as_secs(),
            0
        );
    }

    /// The failure that matters: garbage must be distinguishable from
    /// "no time left", because the caller's fallbacks differ (local
    /// ceiling vs. end now) and the dangerous third option — treating an
    /// unparseable timestamp as unlimited — must be unreachable.
    #[test]
    fn an_unparseable_timestamp_is_an_error_not_an_absence() {
        for bad in ["", "soon", "2026-01-01", "1767225600"] {
            assert!(remaining_until(bad, now()).is_err(), "{bad}");
        }
    }

    /// Offsets other than `Z` are normal in RFC 3339 and must not be
    /// mistaken for garbage.
    #[test]
    fn a_non_utc_offset_is_understood() {
        let left = remaining_until("2026-01-01T01:15:00+01:00", now())
            .expect("parses")
            .expect("still ahead");
        assert_eq!(left.as_secs(), 900);
        // Sanity: the offset is actually applied, not ignored.
        let naive = remaining_until("2026-01-01T01:15:00Z", now())
            .expect("parses")
            .expect("still ahead");
        assert_eq!(naive - left, ChronoDuration::hours(1).to_std().unwrap());
    }
}
