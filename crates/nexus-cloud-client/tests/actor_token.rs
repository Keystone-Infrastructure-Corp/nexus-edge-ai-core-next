//! Phase 1.7 acceptance suite — exercises every bullet point in the
//! `docs/PHASES.md` row for `nexus-cloud-client`.
//!
//! Bullets covered:
//!
//! 1. Missing token on POST rejected with `actor_token_missing`.
//! 2. Bad-signature, wrong-`aud`, wrong-`core_id`, expired, replayed
//!    `jti`, mismatched `path` all rejected with `actor_token_invalid`.
//! 3. `system`-sub invoking `create_camera` rejected with
//!    `actor_token_invalid`; `system`-sub `entitlement_update` accepted.

use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use nexus_cloud_client::actor_token::{
    EnvelopeContext, TrustedKey, VerifiedActor, Verifier, VerifierBuilder,
};
use nexus_cloud_client::dispatcher::{Handler, RpcDispatcher, SystemMethodPolicy};
use nexus_cloud_client::error::{DispatchError, InvalidReason, RejectReason};
use nexus_cloud_client::jti_cache::JtiReplayCache;
use rand_core::OsRng;
use serde_json::json;

const CORE_ID: &str = "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c0";
const ORG_ID: &str = "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c1";
/// The monitoring org a station operator acts as (Phase 11.E2).
const MONITORING_ORG_ID: &str = "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c2";

fn fresh_key(kid: &str) -> (SigningKey, TrustedKey) {
    let sk = SigningKey::generate(&mut OsRng);
    let vk = sk.verifying_key();
    (
        sk,
        TrustedKey {
            kid: kid.to_string(),
            key: vk,
        },
    )
}

fn build_verifier_with(kid: &str) -> (SigningKey, Verifier) {
    let (sk, trusted) = fresh_key(kid);
    let verifier = VerifierBuilder::new(CORE_ID)
        .trusted_key(trusted)
        .replay(Arc::new(JtiReplayCache::new()))
        .build()
        .expect("verifier");
    (sk, verifier)
}

fn b64url<S: AsRef<[u8]>>(s: S) -> String {
    URL_SAFE_NO_PAD.encode(s)
}

#[derive(Clone)]
struct TokenSpec {
    sub: String,
    role: String,
    jti: String,
    aud: String,
    core_id: String,
    org_id: String,
    method: String,
    path: String,
    iat: i64,
    exp: i64,
    kid: String,
    /// Phase 11.E2 — set when the human was acting for a monitoring org.
    actor_org_id: Option<String>,
}

impl TokenSpec {
    fn fresh(now: i64, method: &str, path: &str, kid: &str) -> Self {
        Self {
            sub: "alice@example.com".into(),
            role: "operator".into(),
            jti: uuid::Uuid::now_v7().to_string(),
            aud: "nexus-edge-rpc".into(),
            core_id: CORE_ID.into(),
            org_id: ORG_ID.into(),
            method: method.into(),
            path: path.into(),
            iat: now - 5,
            exp: now + 60,
            kid: kid.into(),
            actor_org_id: None,
        }
    }
}

fn mint(sk: &SigningKey, spec: &TokenSpec) -> String {
    let header = json!({ "alg": "EdDSA", "kid": spec.kid });
    let claims = json!({
        "aud": spec.aud,
        "core_id": spec.core_id,
        "exp": spec.exp,
        "http_method": spec.method,
        "iat": spec.iat,
        "iss": "https://entitlement.nexus.example",
        "jti": spec.jti,
        "org_id": spec.org_id,
        "path": spec.path,
        "role": spec.role,
        "sub": spec.sub,
    });
    let mut claims = claims;
    if let Some(acting) = spec.actor_org_id.as_ref() {
        claims["actor_org_id"] = json!(acting);
    }
    let h = b64url(serde_json::to_vec(&header).unwrap());
    let c = b64url(serde_json::to_vec(&claims).unwrap());
    let signing_input = format!("{h}.{c}");
    let sig = sk.sign(signing_input.as_bytes());
    let s = b64url(sig.to_bytes());
    format!("{h}.{c}.{s}")
}

struct OkHandler;

#[async_trait]
impl Handler for OkHandler {
    async fn handle(
        &self,
        _method: &str,
        _envelope: EnvelopeContext<'_>,
        _actor: &VerifiedActor,
        _body: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        Ok(b"ok".to_vec())
    }
}

// -----------------------------------------------------------------------------
// Bullet 1 — missing token on POST rejected with `actor_token_missing`.
// -----------------------------------------------------------------------------

#[tokio::test]
async fn missing_token_rejected_with_actor_token_missing() {
    let (_, verifier) = build_verifier_with("k1");
    let dispatcher = RpcDispatcher::new(verifier, SystemMethodPolicy::default(), OkHandler);
    let err = dispatcher
        .dispatch(
            "create_camera",
            EnvelopeContext {
                method: "POST",
                path: "/admin/v1/cameras",
            },
            None,
            Some(br#"{"name":"lobby"}"#),
        )
        .await
        .expect_err("dispatch must reject missing token");
    assert_eq!(err.wire_code(), "actor_token_missing");
    match err {
        DispatchError::Reject(RejectReason::Missing) => {}
        other => panic!("expected RejectReason::Missing, got {other:?}"),
    }
}

// -----------------------------------------------------------------------------
// Bullet 2 — invalid token variants → `actor_token_invalid`.
// -----------------------------------------------------------------------------

#[test]
fn happy_path_verifies() {
    let (sk, verifier) = build_verifier_with("k1");
    let now = Utc::now().timestamp();
    let spec = TokenSpec::fresh(now, "POST", "/admin/v1/cameras", "k1");
    let tok = mint(&sk, &spec);
    let actor = verifier
        .verify(
            &tok,
            EnvelopeContext {
                method: "POST",
                path: "/admin/v1/cameras",
            },
        )
        .expect("valid token");
    assert_eq!(actor.sub, "alice@example.com");
    assert_eq!(actor.role, "operator");
}

#[test]
fn bad_signature_rejected() {
    let (sk, verifier) = build_verifier_with("k1");
    let now = Utc::now().timestamp();
    let spec = TokenSpec::fresh(now, "POST", "/admin/v1/cameras", "k1");
    let tok = mint(&sk, &spec);
    // Flip a byte inside the signature segment.
    let mut parts: Vec<&str> = tok.split('.').collect();
    let mut sig_bytes = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    sig_bytes[0] ^= 0xff;
    let bad_sig_b64 = b64url(sig_bytes);
    let bad = format!("{}.{}.{}", parts[0], parts[1], bad_sig_b64);
    parts[2] = &bad_sig_b64;
    let err = verifier
        .verify(
            &bad,
            EnvelopeContext {
                method: "POST",
                path: "/admin/v1/cameras",
            },
        )
        .expect_err("bad sig");
    assert_eq!(err.wire_code(), "actor_token_invalid");
    assert!(matches!(
        err,
        RejectReason::Invalid(InvalidReason::BadSignature)
    ));
}

#[test]
fn wrong_audience_rejected() {
    let (sk, verifier) = build_verifier_with("k1");
    let now = Utc::now().timestamp();
    let mut spec = TokenSpec::fresh(now, "POST", "/admin/v1/cameras", "k1");
    spec.aud = "some-other-audience".into();
    let tok = mint(&sk, &spec);
    let err = verifier
        .verify(
            &tok,
            EnvelopeContext {
                method: "POST",
                path: "/admin/v1/cameras",
            },
        )
        .expect_err("wrong aud");
    assert_eq!(err.wire_code(), "actor_token_invalid");
    assert!(matches!(
        err,
        RejectReason::Invalid(InvalidReason::WrongAudience)
    ));
}

#[test]
fn wrong_core_id_rejected() {
    let (sk, verifier) = build_verifier_with("k1");
    let now = Utc::now().timestamp();
    let mut spec = TokenSpec::fresh(now, "POST", "/admin/v1/cameras", "k1");
    spec.core_id = "00000000-0000-0000-0000-000000000000".into();
    let tok = mint(&sk, &spec);
    let err = verifier
        .verify(
            &tok,
            EnvelopeContext {
                method: "POST",
                path: "/admin/v1/cameras",
            },
        )
        .expect_err("wrong core_id");
    assert_eq!(err.wire_code(), "actor_token_invalid");
    assert!(matches!(
        err,
        RejectReason::Invalid(InvalidReason::WrongCoreId)
    ));
}

#[test]
fn expired_rejected() {
    let (sk, verifier) = build_verifier_with("k1");
    let now = Utc::now().timestamp();
    let mut spec = TokenSpec::fresh(now, "POST", "/admin/v1/cameras", "k1");
    // Push exp well outside the ±30 s skew window.
    spec.iat = now - 300;
    spec.exp = now - 120;
    let tok = mint(&sk, &spec);
    let err = verifier
        .verify(
            &tok,
            EnvelopeContext {
                method: "POST",
                path: "/admin/v1/cameras",
            },
        )
        .expect_err("expired");
    assert_eq!(err.wire_code(), "actor_token_invalid");
    assert!(matches!(err, RejectReason::Invalid(InvalidReason::Expired)));
}

#[test]
fn replayed_jti_rejected() {
    let (sk, verifier) = build_verifier_with("k1");
    let now = Utc::now().timestamp();
    let spec = TokenSpec::fresh(now, "POST", "/admin/v1/cameras", "k1");
    let tok = mint(&sk, &spec);
    let env = EnvelopeContext {
        method: "POST",
        path: "/admin/v1/cameras",
    };
    verifier.verify(&tok, env).expect("first use accepted");
    let err = verifier.verify(&tok, env).expect_err("replay rejected");
    assert_eq!(err.wire_code(), "actor_token_invalid");
    assert!(matches!(err, RejectReason::Invalid(InvalidReason::Replay)));
}

#[test]
fn mismatched_path_rejected() {
    let (sk, verifier) = build_verifier_with("k1");
    let now = Utc::now().timestamp();
    let spec = TokenSpec::fresh(now, "POST", "/admin/v1/cameras", "k1");
    let tok = mint(&sk, &spec);
    let err = verifier
        .verify(
            &tok,
            EnvelopeContext {
                method: "POST",
                path: "/admin/v1/cameras/123/delete",
            },
        )
        .expect_err("path mismatch");
    assert_eq!(err.wire_code(), "actor_token_invalid");
    assert!(matches!(
        err,
        RejectReason::Invalid(InvalidReason::PathMismatch)
    ));
}

#[test]
fn unknown_kid_rejected() {
    let (sk, verifier) = build_verifier_with("k1");
    let now = Utc::now().timestamp();
    let spec = TokenSpec::fresh(now, "POST", "/admin/v1/cameras", "unrecognised-kid");
    let tok = mint(&sk, &spec);
    let err = verifier
        .verify(
            &tok,
            EnvelopeContext {
                method: "POST",
                path: "/admin/v1/cameras",
            },
        )
        .expect_err("unknown kid");
    assert_eq!(err.wire_code(), "actor_token_invalid");
    assert!(matches!(
        err,
        RejectReason::Invalid(InvalidReason::UnknownKeyId)
    ));
}

#[test]
fn malformed_jws_rejected() {
    let (_, verifier) = build_verifier_with("k1");
    let err = verifier
        .verify(
            "not.a.valid.jws",
            EnvelopeContext {
                method: "POST",
                path: "/admin/v1/cameras",
            },
        )
        .expect_err("malformed");
    assert_eq!(err.wire_code(), "actor_token_invalid");
    assert!(matches!(
        err,
        RejectReason::Invalid(InvalidReason::MalformedJws)
    ));
}

// -----------------------------------------------------------------------------
// Bullet 3 — system-sub policy.
// -----------------------------------------------------------------------------

#[tokio::test]
async fn system_sub_invoking_create_camera_rejected() {
    let (sk, verifier) = build_verifier_with("k1");
    let dispatcher = RpcDispatcher::new(verifier, SystemMethodPolicy::default(), OkHandler);
    let now = Utc::now().timestamp();
    let mut spec = TokenSpec::fresh(now, "POST", "/admin/v1/cameras", "k1");
    spec.sub = "system:orchestrator".into();
    spec.role = "system:orchestrator".into();
    let tok = mint(&sk, &spec);
    let err = dispatcher
        .dispatch(
            "create_camera",
            EnvelopeContext {
                method: "POST",
                path: "/admin/v1/cameras",
            },
            Some(&tok),
            None,
        )
        .await
        .expect_err("system-sub blocked on create_camera");
    assert_eq!(err.wire_code(), "actor_token_invalid");
    match err {
        DispatchError::Reject(RejectReason::Invalid(
            InvalidReason::SystemSubNotPermittedForMethod,
        )) => {}
        other => panic!("expected SystemSubNotPermittedForMethod, got {other:?}"),
    }
}

#[tokio::test]
async fn system_sub_entitlement_update_accepted() {
    let (sk, verifier) = build_verifier_with("k1");
    let dispatcher = RpcDispatcher::new(verifier, SystemMethodPolicy::default(), OkHandler);
    let now = Utc::now().timestamp();
    let mut spec = TokenSpec::fresh(now, "POST", "/admin/v1/entitlement", "k1");
    spec.sub = "system:entitlement-svc".into();
    spec.role = "system:entitlement-svc".into();
    let tok = mint(&sk, &spec);
    let body = dispatcher
        .dispatch(
            "entitlement_update",
            EnvelopeContext {
                method: "POST",
                path: "/admin/v1/entitlement",
            },
            Some(&tok),
            None,
        )
        .await
        .expect("system entitlement update accepted");
    assert_eq!(body, b"ok");
}

// -----------------------------------------------------------------------------
// Phase 11.E2 — the acting org survives verification.
// -----------------------------------------------------------------------------

/// A cross-org mutation carries TWO orgs: `org_id` is whose estate is being
/// changed, `actor_org_id` is who is doing it. The engine authorises on
/// neither — the cloud decided that against a live `monitoring_grants` row
/// before minting — but it has to RECORD both, or the on-box audit can say a
/// setting changed and who typed it while losing which company they worked
/// for.
///
/// The absent case is asserted alongside, because "no acting org" and "acting
/// org we failed to parse" must not look the same downstream.
#[test]
fn acting_org_claim_reaches_the_verified_actor() {
    let (sk, verifier) = build_verifier_with("k1");
    let now = Utc::now().timestamp();

    let mut spec = TokenSpec::fresh(now, "POST", "/admin/cameras", "k1");
    spec.actor_org_id = Some(MONITORING_ORG_ID.to_owned());
    let token = mint(&sk, &spec);

    let actor = verifier
        .verify(
            &token,
            EnvelopeContext {
                method: "POST",
                path: "/admin/cameras",
            },
        )
        .expect("token with an acting org must still verify");
    assert_eq!(
        actor.actor_org_id.as_deref(),
        Some(MONITORING_ORG_ID),
        "the acting org must survive verification"
    );
    assert_eq!(actor.org_id, ORG_ID, "org_id still names the target estate");

    // An ordinary, same-org mutation leaves it unset rather than echoing the
    // target org — a row that always names an acting org cannot be used to
    // find the ones that had a station behind them.
    let plain = mint(&sk, &TokenSpec::fresh(now, "POST", "/admin/sites", "k1"));
    let actor = verifier
        .verify(
            &plain,
            EnvelopeContext {
                method: "POST",
                path: "/admin/sites",
            },
        )
        .expect("ordinary token verifies");
    assert_eq!(actor.actor_org_id, None);
}
