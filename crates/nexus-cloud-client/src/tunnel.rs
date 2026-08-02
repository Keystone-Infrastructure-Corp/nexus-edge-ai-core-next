//! WSS tunnel client to `edge-gateway /v1/tunnel`.
//!
//! Phase 1.8 ships the body: an `async` connect + reader/writer pair
//! over WSS with mTLS, plus a tiny heartbeat loop. RPC dispatch
//! (state-mutating cloud → edge calls) lands in the next slice once
//! the engine has handlers to dispatch to.
//!
//! ## Trust posture
//!
//! * **Server identity** — verified against a *union* of (a) the
//!   internal CA chain returned by enrollment-svc (`ca_chain_pem`)
//!   and (b) Mozilla's public CA root store (`webpki-roots`). The
//!   internal CA path covers production deployments where the
//!   gateway terminates TLS itself with an internal-CA-issued leaf;
//!   the public-root path covers managed-ingress deployments where
//!   TLS terminates at e.g. Azure Container Apps' front door with a
//!   public-CA-issued leaf (Microsoft → DigiCert). Both paths are
//!   acceptable because client identity (mTLS) is what authenticates
//!   the core to the gateway; server identity here just confirms
//!   we're talking to a host the DNS owner authorised TLS for.
//! * **Client identity** — the leaf cert + private key written by the
//!   `enroll` subcommand are presented during the TLS handshake; the
//!   gateway pins `(org_id, site_id, core_id)` from the cert's URI
//!   SANs.
//! * **No fallback** — if neither root store validates, the connect
//!   fails closed. There is no `--insecure-skip-verify` knob anywhere
//!   in this crate; testing uses a locally-trusted CA instead.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::{SinkExt as _, StreamExt as _};
use nexus_cloud_protocol::v1::{Envelope, EnvelopeBody};
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::Connector;
use tracing::{debug, info, warn};

/// Handle the engine talks to. Phase 1.8 keeps the outbound surface
/// minimal: fire-and-forget `send`. Phase 2 (Step 2.1c) introduces
/// inbound dispatch, but the receiver is owned by [`Connection`]
/// directly \u2014 only the `send` half is shared via this trait so
/// arbitrary engine subsystems can hold an `Arc<dyn TunnelHandle>`
/// without competing for inbound frames.
#[async_trait]
pub trait TunnelHandle: Send + Sync {
    /// Send an outbound envelope (edge → cloud). Returns when the frame
    /// has been queued for the WSS writer task; not when the cloud has
    /// acknowledged it.
    async fn send(&self, envelope: Envelope) -> Result<(), TunnelError>;
}

/// Uplink priority tier for an outbound envelope. See
/// [`docs/edge-core/M_PERF_CROWD.md` Phase H][ph] — a single WSS writer
/// drains three separate channels in strict priority order so a
/// `entity_sighting` flood can never delay heartbeats (which would
/// surface as bogus `cores.last_skew_ms`) or `rpc_response` frames
/// (which would surface as "core offline / can't get settings").
///
/// [ph]: https://github.com/Keystone-Infrastructure-Corp/nexus-cloud-console/blob/main/docs/edge-core/M_PERF_CROWD.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Heartbeats, rpc responses, update progress, webrtc signaling,
    /// session/roster control. Never dropped; jumps the bulk queue.
    Control,
    /// Security events (`alert`) and clip replication receipts. Never
    /// dropped (the edge outbox reconciles on disconnect), but ranked
    /// below control so an alert storm cannot delay heartbeats.
    Alert,
    /// Best-effort telemetry: appearance sightings + live-view frames.
    /// Dropped on a full channel rather than blocking the writer.
    Bulk,
}

/// Classify an envelope into its uplink [`Tier`]. Only the known
/// high-volume kinds are `Bulk`; alerts + clip receipts are `Alert`;
/// **everything else defaults to `Control`** so a new low-volume kind
/// is never accidentally droppable.
pub(crate) fn tier_of(body: &EnvelopeBody) -> Tier {
    match body {
        EnvelopeBody::EntitySighting(_)
        | EnvelopeBody::EntitySightingBatch(_)
        | EnvelopeBody::LbrFrame(_) => Tier::Bulk,
        EnvelopeBody::Alert(_) | EnvelopeBody::ClipReplicated(_) => Tier::Alert,
        _ => Tier::Control,
    }
}

/// Milliseconds since the Unix epoch from the system clock. Used to
/// re-stamp a heartbeat's `edge_ts_unix_ms` at flush time (see
/// [`stamp_at_flush`]).
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Re-stamp an envelope's edge-side timestamps immediately before it is
/// written to the socket. For heartbeats this overwrites
/// `edge_ts_unix_ms` (and `meta.ts`) with the true flush instant so the
/// gateway's clock-skew EMA measures transport latency, not the time
/// the envelope spent queued behind bulk frames. Non-heartbeat kinds
/// are left untouched.
fn stamp_at_flush(env: &mut Envelope) {
    if let EnvelopeBody::Heartbeat(ref mut hb) = env.body {
        let now = now_unix_ms();
        hb.edge_ts_unix_ms = Some(now);
        env.meta.ts = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now as i64)
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc3339();
    }
}

/// Errors the tunnel client can surface.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TunnelError {
    /// Tunnel is currently disconnected (engine should retry after
    /// the reconnect backoff has elapsed).
    #[error("tunnel disconnected")]
    Disconnected,
    /// Failed to build the rustls client config (bad PEM, no chain
    /// entries, etc.). Wrap as a string because rustls' errors don't
    /// implement `Clone`.
    #[error("tls config: {0}")]
    TlsConfig(String),
    /// Failed to perform the WSS handshake.
    #[error("tunnel handshake: {0}")]
    Handshake(String),
    /// Outbound channel saturated or closed before the writer could
    /// flush the frame. The engine should drop the message; the next
    /// tunnel reconnect will send a fresh heartbeat.
    #[error("tunnel send channel closed")]
    SendChannelClosed,
}

/// Phase 1.8 tunnel client. Holds the resolved `wss://gateway/v1/tunnel`
/// URL + the mTLS identity. [`Self::connect`] performs the WSS+mTLS
/// handshake and returns a live [`Connection`].
#[derive(Debug, Clone)]
pub struct TunnelClient {
    gateway_url: String,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
    ca_chain_pem: Vec<u8>,
}

/// A live tunnel connection. Implements [`TunnelHandle`] for outbound
/// sends; spawns its own reader + writer task under the hood. Dropping
/// the [`Connection`] closes the underlying WebSocket via the oneshot
/// close signal.
///
/// Phase 2 Step 2.1c: the reader task forwards parsed inbound
/// [`Envelope`]s onto a bounded channel exposed via
/// [`Self::take_inbound`]. The first caller takes ownership of the
/// receiver; subsequent callers get `None`. If no one drains the
/// channel, the bounded capacity backpressures the reader \u2014 the
/// reader logs and drops any frame that can't be queued so the WSS
/// pump never stalls on slow handlers.
pub struct Connection {
    /// Tier 1 (control): heartbeats, rpc responses, signaling. Never
    /// dropped; drained before alerts and bulk. Cap 64.
    ctl_tx: mpsc::Sender<Envelope>,
    /// Tier 2 (alerts): security events + clip receipts. Never dropped;
    /// drained after control, before bulk. Cap 64.
    alert_tx: mpsc::Sender<Envelope>,
    /// Tier 3 (bulk): appearance sightings + live-view frames. Dropped
    /// on a full channel (`try_send`) so best-effort telemetry can
    /// never head-of-line-block the control/alert tiers. Cap 32.
    out_tx: mpsc::Sender<Envelope>,
    in_rx: Option<mpsc::Receiver<Envelope>>,
    _close_tx: tokio::sync::oneshot::Sender<()>,
    _join: tokio::task::JoinHandle<()>,
}

impl TunnelClient {
    /// Build a client targeting the resolved `wss://gateway/v1/tunnel`
    /// URL from the enrollment artifact, with mTLS identity attached.
    #[must_use]
    pub fn new(
        gateway_url: impl Into<String>,
        cert_pem: impl Into<Vec<u8>>,
        key_pem: impl Into<Vec<u8>>,
        ca_chain_pem: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            gateway_url: gateway_url.into(),
            cert_pem: cert_pem.into(),
            key_pem: key_pem.into(),
            ca_chain_pem: ca_chain_pem.into(),
        }
    }

    /// Configured gateway URL.
    #[must_use]
    pub fn gateway_url(&self) -> &str {
        &self.gateway_url
    }

    /// Host (and port, if explicit) of the configured gateway URL.
    ///
    /// The remote-shell side channel is only allowed to dial this exact
    /// authority — see [`Self::connect_side_channel`].
    #[must_use]
    pub fn gateway_authority(&self) -> Option<String> {
        authority_of(&self.gateway_url)
    }

    /// Open a SECOND, non-enveloped WSS connection for a remote-shell
    /// byte pipe, reusing the same mTLS identity as the control tunnel.
    ///
    /// The side channel deliberately does not ride the control tunnel:
    /// that socket has a single writer draining three priority queues,
    /// and an interactive shell's byte stream would either starve the
    /// heartbeat or be starved by it. A separate socket keeps both
    /// well-behaved.
    ///
    /// `url` MUST resolve to the same host:port as the control tunnel.
    /// A cloud that could name any authority here would turn every core
    /// into an outbound pivot, and the operator's firewall exception was
    /// only ever granted for one destination.
    ///
    /// # Errors
    ///
    /// * [`TunnelError::Handshake`] — `url` names a different authority
    ///   than the control tunnel, or the WSS handshake failed.
    /// * [`TunnelError::TlsConfig`] — the mTLS identity failed to load.
    pub async fn connect_side_channel(&self, url: &str) -> Result<SideChannel, TunnelError> {
        let expected = self
            .gateway_authority()
            .ok_or_else(|| TunnelError::Handshake("gateway url has no host".into()))?;
        let actual = authority_of(url)
            .ok_or_else(|| TunnelError::Handshake("side channel url has no host".into()))?;
        if actual != expected {
            return Err(TunnelError::Handshake(format!(
                "side channel authority `{actual}` is not the control tunnel's `{expected}`"
            )));
        }

        let tls_config = build_client_config(&self.cert_pem, &self.key_pem, &self.ca_chain_pem)
            .map_err(TunnelError::TlsConfig)?;
        let connector = Connector::Rustls(Arc::new(tls_config));

        let (ws_stream, _resp) =
            tokio_tungstenite::connect_async_tls_with_config(url, None, false, Some(connector))
                .await
                .map_err(|e| TunnelError::Handshake(e.to_string()))?;

        Ok(ws_stream)
    }

    /// Open the WSS+mTLS connection and spawn the reader/writer pair.
    ///
    /// # Errors
    ///
    /// * [`TunnelError::TlsConfig`] — PEM parse / rustls builder failed.
    /// * [`TunnelError::Handshake`] — WSS handshake failed (DNS, TCP,
    ///   TLS, or HTTP upgrade).
    pub async fn connect(&self) -> Result<Connection, TunnelError> {
        let tls_config = build_client_config(&self.cert_pem, &self.key_pem, &self.ca_chain_pem)
            .map_err(TunnelError::TlsConfig)?;
        let connector = Connector::Rustls(Arc::new(tls_config));

        let (ws_stream, _resp) = tokio_tungstenite::connect_async_tls_with_config(
            &self.gateway_url,
            None,
            false,
            Some(connector),
        )
        .await
        .map_err(|e| TunnelError::Handshake(e.to_string()))?;

        info!(url = %self.gateway_url, "cloud tunnel connected");

        let (mut writer, mut reader) = ws_stream.split();
        // Phase H — uplink priority channels. Three bounded queues drained
        // by one writer in strict priority order (control → alert → bulk)
        // so an `entity_sighting` flood cannot delay heartbeats or rpc
        // responses. See docs/edge-core/M_PERF_CROWD.md.
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<Envelope>(64);
        let (alert_tx, mut alert_rx) = mpsc::channel::<Envelope>(64);
        let (out_tx, mut out_rx) = mpsc::channel::<Envelope>(32);
        let (in_tx, in_rx) = mpsc::channel::<Envelope>(32);
        let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();

        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut close_rx => {
                        debug!("tunnel close signal received; sending Close frame");
                        let _ = writer.send(Message::Close(None)).await;
                        break;
                    }
                    // Tier 1: control. Highest write priority. Heartbeats are
                    // re-stamped with the true flush instant here so the
                    // gateway's skew EMA measures transport latency, not queue
                    // dwell time.
                    maybe = ctl_rx.recv() => {
                        let Some(mut env) = maybe else { break };
                        stamp_at_flush(&mut env);
                        match serde_json::to_string(&env) {
                            Ok(text) => {
                                if let Err(e) = writer.send(Message::Text(text)).await {
                                    warn!(error = %e, "tunnel control write failed; closing");
                                    break;
                                }
                            }
                            Err(e) => warn!(error = %e, "tunnel control envelope serialise failed; dropping"),
                        }
                    }
                    // Tier 2: alerts. Drained after control, before bulk.
                    maybe = alert_rx.recv() => {
                        let Some(env) = maybe else { break };
                        match serde_json::to_string(&env) {
                            Ok(text) => {
                                if let Err(e) = writer.send(Message::Text(text)).await {
                                    warn!(error = %e, "tunnel alert write failed; closing");
                                    break;
                                }
                            }
                            Err(e) => warn!(error = %e, "tunnel alert envelope serialise failed; dropping"),
                        }
                    }
                    // Inbound frames (acks, pings, rpc calls) rank above bulk
                    // so the cloud's control traffic is never starved by an
                    // outbound sighting flood.
                    incoming = reader.next() => {
                        match incoming {
                            Some(Ok(Message::Text(text))) => {
                                match serde_json::from_str::<Envelope>(&text) {
                                    Ok(env) => {
                                        debug!(
                                            kind = ?std::mem::discriminant(&env.body),
                                            "tunnel inbound envelope",
                                        );
                                        // Backpressure: if the engine
                                        // hasn't taken the inbound
                                        // receiver, or is dispatching
                                        // slower than frames arrive,
                                        // drop with a warn rather
                                        // than stall the reader.
                                        if let Err(e) = in_tx.try_send(env) {
                                            warn!(
                                                error = %e,
                                                "tunnel inbound queue full or dropped; envelope discarded",
                                            );
                                        }
                                    }
                                    Err(e) => warn!(error = %e, "tunnel inbound parse failed"),
                                }
                            }
                            Some(Ok(Message::Ping(p))) => {
                                let _ = writer.send(Message::Pong(p)).await;
                            }
                            Some(Ok(Message::Close(_))) => {
                                info!("tunnel closed by remote");
                                break;
                            }
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                warn!(error = %e, "tunnel read error; closing");
                                break;
                            }
                            None => {
                                info!("tunnel stream ended");
                                break;
                            }
                        }
                    }
                    // Tier 3: bulk. Lowest write priority; only serviced when
                    // no control frame, alert frame, or inbound frame is
                    // ready. A large sighting batch here blocks the writer for
                    // at most one frame's flush, never the whole backlog.
                    maybe = out_rx.recv() => {
                        let Some(env) = maybe else { break };
                        match serde_json::to_string(&env) {
                            Ok(text) => {
                                if let Err(e) = writer.send(Message::Text(text)).await {
                                    warn!(error = %e, "tunnel bulk write failed; closing");
                                    break;
                                }
                            }
                            Err(e) => warn!(error = %e, "tunnel bulk envelope serialise failed; dropping"),
                        }
                    }
                }
            }
            debug!("tunnel pump exiting");
        });

        Ok(Connection {
            ctl_tx,
            alert_tx,
            out_tx,
            in_rx: Some(in_rx),
            _close_tx: close_tx,
            _join: join,
        })
    }
}

impl Connection {
    /// Take ownership of the inbound envelope receiver. Returns
    /// `Some` exactly once per connection; subsequent calls return
    /// `None`. Engine dispatcher loops call this once at
    /// connect-time and select on it alongside the heartbeat pump.
    #[must_use]
    pub fn take_inbound(&mut self) -> Option<mpsc::Receiver<Envelope>> {
        self.in_rx.take()
    }
}

#[async_trait]
impl TunnelHandle for Connection {
    async fn send(&self, envelope: Envelope) -> Result<(), TunnelError> {
        // Route by tier. Control and alert frames are never dropped: they
        // await queue capacity (each queue is 64 deep). Bulk frames are
        // best-effort — a full bulk queue drops the frame rather than
        // blocking the caller (and, transitively, the writer).
        match tier_of(&envelope.body) {
            Tier::Control => self
                .ctl_tx
                .send(envelope)
                .await
                .map_err(|_| TunnelError::SendChannelClosed),
            Tier::Alert => self
                .alert_tx
                .send(envelope)
                .await
                .map_err(|_| TunnelError::SendChannelClosed),
            Tier::Bulk => match self.out_tx.try_send(envelope) {
                Ok(()) => Ok(()),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("tunnel bulk queue full; dropping best-effort envelope");
                    Ok(())
                }
                Err(mpsc::error::TrySendError::Closed(_)) => Err(TunnelError::SendChannelClosed),
            },
        }
    }
}

/// Blanket impl so engine code that holds an `Arc<Connection>` can
/// hand it to anything that wants `Arc<dyn TunnelHandle>` (or to a
/// generic bound `T: TunnelHandle`) without an extra adapter type.
///
/// Phase 2 \u00b7 Step 2.8 \u2014 [`crate::TunnelOutbox::set_handle`] stores
/// an `Arc<Connection>` cloned per-reconnect; the outbox publishes
/// through that handle via this impl.
#[async_trait]
impl<T: TunnelHandle + ?Sized> TunnelHandle for Arc<T> {
    async fn send(&self, envelope: Envelope) -> Result<(), TunnelError> {
        (**self).send(envelope).await
    }
}

/// A live remote-shell side channel: a raw WSS stream carrying binary
/// frames in both directions with no envelope wrapper.
pub type SideChannel =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Extract `host[:port]` from a `ws(s)://` URL without pulling in a URL
/// parser. Returns `None` if there is no authority component.
fn authority_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|a| !a.is_empty())?;
    // Strip any userinfo — it is not part of the destination identity.
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    Some(host.to_ascii_lowercase())
}

/// Build a [`ClientConfig`] with mTLS identity + a root store seeded
/// from a union of `ca_chain_pem` (the internal CA we trust the gateway
/// against when it terminates TLS itself) and Mozilla's public CA
/// roots (`webpki-roots`, for the managed-ingress case where the
/// gateway sits behind Azure Container Apps and TLS terminates at
/// Microsoft's edge with a DigiCert-issued leaf). See the crate-level
/// trust posture docs for the rationale.
fn build_client_config(
    cert_pem: &[u8],
    key_pem: &[u8],
    ca_chain_pem: &[u8],
) -> Result<ClientConfig, String> {
    // Install the ring crypto provider on first use. This is a no-op if
    // some other crate already installed it — rustls 0.23 supports both
    // ring and aws-lc-rs and refuses to default automatically.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

    let ca_certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut std::io::Cursor::new(ca_chain_pem))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("parse ca_chain_pem: {e}"))?;
    if ca_certs.is_empty() {
        return Err("ca_chain_pem contained no certificates".into());
    }
    let mut roots = RootCertStore::empty();
    for c in ca_certs {
        roots.add(c).map_err(|e| format!("trust ca cert: {e}"))?;
    }
    // Augment with Mozilla's public CA roots. `webpki_roots::TLS_SERVER_ROOTS`
    // is a static slice of `TrustAnchor`s; extending a `RootCertStore` with
    // them is the rustls-recommended pattern. `extend` returns nothing — it
    // can't fail since the anchors are pre-validated at the crate level.
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let leaf_chain: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut std::io::Cursor::new(cert_pem))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("parse cert_pem: {e}"))?;
    if leaf_chain.is_empty() {
        return Err("cert_pem contained no certificates".into());
    }

    let private_key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut std::io::Cursor::new(key_pem))
            .map_err(|e| format!("parse key_pem: {e}"))?
            .ok_or_else(|| "key_pem contained no private key".to_string())?;

    ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(leaf_chain, private_key)
        .map_err(|e| format!("build client config: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_client_config_rejects_empty_ca_chain() {
        let err = build_client_config(b"", b"", b"").expect_err("empty inputs must fail");
        assert!(err.contains("ca_chain_pem"));
    }

    #[test]
    fn build_client_config_rejects_missing_key() {
        let cert_pem = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let ca_pem = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        assert!(build_client_config(cert_pem, b"", ca_pem).is_err());
    }

    #[test]
    fn authority_ignores_path_and_case() {
        assert_eq!(
            authority_of("wss://Gateway.Example:443/v1/tunnel").as_deref(),
            Some("gateway.example:443"),
        );
        assert_eq!(
            authority_of("wss://gateway.example/v1/shell/abc").as_deref(),
            Some("gateway.example"),
        );
        assert_eq!(authority_of("not-a-url"), None);
    }

    #[tokio::test]
    async fn side_channel_refuses_a_foreign_host() {
        let client = TunnelClient::new("wss://gateway.example/v1/tunnel", b"", b"", b"");
        let err = client
            .connect_side_channel("wss://attacker.example/v1/shell/abc")
            .await
            .expect_err("a different authority must be refused");
        assert!(matches!(err, TunnelError::Handshake(_)), "{err:?}");
    }

    #[tokio::test]
    async fn side_channel_refuses_a_different_port_on_the_same_host() {
        let client = TunnelClient::new("wss://gateway.example/v1/tunnel", b"", b"", b"");
        assert!(client
            .connect_side_channel("wss://gateway.example:8443/v1/shell/abc")
            .await
            .is_err());
    }
}
