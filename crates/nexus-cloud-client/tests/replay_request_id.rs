//! Phase 1.16 acceptance — replay cache widened to `(jti, request_id)`.
//!
//! Verifies the two new APIs on [`Verifier`]/[`JtiReplayCache`] behave
//! as the wedge plan demands:
//!
//! 1. Same `jti`, distinct `request_id` values → both accepted.
//! 2. Same `(jti, request_id)` tuple twice → second is `Replay`.
//! 3. Calling [`Verifier::verify`] with no `request_id` (the v1.7
//!    contract) still rejects a repeat of the same `jti`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use nexus_cloud_client::actor_token::{EnvelopeContext, TrustedKey, VerifierBuilder};
use nexus_cloud_client::error::{InvalidReason, RejectReason};
use nexus_cloud_client::jti_cache::JtiReplayCache;
use rand_core::OsRng;
use serde_json::json;

const CORE_ID: &str = "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c0";
const ORG_ID: &str = "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c1";

fn b64url<S: AsRef<[u8]>>(s: S) -> String {
    URL_SAFE_NO_PAD.encode(s)
}

fn mint(sk: &SigningKey, kid: &str, jti: &str, now: i64) -> String {
    let header = json!({ "alg": "EdDSA", "kid": kid });
    let claims = json!({
        "aud": "nexus-edge-rpc",
        "core_id": CORE_ID,
        "exp": now + 60,
        "http_method": "POST",
        "iat": now - 5,
        "iss": "https://entitlement.nexus.example",
        "jti": jti,
        "org_id": ORG_ID,
        "path": "/admin/v1/cameras",
        "role": "operator",
        "sub": "alice@example.com",
    });
    let h = b64url(serde_json::to_vec(&header).unwrap());
    let c = b64url(serde_json::to_vec(&claims).unwrap());
    let signing_input = format!("{h}.{c}");
    let sig = sk.sign(signing_input.as_bytes());
    let s = b64url(sig.to_bytes());
    format!("{h}.{c}.{s}")
}

#[test]
fn same_jti_different_request_id_is_not_a_replay() {
    let sk = SigningKey::generate(&mut OsRng);
    let trusted = TrustedKey {
        kid: "k1".into(),
        key: sk.verifying_key(),
    };
    let verifier = VerifierBuilder::new(CORE_ID)
        .trusted_key(trusted)
        .replay(Arc::new(JtiReplayCache::new()))
        .build()
        .expect("verifier");

    let now = Utc::now().timestamp();
    let jti = uuid::Uuid::now_v7().to_string();
    let tok = mint(&sk, "k1", &jti, now);
    let env = EnvelopeContext {
        method: "POST",
        path: "/admin/v1/cameras",
    };

    // Same jti, two distinct request_ids → both accepted.
    verifier
        .verify_with_request_id(&tok, env, Some("req-a"))
        .expect("first request_id accepted");
    verifier
        .verify_with_request_id(&tok, env, Some("req-b"))
        .expect("second distinct request_id accepted");
}

#[test]
fn same_jti_and_request_id_tuple_is_a_replay() {
    let sk = SigningKey::generate(&mut OsRng);
    let trusted = TrustedKey {
        kid: "k1".into(),
        key: sk.verifying_key(),
    };
    let verifier = VerifierBuilder::new(CORE_ID)
        .trusted_key(trusted)
        .replay(Arc::new(JtiReplayCache::new()))
        .build()
        .expect("verifier");

    let now = Utc::now().timestamp();
    let jti = uuid::Uuid::now_v7().to_string();
    let tok = mint(&sk, "k1", &jti, now);
    let env = EnvelopeContext {
        method: "POST",
        path: "/admin/v1/cameras",
    };

    verifier
        .verify_with_request_id(&tok, env, Some("req-same"))
        .expect("first use accepted");
    let err = verifier
        .verify_with_request_id(&tok, env, Some("req-same"))
        .expect_err("same tuple is a replay");
    assert!(matches!(err, RejectReason::Invalid(InvalidReason::Replay)));
}

#[test]
fn v1_7_contract_without_request_id_still_dedups_on_jti() {
    let sk = SigningKey::generate(&mut OsRng);
    let trusted = TrustedKey {
        kid: "k1".into(),
        key: sk.verifying_key(),
    };
    let verifier = VerifierBuilder::new(CORE_ID)
        .trusted_key(trusted)
        .replay(Arc::new(JtiReplayCache::new()))
        .build()
        .expect("verifier");

    let now = Utc::now().timestamp();
    let jti = uuid::Uuid::now_v7().to_string();
    let tok = mint(&sk, "k1", &jti, now);
    let env = EnvelopeContext {
        method: "POST",
        path: "/admin/v1/cameras",
    };

    verifier.verify(&tok, env).expect("first use accepted");
    let err = verifier
        .verify(&tok, env)
        .expect_err("v1.7 path replays on jti alone");
    assert!(matches!(err, RejectReason::Invalid(InvalidReason::Replay)));
}
