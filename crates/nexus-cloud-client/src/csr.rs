//! CSR generation for the `enroll` flow.
//!
//! The edge engine calls [`generate_keypair_and_csr`] once per
//! enrollment: it mints a fresh ECDSA P-256 keypair, builds a PKCS#10
//! Certificate Signing Request whose Common Name is the operator-
//! supplied label, and returns the PEM-encoded CSR + the matching
//! private key (also PEM, PKCS#8). The engine then:
//!
//! 1. POSTs `{code, csr_pem, fingerprint}` to enrollment-svc.
//! 2. Persists `cert_pem` (signed leaf) + `private_key_pem` (kept
//!    locally) + `ca_chain_pem` to its local Store under the
//!    `cloud_enrollment` row.
//! 3. Uses the leaf cert + private key as the mTLS client identity
//!    when dialling [`crate::tunnel::TunnelClient`].
//!
//! Subject derivation: enrollment-svc DISCARDS the requester's SANs
//! and writes its own based on the resolved (org_id, site_id,
//! core_id) tuple. The Common Name here is informational only.
//!
//! Algorithm: **ECDSA P-256** (`rcgen::PKCS_ECDSA_P256_SHA256`).
//! Previously Ed25519, but Azure Container Apps' Envoy front door
//! advertises a `signature_algorithms` extension in its TLS 1.3
//! `CertificateRequest` that does NOT include `ed25519` (0x0807).
//! Per RFC 8446 §4.4.2.2 rustls then refuses to send the cert and
//! the handshake dies with `CertificateRequired`. ECDSA P-256
//! (`ecdsa_secp256r1_sha256` = 0x0403) is in every Envoy build's
//! default list. The CA itself stays Ed25519 (cert signature algs
//! can mix freely with the cert's public-key alg); only the leaf
//! public key + the matching client private key change.
//!
//! The unrelated Ed25519 keys elsewhere in the edge crypto posture
//! (actor_token verifier, entitlement JWT verifier, the optional
//! per-core signing key from enrollment) are untouched — those
//! never participate in a TLS handshake.

use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair,
    KeyUsagePurpose, SanType, PKCS_ECDSA_P256_SHA256,
};

/// Successful CSR build — PEMs the engine persists + POSTs.
#[derive(Debug, Clone)]
pub struct CsrBundle {
    /// PEM-encoded PKCS#10 CSR. Sent to enrollment-svc verbatim.
    pub csr_pem: String,
    /// PEM-encoded PKCS#8 private key. Stays on the box; pairs with
    /// the signed leaf cert as the mTLS client identity.
    pub private_key_pem: String,
}

/// Errors from CSR generation. All wrap `rcgen::Error` because every
/// failure mode here is an `rcgen` operation.
#[derive(Debug, thiserror::Error)]
pub enum CsrError {
    /// Underlying CSR construction failed. The wrapped variant carries
    /// the descriptive message from `rcgen` (key-generation failure,
    /// serialisation failure, etc.).
    #[error("CSR generation: {0}")]
    Rcgen(#[from] rcgen::Error),
    /// v0.1.36 — a DNS SAN supplied by the caller failed IA5String
    /// validation (non-ASCII, control bytes, or > 253 chars). Carries
    /// the offending string verbatim so the caller can log + skip.
    #[error("CSR DNS SAN invalid (IA5 violation): {0:?}")]
    InvalidDnsSan(String),
}

/// Generate a fresh ECDSA P-256 keypair + matching PKCS#10 CSR with
/// `common_name` set in the subject DN.
///
/// `common_name` is purely informational — enrollment-svc rewrites the
/// SANs from the resolved (org_id, site_id, core_id) tuple. A sane
/// default at call sites is the hostname or the operator-supplied
/// core label.
///
/// # Errors
///
/// Returns [`CsrError::Rcgen`] when the P-256 keypair cannot be
/// generated (rare — only happens if the system RNG is unavailable)
/// or when CSR serialisation fails.
pub fn generate_keypair_and_csr(common_name: &str) -> Result<CsrBundle, CsrError> {
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name.to_string());
    params.distinguished_name = dn;
    let csr = params.serialize_request(&key_pair)?;
    Ok(CsrBundle {
        csr_pem: csr.pem()?,
        private_key_pem: key_pair.serialize_pem(),
    })
}

/// v0.1.36 (M-HTTPS Phase 3) — generate a fresh ECDSA P-256 keypair +
/// CSR earmarked for the engine's local HTTPS *server* leaf.
/// Companion to [`generate_keypair_and_csr`] (which mints the mTLS
/// *client* leaf for the WSS tunnel).
///
/// Different from the client CSR in three ways:
///
/// 1. CSR carries `subjectAltName` extensions for every DNS name and
///    IP literal the browser might hit (`hostname`, `hostname.local`,
///    `nexus.local`, `localhost`, `127.0.0.1`, `::1`, plus every
///    non-link-local interface IP). The cloud re-asserts them
///    cloud-side (after sanitisation) before signing.
/// 2. CSR's `key_usages` advertise `DigitalSignature` +
///    `KeyEncipherment` (RSA forward-compat).
/// 3. CSR's `extended_key_usages` advertise `ServerAuth` only.
///
/// The cloud's signing path enforces all three regardless of what
/// the CSR claims — see
/// `services/enrollment-svc/src/ca.rs::sign_csr_server` — so these
/// are advisory, not authoritative. We still set them so the CSR
/// is self-describing and works with non-cloud signers in dev.
///
/// # Errors
///
/// Returns [`CsrError::Rcgen`] when keypair generation or CSR
/// serialisation fails.
pub fn generate_server_keypair_and_csr(
    common_name: &str,
    dns_sans: &[String],
    ip_sans: &[std::net::IpAddr],
) -> Result<CsrBundle, CsrError> {
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name.to_string());
    params.distinguished_name = dn;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let mut sans: Vec<SanType> = Vec::with_capacity(dns_sans.len() + ip_sans.len());
    for d in dns_sans {
        let ia5 = rcgen::Ia5String::try_from(d.as_str())
            .map_err(|_| CsrError::InvalidDnsSan(d.clone()))?;
        sans.push(SanType::DnsName(ia5));
    }
    for ip in ip_sans {
        sans.push(SanType::IpAddress(*ip));
    }
    params.subject_alt_names = sans;

    let csr = params.serialize_request(&key_pair)?;
    Ok(CsrBundle {
        csr_pem: csr.pem()?,
        private_key_pem: key_pair.serialize_pem(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_csr_has_expected_pem_headers() {
        let bundle = generate_keypair_and_csr("test-core").expect("csr generation should succeed");
        assert!(
            bundle
                .csr_pem
                .starts_with("-----BEGIN CERTIFICATE REQUEST-----"),
            "CSR PEM should start with the standard header, got: {:?}",
            &bundle.csr_pem[..bundle.csr_pem.len().min(60)]
        );
        assert!(
            bundle.csr_pem.contains("-----END CERTIFICATE REQUEST-----"),
            "CSR PEM should contain the end marker"
        );
        assert!(
            bundle
                .private_key_pem
                .starts_with("-----BEGIN PRIVATE KEY-----"),
            "key PEM should start with PKCS#8 header, got: {:?}",
            &bundle.private_key_pem[..bundle.private_key_pem.len().min(60)]
        );
    }

    #[test]
    fn generated_csrs_have_distinct_keypairs() {
        let a = generate_keypair_and_csr("core-a").unwrap();
        let b = generate_keypair_and_csr("core-b").unwrap();
        assert_ne!(
            a.private_key_pem, b.private_key_pem,
            "two enrollments must mint independent keypairs"
        );
        assert_ne!(a.csr_pem, b.csr_pem);
    }
}
