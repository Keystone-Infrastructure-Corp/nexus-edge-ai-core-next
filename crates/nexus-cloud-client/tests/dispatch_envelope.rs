//! Phase 1.7 — `RpcDispatcher::dispatch_envelope` acceptance.
//!
//! Verifies the convenience entry point that takes a wire-protocol
//! `Envelope` directly (no `EnvelopeContext` boilerplate at the call
//! site).
//!
//! Coverage:
//!
//! 1. Happy path: valid `rpc_call` envelope produces an
//!    `RpcResponsePayload { status: 200, body }`.
//! 2. `request_id` from the envelope is fed into the replay cache so an
//!    idempotent retry with a fresh `jti` but the same `request_id` is
//!    admitted (Phase 1.16 contract).
//! 3. Calling `dispatch_envelope` with a non-`rpc_call` envelope is
//!    rejected (defensive; the engine should never do this).

use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use nexus_cloud_client::actor_token::{
    EnvelopeContext, TrustedKey, VerifiedActor, VerifierBuilder,
};
use nexus_cloud_client::dispatcher::{Handler, RpcDispatcher, SystemMethodPolicy};
use nexus_cloud_client::error::{DispatchError, InvalidReason, RejectReason};
use nexus_cloud_client::jti_cache::JtiReplayCache;
use nexus_cloud_protocol::v1::{
    Envelope, EnvelopeBody, EnvelopeMeta, HeartbeatPayload, RpcCallPayload,
};
use rand_core::OsRng;
use serde_json::json;

const CORE_ID: &str = "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c0";
const ORG_ID: &str = "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c1";

fn b64url<S: AsRef<[u8]>>(s: S) -> String {
    URL_SAFE_NO_PAD.encode(s)
}

fn mint(sk: &SigningKey, kid: &str, jti: &str, method: &str, path: &str, now: i64) -> String {
    let header = json!({ "alg": "EdDSA", "kid": kid });
    let claims = json!({
        "aud": "nexus-edge-rpc",
        "core_id": CORE_ID,
        "exp": now + 60,
        "http_method": method,
        "iat": now - 5,
        "iss": "https://entitlement.nexus.example",
        "jti": jti,
        "org_id": ORG_ID,
        "path": path,
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

struct EchoHandler;

#[async_trait]
impl Handler for EchoHandler {
    async fn handle(
        &self,
        _method: &str,
        _envelope: EnvelopeContext<'_>,
        _actor: &VerifiedActor,
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        let payload = body.unwrap_or(b"");
        Ok(format!(r#"{{"echoed":{}}}"#, payload.len()).into_bytes())
    }
}

fn build_dispatcher() -> (SigningKey, RpcDispatcher<EchoHandler>) {
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
    let dispatcher = RpcDispatcher::new(verifier, SystemMethodPolicy::default(), EchoHandler);
    (sk, dispatcher)
}

fn rpc_envelope(payload: RpcCallPayload) -> Envelope {
    Envelope {
        meta: EnvelopeMeta {
            id: uuid::Uuid::now_v7().to_string(),
            in_reply_to: None,
            seq: None,
            trace: None,
            ts: Utc::now().to_rfc3339(),
            v: 1,
        },
        body: EnvelopeBody::RpcCall(payload),
    }
}

#[tokio::test]
async fn dispatch_envelope_happy_path() {
    let (sk, dispatcher) = build_dispatcher();
    let now = Utc::now().timestamp();
    let jti = uuid::Uuid::now_v7().to_string();
    let tok = mint(&sk, "k1", &jti, "POST", "/admin/v1/cameras", now);

    let env = rpc_envelope(RpcCallPayload {
        actor_token: Some(tok),
        body: None,
        headers: None,
        method: "POST".into(),
        path: "/admin/v1/cameras".into(),
        request_id: Some(uuid::Uuid::now_v7().to_string()),
    });

    let resp = dispatcher
        .dispatch_envelope(&env)
        .await
        .expect("dispatch succeeds");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, json!({ "echoed": 0 }));
}

#[tokio::test]
async fn dispatch_envelope_idempotent_retry_with_fresh_jti_is_admitted() {
    let (sk, dispatcher) = build_dispatcher();
    let now = Utc::now().timestamp();
    let request_id = uuid::Uuid::now_v7().to_string();

    // Cloud's first attempt.
    let jti1 = uuid::Uuid::now_v7().to_string();
    let tok1 = mint(&sk, "k1", &jti1, "POST", "/admin/v1/cameras", now);
    let env1 = rpc_envelope(RpcCallPayload {
        actor_token: Some(tok1),
        body: None,
        headers: None,
        method: "POST".into(),
        path: "/admin/v1/cameras".into(),
        request_id: Some(request_id.clone()),
    });
    dispatcher
        .dispatch_envelope(&env1)
        .await
        .expect("first attempt accepted");

    // Cloud retries with the SAME request_id but a FRESHLY-minted token
    // (new jti, new sig) — must be admitted by the replay layer.
    let jti2 = uuid::Uuid::now_v7().to_string();
    let tok2 = mint(&sk, "k1", &jti2, "POST", "/admin/v1/cameras", now);
    let env2 = rpc_envelope(RpcCallPayload {
        actor_token: Some(tok2),
        body: None,
        headers: None,
        method: "POST".into(),
        path: "/admin/v1/cameras".into(),
        request_id: Some(request_id.clone()),
    });
    dispatcher
        .dispatch_envelope(&env2)
        .await
        .expect("retry with fresh jti + same request_id accepted");
}

#[tokio::test]
async fn dispatch_envelope_true_replay_still_rejected() {
    let (sk, dispatcher) = build_dispatcher();
    let now = Utc::now().timestamp();
    let request_id = uuid::Uuid::now_v7().to_string();
    let jti = uuid::Uuid::now_v7().to_string();
    let tok = mint(&sk, "k1", &jti, "POST", "/admin/v1/cameras", now);
    let env = rpc_envelope(RpcCallPayload {
        actor_token: Some(tok),
        body: None,
        headers: None,
        method: "POST".into(),
        path: "/admin/v1/cameras".into(),
        request_id: Some(request_id),
    });

    dispatcher
        .dispatch_envelope(&env)
        .await
        .expect("first use accepted");
    let err = dispatcher
        .dispatch_envelope(&env)
        .await
        .expect_err("same (jti, request_id) is a replay");
    match err {
        DispatchError::Reject(RejectReason::Invalid(InvalidReason::Replay)) => {}
        other => panic!("expected Replay, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_envelope_rejects_non_rpc_call() {
    let (_sk, dispatcher) = build_dispatcher();
    let env = Envelope {
        meta: EnvelopeMeta {
            id: uuid::Uuid::now_v7().to_string(),
            in_reply_to: None,
            seq: None,
            trace: None,
            ts: Utc::now().to_rfc3339(),
            v: 1,
        },
        body: EnvelopeBody::Heartbeat(HeartbeatPayload {
            edge_ts_unix_ms: None,
            name: None,
            online_cameras: 0,
            queued_alerts: 0,
            release: None,
            tier: "t36".into(),
            uptime_s: 0,
            version: "0.5.0".into(),
        }),
    };
    let err = dispatcher
        .dispatch_envelope(&env)
        .await
        .expect_err("non-rpc_call rejected");
    assert!(matches!(
        err,
        DispatchError::Reject(RejectReason::Invalid(InvalidReason::MalformedClaims))
    ));
}

/// Spy handler that records the actor passed to it. Used to confirm
/// the dispatcher synthesises the `anonymous:cloud-read` actor for
/// token-less read-only RPCs (ARCHITECTURE.md §5.7).
struct ArcSpyHandler {
    seen: Arc<std::sync::Mutex<Option<VerifiedActor>>>,
}

#[async_trait]
impl Handler for ArcSpyHandler {
    async fn handle(
        &self,
        _method: &str,
        _envelope: EnvelopeContext<'_>,
        actor: &VerifiedActor,
        _body: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        *self.seen.lock().unwrap() = Some(actor.clone());
        Ok(b"{}".to_vec())
    }
}

fn build_dispatcher_with_spy() -> (
    RpcDispatcher<ArcSpyHandler>,
    Arc<std::sync::Mutex<Option<VerifiedActor>>>,
) {
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
    let seen = Arc::new(std::sync::Mutex::new(None));
    let handler = ArcSpyHandler { seen: seen.clone() };
    let dispatcher = RpcDispatcher::new(verifier, SystemMethodPolicy::default(), handler);
    (dispatcher, seen)
}

#[tokio::test]
async fn dispatch_envelope_anonymous_get_without_actor_token_accepted() {
    let (dispatcher, seen) = build_dispatcher_with_spy();
    let env = rpc_envelope(RpcCallPayload {
        actor_token: None,
        body: None,
        headers: None,
        method: "GET".into(),
        path: "/admin/discovery/sessions/abc".into(),
        request_id: Some(uuid::Uuid::now_v7().to_string()),
    });

    let resp = dispatcher
        .dispatch_envelope(&env)
        .await
        .expect("token-less GET accepted as anonymous read");
    assert_eq!(resp.status, 200);

    let captured = seen
        .lock()
        .unwrap()
        .clone()
        .expect("handler observed the synthesised actor");
    assert_eq!(captured.sub, "anonymous:cloud-read");
    assert_eq!(captured.role, "anonymous");
}

#[tokio::test]
async fn dispatch_envelope_anonymous_head_and_options_also_accepted() {
    for method in ["HEAD", "OPTIONS", "get", "Head"] {
        let (dispatcher, _seen) = build_dispatcher_with_spy();
        let env = rpc_envelope(RpcCallPayload {
            actor_token: None,
            body: None,
            headers: None,
            method: method.into(),
            path: "/admin/anything".into(),
            request_id: Some(uuid::Uuid::now_v7().to_string()),
        });
        let resp = dispatcher
            .dispatch_envelope(&env)
            .await
            .unwrap_or_else(|e| panic!("method {method} should be accepted, got {e:?}"));
        assert_eq!(resp.status, 200, "method {method}");
    }
}

#[tokio::test]
async fn dispatch_envelope_mutating_without_actor_token_still_rejected() {
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let (dispatcher, _seen) = build_dispatcher_with_spy();
        let env = rpc_envelope(RpcCallPayload {
            actor_token: None,
            body: None,
            headers: None,
            method: method.into(),
            path: "/admin/v1/cameras".into(),
            request_id: Some(uuid::Uuid::now_v7().to_string()),
        });
        let err = dispatcher
            .dispatch_envelope(&env)
            .await
            .expect_err("mutating method without token must reject");
        assert!(
            matches!(err, DispatchError::Reject(RejectReason::Missing)),
            "method {method}: expected RejectReason::Missing, got {err:?}"
        );
    }
}
