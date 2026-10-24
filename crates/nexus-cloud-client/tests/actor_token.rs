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
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
    /// SPEC-071 P3 — the catalog permission the cloud authorized on.
    perm: Option<String>,
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
            perm: None,
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
    if let Some(perm) = spec.perm.as_ref() {
        claims["perm"] = json!(perm);
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

/// The verified capability is retained without inventing a preset role.
#[test]
fn permission_claim_is_retained_without_promoting_the_role() {
    let (sk, verifier) = build_verifier_with("k1");
    let now = Utc::now().timestamp();

    let mut spec = TokenSpec::fresh(now, "POST", "/admin/cameras", "k1");
    spec.role = "viewer".into();
    spec.perm = Some("settings.write".to_owned());
    let token = mint(&sk, &spec);

    let actor = verifier
        .verify(
            &token,
            EnvelopeContext {
                method: "POST",
                path: "/admin/cameras",
            },
        )
        .expect("a token carrying `perm` must verify on a build that knows it");
    assert_eq!(actor.role, "viewer");
    assert_eq!(actor.perm.as_deref(), Some("settings.write"));

    // A signer that supplied no permission omits the claim entirely; the
    // same build must still accept that token.
    let legacy = mint(&sk, &TokenSpec::fresh(now, "POST", "/admin/sites", "k1"));
    verifier
        .verify(
            &legacy,
            EnvelopeContext {
                method: "POST",
                path: "/admin/sites",
            },
        )
        .expect("a token without `perm` must keep verifying");
}

#[tokio::test]
async fn a_signed_permission_must_match_the_actual_action_before_dispatch() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHandler(Arc<AtomicUsize>);
    #[async_trait]
    impl Handler for CountingHandler {
        async fn handle(
            &self,
            _: &str,
            _: EnvelopeContext<'_>,
            _: &VerifiedActor,
            _: Option<&[u8]>,
        ) -> Result<Vec<u8>, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }
    let (sk, verifier) = build_verifier_with("k1");
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatcher = RpcDispatcher::new(
        verifier,
        SystemMethodPolicy::default(),
        CountingHandler(calls.clone()),
    );
    for (method, path, permission) in [
        ("PUT", "/admin/cameras/1", "live.ptz"),
        (
            "POST",
            "/admin/cameras/1/firmware:upgrade",
            "settings.write",
        ),
        (
            "PUT",
            "/admin/cameras/1/network/interface",
            "settings.write",
        ),
        ("PUT", "/admin/remote_access", "settings.write"),
        ("POST", "/admin/proxy", "settings.write"),
        ("POST", "/admin/cameras/1/onvif/raw", "settings.write"),
        ("POST", "/admin/unknown", "settings.write"),
        ("PUT", "/admin/cameras/1", "unknown.permission"),
        (
            "PUT",
            "/admin/cameras/1/../../remote_access",
            "settings.write",
        ),
        ("PUT", "/admin/cameras/%31", "settings.write"),
    ] {
        let mut spec = TokenSpec::fresh(Utc::now().timestamp(), method, path, "k1");
        spec.role = "owner".into();
        spec.perm = Some(permission.into());
        let result = dispatcher
            .dispatch(
                "write",
                EnvelopeContext { method, path },
                Some(&mint(&sk, &spec)),
                None,
            )
            .await;
        assert!(result.is_err(), "{method} {path} accepted {permission}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "denial must precede handler invocation"
        );
    }
}

#[tokio::test]
async fn signed_camera_and_ptz_actions_bind_the_body_before_the_handler() {
    let (sk, verifier) = build_verifier_with("k1");
    let dispatcher = RpcDispatcher::new(verifier, SystemMethodPolicy::default(), OkHandler);
    for (path, permission, body, allowed) in [
        (
            "/admin/cameras/1/ptz",
            "live.ptz",
            json!({"action":"move"}),
            true,
        ),
        (
            "/admin/cameras/1/ptz",
            "live.ptz",
            json!({"action":"set_home"}),
            false,
        ),
        (
            "/admin/cameras/1/ptz",
            "settings.write",
            json!({"action":"set_home"}),
            true,
        ),
        (
            "/admin/cameras/1/ptz",
            "settings.write",
            json!({"action":"move"}),
            false,
        ),
        (
            "/admin/cameras/1/ptz",
            "live.ptz",
            json!({"action":"move","camera_id":2}),
            false,
        ),
        (
            "/admin/cameras/1/ptz",
            "live.ptz",
            json!({"action":"move","nested":{"camera_ids":[1,2]}}),
            false,
        ),
        (
            "/admin/cameras/1/ptz",
            "live.ptz",
            json!({"action":"move","camera_id":1}),
            true,
        ),
        (
            "/admin/cameras/1/ptz",
            "live.ptz",
            json!({"action":"new_unknown_action"}),
            false,
        ),
    ] {
        let mut spec = TokenSpec::fresh(Utc::now().timestamp(), "POST", path, "k1");
        spec.role = "viewer".into();
        spec.perm = Some(permission.into());
        let result = dispatcher
            .dispatch(
                "ptz",
                EnvelopeContext {
                    method: "POST",
                    path,
                },
                Some(&mint(&sk, &spec)),
                Some(&serde_json::to_vec(&body).unwrap()),
            )
            .await;
        assert_eq!(result.is_ok(), allowed, "{permission} {body}");
    }
}

#[tokio::test]
async fn signed_permissions_preserve_the_system_method_allowlist() {
    let (sk, verifier) = build_verifier_with("k1");
    let dispatcher = RpcDispatcher::new(verifier, SystemMethodPolicy::default(), OkHandler);
    let mut spec = TokenSpec::fresh(Utc::now().timestamp(), "POST", "/admin/cameras", "k1");
    spec.sub = "system:fleet".into();
    spec.perm = Some("settings.write".into());
    let err = dispatcher
        .dispatch(
            "create_camera",
            EnvelopeContext {
                method: "POST",
                path: "/admin/cameras",
            },
            Some(&mint(&sk, &spec)),
            Some(b"{}"),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        DispatchError::Reject(RejectReason::Invalid(
            InvalidReason::SystemSubNotPermittedForMethod
        ))
    ));
}

#[test]
fn logical_tunnel_permissions_are_exact_and_keep_viewer_role() {
    let (sk, verifier) = build_verifier_with("k1");
    for (path, required) in [
        ("diag_collect", "diagnostics.collect"),
        ("update_assignment", "fleet.apply"),
        ("update_cancel", "fleet.apply"),
        ("update_rollback", "fleet.apply"),
    ] {
        for permission in [required, "settings.write"] {
            let mut spec = TokenSpec::fresh(Utc::now().timestamp(), "POST", path, "k1");
            spec.role = "viewer".into();
            spec.perm = Some(permission.into());
            let result = verifier.verify(
                &mint(&sk, &spec),
                EnvelopeContext {
                    method: "POST",
                    path,
                },
            );
            assert_eq!(result.is_ok(), permission == required);
            if let Ok(actor) = result {
                assert_eq!(actor.role, "viewer");
                assert!(actor.allows_rpc(
                    EnvelopeContext {
                        method: "POST",
                        path
                    },
                    None,
                    false
                ));
            }
        }
    }
}
