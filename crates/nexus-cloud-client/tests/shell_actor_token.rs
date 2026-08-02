//! Acceptance suite for the remote-shell actor token.
//!
//! A shell token is a *different* credential from an RPC actor token: it
//! carries `aud = nexus-edge-shell` and a `session_id` instead of an HTTP
//! method + path. The two must not be interchangeable in either direction,
//! because an RPC token is minted for a far broader population than the one
//! allowed to open a shell.

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use nexus_cloud_client::actor_token::{TrustedKey, Verifier, VerifierBuilder, SHELL_AUDIENCE};
use nexus_cloud_client::error::{InvalidReason, RejectReason};
use nexus_cloud_client::jti_cache::JtiReplayCache;
use rand_core::OsRng;
use serde_json::json;

const CORE_ID: &str = "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c0";
const ORG_ID: &str = "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c1";
const SESSION_ID: &str = "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c2";
const KID: &str = "shell-test-kid";

fn build() -> (SigningKey, Verifier) {
    let sk = SigningKey::generate(&mut OsRng);
    let vk = sk.verifying_key();
    let verifier = VerifierBuilder::new(CORE_ID)
        .trusted_key(TrustedKey {
            kid: KID.to_string(),
            key: vk,
        })
        .replay(Arc::new(JtiReplayCache::new()))
        .build()
        .expect("verifier");
    (sk, verifier)
}

fn b64url<S: AsRef<[u8]>>(s: S) -> String {
    URL_SAFE_NO_PAD.encode(s)
}

/// Mints a shell token. Every field is a parameter so a test can bend
/// exactly one of them and assert the verifier notices.
#[allow(clippy::too_many_arguments)]
fn mint(
    sk: &SigningKey,
    aud: &str,
    core_id: &str,
    session_id: &str,
    jti: &str,
    iat: i64,
    exp: i64,
) -> String {
    let header = json!({ "alg": "EdDSA", "kid": KID });
    let claims = json!({
        "aud": aud,
        "core_id": core_id,
        "exp": exp,
        "iat": iat,
        "iss": "https://entitlement.nexus.example",
        "jti": jti,
        "org_id": ORG_ID,
        "role": "admin",
        "session_id": session_id,
        "sub": "alice@example.com",
    });
    let h = b64url(serde_json::to_vec(&header).unwrap());
    let c = b64url(serde_json::to_vec(&claims).unwrap());
    let signing_input = format!("{h}.{c}");
    let s = b64url(sk.sign(signing_input.as_bytes()).to_bytes());
    format!("{h}.{c}.{s}")
}

fn fresh(sk: &SigningKey) -> String {
    let now = Utc::now().timestamp();
    mint(
        sk,
        SHELL_AUDIENCE,
        CORE_ID,
        SESSION_ID,
        &uuid::Uuid::now_v7().to_string(),
        now - 5,
        now + 60,
    )
}

#[test]
fn a_well_formed_shell_token_is_accepted() {
    let (sk, v) = build();
    let actor = v.verify_shell(&fresh(&sk), SESSION_ID).expect("accepted");
    assert_eq!(actor.org_id, ORG_ID);
    assert_eq!(actor.role, "admin");
}

#[test]
fn an_rpc_audience_token_cannot_open_a_shell() {
    let (sk, v) = build();
    let now = Utc::now().timestamp();
    let token = mint(
        &sk,
        "nexus-edge-rpc",
        CORE_ID,
        SESSION_ID,
        &uuid::Uuid::now_v7().to_string(),
        now - 5,
        now + 60,
    );
    assert!(matches!(
        v.verify_shell(&token, SESSION_ID),
        Err(RejectReason::Invalid(InvalidReason::WrongAudience))
    ));
}

#[test]
fn a_token_for_another_session_is_rejected() {
    let (sk, v) = build();
    assert!(matches!(
        v.verify_shell(&fresh(&sk), "0190f7be-7c6a-7d4f-8f01-ffffffffffff"),
        Err(RejectReason::Invalid(InvalidReason::PathMismatch))
    ));
}

#[test]
fn a_token_for_another_core_is_rejected() {
    let (sk, v) = build();
    let now = Utc::now().timestamp();
    let token = mint(
        &sk,
        SHELL_AUDIENCE,
        "0190f7be-7c6a-7d4f-8f01-aaaaaaaaaaaa",
        SESSION_ID,
        &uuid::Uuid::now_v7().to_string(),
        now - 5,
        now + 60,
    );
    assert!(matches!(
        v.verify_shell(&token, SESSION_ID),
        Err(RejectReason::Invalid(InvalidReason::WrongCoreId))
    ));
}

#[test]
fn an_expired_token_is_rejected() {
    let (sk, v) = build();
    let now = Utc::now().timestamp();
    let token = mint(
        &sk,
        SHELL_AUDIENCE,
        CORE_ID,
        SESSION_ID,
        &uuid::Uuid::now_v7().to_string(),
        now - 600,
        now - 300,
    );
    assert!(matches!(
        v.verify_shell(&token, SESSION_ID),
        Err(RejectReason::Invalid(InvalidReason::Expired))
    ));
}

#[test]
fn a_replayed_shell_token_is_rejected() {
    let (sk, v) = build();
    let token = fresh(&sk);
    v.verify_shell(&token, SESSION_ID).expect("first use");
    assert!(matches!(
        v.verify_shell(&token, SESSION_ID),
        Err(RejectReason::Invalid(InvalidReason::Replay))
    ));
}

#[test]
fn a_foreign_signature_is_rejected() {
    let (_sk, v) = build();
    let other = SigningKey::generate(&mut OsRng);
    assert!(v.verify_shell(&fresh(&other), SESSION_ID).is_err());
}
