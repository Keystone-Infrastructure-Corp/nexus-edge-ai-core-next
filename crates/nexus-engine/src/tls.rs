//! In-process TLS support for the engine UI on `:443`
//! (M-HTTPS Phase 1).
//!
//! Three responsibilities:
//!
//! 1. **Self-signed bootstrap** — `init_self_signed_cert` mints a
//!    rcgen-generated leaf valid for the host's external identities
//!    (hostname, `<hostname>.local`, every non-loopback IPv4/IPv6
//!    plus `localhost`/`127.0.0.1`). Idempotent: if a usable cert
//!    already exists at the target path the call is a no-op unless
//!    `force == true`. Lets the installer call this unconditionally
//!    on first boot without trampling an operator-provided cert or
//!    the cloud-issued leaf that Phase 3 will drop in.
//!
//! 2. **Runtime load + hot-reload** — `load_rustls_config` parses
//!    the PEM pair into the typed shape `axum-server` wants;
//!    `spawn_cert_watcher` polls mtime once a minute and calls
//!    `RustlsConfig::reload_from_pem` when the bytes change. We
//!    poll (rather than inotify) to avoid a `notify` dep; cert
//!    rotation is rare (cloud cycles every 90d in Phase 3) so the
//!    one-minute latency is fine.
//!
//! 3. **`http://` → `https://` redirect** — `redirect_router`
//!    returns a tiny `axum::Router` that 308-redirects every path
//!    to the same path on `https://<host>:<port>`. Mounted on the
//!    `ui_bind` listener when `redirect_http_to_https = true`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use axum::extract::OriginalUri;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect};
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use tracing::{info, warn};

/// v0.1.36 — DNS + IP SAN list the engine's local HTTPS listener
/// should be reachable as. Shared between [`init_self_signed_cert`]
/// (the bootstrap path) and `cloud_enroll::build_server_csr` (which
/// asks the cloud to sign a leaf with these same names). Keeping
/// both call sites on one helper means a fleet upgrade can't end
/// up with a self-signed leaf advertising different names than
/// the cloud-issued replacement.
///
/// Returns `(dns_sans, ip_sans)`. The DNS list always contains
/// `nexus.local` and `localhost`; the local hostname (+ its
/// `.local` mDNS form for un-qualified names) is appended when
/// available. The IP list always contains `127.0.0.1` and `::1`;
/// every non-loopback, non-link-local interface IP is appended.
#[must_use]
pub fn derive_local_sans() -> (Vec<String>, Vec<IpAddr>) {
    let mut sans: Vec<String> = Vec::new();
    if let Ok(host) = hostname::get() {
        let host = host.to_string_lossy().to_string();
        if !host.is_empty() {
            sans.push(host.clone());
            if !host.contains('.') {
                sans.push(format!("{host}.local"));
            }
        }
    }
    sans.push("nexus.local".to_string());
    sans.push("localhost".to_string());

    let mut ip_sans: Vec<IpAddr> = vec![
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ];
    if let Ok(addrs) = if_addrs::get_if_addrs() {
        for ifaddr in addrs {
            let ip = ifaddr.ip();
            if ip.is_loopback() {
                continue;
            }
            match ip {
                IpAddr::V4(v4) if !v4.is_link_local() => ip_sans.push(IpAddr::V4(v4)),
                IpAddr::V6(v6) if !is_v6_link_local(&v6) => ip_sans.push(IpAddr::V6(v6)),
                _ => {}
            }
        }
    }

    sans.sort();
    sans.dedup();
    ip_sans.sort();
    ip_sans.dedup();
    (sans, ip_sans)
}

/// Generate a self-signed leaf at `cert_path`/`key_path` if one is
/// not already present. Returns `Ok(true)` when a new cert was
/// written, `Ok(false)` when an existing cert was kept.
///
/// SAN list includes:
///   * the system hostname (`gethostname`)
///   * `<hostname>.local` (the mDNS form most LAN browsers reach)
///   * `nexus.local` (the project's well-known mDNS alias)
///   * `localhost`
///   * every non-loopback IPv4 address bound on any local interface
///   * every globally-scoped IPv6 address bound on any local interface
///   * `127.0.0.1` and `::1` (so loopback HTTPS works for tests)
///
/// `force = true` regenerates even when a cert is already present
/// (used by the `tls init --force` operator escape hatch).
pub fn init_self_signed_cert(cert_path: &Path, key_path: &Path, force: bool) -> Result<bool> {
    if !force && cert_path.exists() && key_path.exists() {
        return Ok(false);
    }

    let (mut sans, ip_sans) = derive_local_sans();
    for ip in &ip_sans {
        sans.push(ip.to_string());
    }
    sans.sort();
    sans.dedup();

    let mut params =
        rcgen::CertificateParams::new(sans.clone()).context("rcgen rejected SAN list")?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "nexus-engine");
    params.distinguished_name.push(
        rcgen::DnType::OrganizationName,
        "Nexus Edge AI (self-signed)",
    );
    // Rely on rcgen's default validity (not_before = now-1d,
    // not_after = now+4y). Plenty long for the bootstrap leaf;
    // the Phase 3 cloud-issued cert takes over well before
    // expiry and can rotate independently.

    let key_pair = rcgen::KeyPair::generate().context("rcgen key generation failed")?;
    let cert = params
        .self_signed(&key_pair)
        .context("rcgen self-signed serialisation failed")?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    write_pem(cert_path, &cert_pem, 0o644)?;
    write_pem(key_path, &key_pem, 0o640)?;

    info!(
        cert = %cert_path.display(),
        key = %key_path.display(),
        sans = ?sans,
        "generated self-signed TLS leaf for engine UI",
    );
    Ok(true)
}

fn is_v6_link_local(addr: &Ipv6Addr) -> bool {
    let seg = addr.segments();
    (seg[0] & 0xffc0) == 0xfe80
}

fn write_pem(path: &Path, contents: &str, mode: u32) -> Result<()> {
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new_in(
        path.parent()
            .ok_or_else(|| anyhow!("cert path has no parent: {}", path.display()))?,
    )
    .with_context(|| format!("tempfile in {}", path.parent().unwrap().display()))?;
    tmp.write_all(contents.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    tmp.as_file().sync_all()?;
    let persisted = tmp
        .persist(path)
        .with_context(|| format!("persist {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(mode);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    drop(persisted);
    let _ = mode; // silence non-unix unused-var
    Ok(())
}

/// Install the rustls `ring` `CryptoProvider` as the process-wide
/// default. Safe to call more than once (subsequent calls are
/// no-ops by rustls's own dedup). Required because we use
/// axum-server's `tls-rustls-no-provider` feature — the cloud
/// tunnel already pulls in rustls but never installs a provider,
/// so it falls to us.
pub fn install_default_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Parse a PEM cert+key pair into an `axum-server` `RustlsConfig`.
pub async fn load_rustls_config(cert_path: &Path, key_path: &Path) -> Result<RustlsConfig> {
    let cert_pem = tokio::fs::read(cert_path)
        .await
        .with_context(|| format!("read {}", cert_path.display()))?;
    let key_pem = tokio::fs::read(key_path)
        .await
        .with_context(|| format!("read {}", key_path.display()))?;
    RustlsConfig::from_pem(cert_pem, key_pem)
        .await
        .with_context(|| {
            format!(
                "build RustlsConfig from {} / {}",
                cert_path.display(),
                key_path.display()
            )
        })
}

/// v0.1.36 \u2014 build `RustlsConfig` directly from in-memory PEM
/// bytes (cloud-issued path). The cloud-issued leaf is stored in
/// `cloud_enrollment.server_cert_pem` / `.server_private_key_pem`
/// (M-HTTPS Phase 3) and rotates only on re-enrollment, so no
/// disk-watching mechanism is needed.
pub async fn load_rustls_config_from_pems(cert_pem: &[u8], key_pem: &[u8]) -> Result<RustlsConfig> {
    RustlsConfig::from_pem(cert_pem.to_vec(), key_pem.to_vec())
        .await
        .context("build RustlsConfig from in-memory cloud-issued PEM")
}

/// v0.1.36 \u2014 origin of the TLS leaf currently bound to the
/// HTTPS listener. Drives the HSTS gate: only a cloud-issued
/// leaf earns HSTS headers, because shipping HSTS on top of a
/// browser-untrusted self-signed leaf would trap the operator's
/// browser for `max-age` seconds even after they accepted a
/// proper cert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertSource {
    /// Cert was minted in-process by [`init_self_signed_cert`].
    /// HSTS MUST NOT be advertised.
    SelfSigned,
    /// Cert was minted by the cloud's enrollment-svc CA (M-HTTPS
    /// Phase 3, v0.1.36). The leaf chains to a CA root the
    /// operator's browser can be configured to trust; HSTS is
    /// safe to advertise.
    CloudIssued,
}

/// Spawn a background task that re-reads the cert+key PEM whenever
/// either file's mtime advances. Cancels on `shutdown.cancelled()`.
pub fn spawn_cert_watcher(
    config: RustlsConfig,
    cert_path: PathBuf,
    key_path: PathBuf,
    shutdown: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = (mtime(&cert_path), mtime(&key_path));
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let cur = (mtime(&cert_path), mtime(&key_path));
            if cur != last {
                match config
                    .reload_from_pem_file(cert_path.clone(), key_path.clone())
                    .await
                {
                    Ok(()) => {
                        info!(
                            cert = %cert_path.display(),
                            "reloaded TLS cert from disk",
                        );
                        last = cur;
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            cert = %cert_path.display(),
                            "TLS cert reload failed; keeping previous cert in memory",
                        );
                    }
                }
            }
        }
    })
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Build a tiny router whose only job is to 308-redirect every
/// incoming request to the same path on `https://<host>:<https_port>`.
/// Method + body are preserved by the 308 status code.
///
/// `https_port` is omitted from the redirect URL when it is `443`
/// (the standard port), to keep operator-facing URLs clean.
pub fn redirect_router(https_port: u16) -> Router {
    Router::new().fallback(
        move |headers: HeaderMap, OriginalUri(uri): OriginalUri| async move {
            let host = headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            match redirect_target(host, &uri, https_port) {
                Ok(target) => Redirect::permanent(&target).into_response(),
                Err(err) => {
                    warn!(error = %err, host = %host, "rejecting redirect: bad Host header");
                    (StatusCode::BAD_REQUEST, "bad Host header").into_response()
                }
            }
        },
    )
}

/// Pure function so `redirect_router`'s URL construction is testable
/// without spinning a server.
pub(crate) fn redirect_target(host_header: &str, uri: &Uri, https_port: u16) -> Result<String> {
    // Strip any port suffix off the Host header; an HTTP listener
    // bound on `:80` may receive `Host: example.com:80` or just
    // `example.com`, and either way we want to substitute our own
    // HTTPS port.
    let host_only = match host_header.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host_header,
    };
    if host_only.is_empty() {
        bail!("empty host");
    }
    // Reject suspicious bytes that would let a client smuggle a
    // scheme/path into the Location header.
    if host_only
        .bytes()
        .any(|b| b == b'/' || b == b'\\' || b == b'\r' || b == b'\n' || b == b' ')
    {
        bail!("host contains illegal characters");
    }
    let path_and_query = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let port_suffix = if https_port == 443 {
        String::new()
    } else {
        format!(":{https_port}")
    };
    Ok(format!("https://{host_only}{port_suffix}{path_and_query}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_default_port_strips_suffix() {
        let uri: Uri = "/dashboard?x=1".parse().unwrap();
        let t = redirect_target("nexus.local", &uri, 443).unwrap();
        assert_eq!(t, "https://nexus.local/dashboard?x=1");
    }

    #[test]
    fn redirect_strips_explicit_http_port() {
        let uri: Uri = "/".parse().unwrap();
        let t = redirect_target("nexus.local:80", &uri, 443).unwrap();
        assert_eq!(t, "https://nexus.local/");
    }

    #[test]
    fn redirect_custom_port_kept() {
        let uri: Uri = "/api/v1/health".parse().unwrap();
        let t = redirect_target("10.0.0.5", &uri, 8443).unwrap();
        assert_eq!(t, "https://10.0.0.5:8443/api/v1/health");
    }

    #[test]
    fn redirect_rejects_path_smuggle() {
        let uri: Uri = "/".parse().unwrap();
        assert!(redirect_target("evil.example/attacker.com", &uri, 443).is_err());
        assert!(redirect_target("evil\r\nLocation: http://x", &uri, 443).is_err());
    }

    #[test]
    fn redirect_rejects_empty_host() {
        let uri: Uri = "/".parse().unwrap();
        assert!(redirect_target("", &uri, 443).is_err());
        assert!(redirect_target(":443", &uri, 443).is_err());
    }

    #[test]
    fn self_signed_idempotent_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        assert!(init_self_signed_cert(&cert, &key, false).unwrap());
        let cert_bytes_1 = std::fs::read(&cert).unwrap();
        assert!(!init_self_signed_cert(&cert, &key, false).unwrap());
        let cert_bytes_2 = std::fs::read(&cert).unwrap();
        assert_eq!(cert_bytes_1, cert_bytes_2);
        assert!(init_self_signed_cert(&cert, &key, true).unwrap());
        let cert_bytes_3 = std::fs::read(&cert).unwrap();
        assert_ne!(cert_bytes_1, cert_bytes_3);
    }
}
