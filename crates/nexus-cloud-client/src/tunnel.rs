//! WSS tunnel client to `edge-gateway /v1/tunnel`.
//!
//! Phase 1.8 ships the body: an `async` connect + reader/writer pair
//! over WSS with mTLS, plus a tiny heartbeat loop. RPC dispatch
//! (state-mutating cloud → edge calls) lands in the next slice once
//! the engine has handlers to dispatch to.
//!
//! ## Trust posture
//!
//! * **Server identity** — verified against the CA chain returned by
//!   enrollment-svc (`ca_chain_pem`). The gateway's leaf cert is signed
//!   by the same internal CA that issued the edge's client cert; we do
//!   NOT use webpki roots for the gateway.
//! * **Client identity** — the leaf cert + private key written by the
//!   `enroll` subcommand are presented during the TLS handshake; the
//!   gateway pins `(org_id, site_id, core_id)` from the cert's URI
//!   SANs.
//! * **No fallback** — if the CA chain doesn't validate, the connect
//!   fails closed. There is no `--insecure-skip-verify` knob anywhere
//!   in this crate; testing uses a locally-trusted CA instead.

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{SinkExt as _, StreamExt as _};
use nexus_cloud_protocol::v1::Envelope;
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
                    maybe = out_rx.recv() => {
                        let Some(env) = maybe else { break };
                        match serde_json::to_string(&env) {
                            Ok(text) => {
                                if let Err(e) = writer.send(Message::Text(text)).await {
                                    warn!(error = %e, "tunnel write failed; closing");
                                    break;
                                }
                            }
                            Err(e) => warn!(error = %e, "tunnel envelope serialise failed; dropping"),
                        }
                    }
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
                }
            }
            debug!("tunnel pump exiting");
        });

        Ok(Connection {
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
        self.out_tx
            .send(envelope)
            .await
            .map_err(|_| TunnelError::SendChannelClosed)
    }
}

/// Build a [`ClientConfig`] with mTLS identity + a root store seeded
/// from `ca_chain_pem` (the internal CA we trust the gateway against).
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
}
