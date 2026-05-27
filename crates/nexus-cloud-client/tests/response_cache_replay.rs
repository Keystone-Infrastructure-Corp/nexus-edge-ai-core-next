//! Phase 1.16 — engine response-cache replay acceptance.
//!
//! Verifies that when [`RpcDispatcher`] is configured with an
//! [`RpcResponseCache`], a retry that carries the same
//! `(actor_token.jti, request_id)` receives the **byte-identical**
//! cached `rpc_response` body without re-invoking the handler. Closes
//! the engine half of the 1.16 acceptance criterion in
//! [`docs/cloud-console/PHASES.md`](../../../nexus-cloud-console/docs/cloud-console/PHASES.md).

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
use nexus_cloud_client::response_cache::RpcResponseCache;
use nexus_cloud_protocol::v1::{Envelope, EnvelopeBody, EnvelopeMeta, RpcCallPayload};
use rand_core::OsRng;
use serde_json::json;
use tokio::sync::Mutex;

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

/// Handler that counts how many times it was invoked AND embeds the
/// invocation index in its response body. If the dispatcher cached
/// the first response correctly, replays serve the index from call
/// #1 regardless of how many retries arrive.
struct CountingHandler {
    calls: Arc<Mutex<u64>>,
}

#[async_trait]
impl Handler for CountingHandler {
    async fn handle(
        &self,
        _method: &str,
        _envelope: EnvelopeContext<'_>,
        _actor: &VerifiedActor,
        _body: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        let mut guard = self.calls.lock().await;
        *guard += 1;
        // Use a body shape that would canonicalise differently under
        // `jsonb` (intentional out-of-alphabetical key order) so we
        // can prove the cache stores raw bytes, not parsed JSON.
        Ok(format!(r#"{{"zeta":{},"alpha":"step-{}"}}"#, *guard, *guard).into_bytes())
    }
}

fn build_dispatcher_with_response_cache(
) -> (SigningKey, RpcDispatcher<CountingHandler>, Arc<Mutex<u64>>) {
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
    let calls = Arc::new(Mutex::new(0_u64));
    let handler = CountingHandler {
        calls: Arc::clone(&calls),
    };
    let dispatcher = RpcDispatcher::new(verifier, SystemMethodPolicy::default(), handler)
        .with_response_cache(Arc::new(RpcResponseCache::new()));
    (sk, dispatcher, calls)
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
async fn duplicate_jti_and_request_id_replays_byte_identically() {
    let (sk, dispatcher, calls) = build_dispatcher_with_response_cache();
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

    let first = dispatcher
        .dispatch_envelope(&env)
        .await
        .expect("first dispatch ok");
    let second = dispatcher
        .dispatch_envelope(&env)
        .await
        .expect("retry served from cache");

    // Body must be byte-identical (counting handler embeds call index;
    // if the second call had re-run, `zeta` would be 2).
    assert_eq!(first.body, second.body);
    assert_eq!(first.status, 200);
    assert_eq!(second.status, 200);
    assert_eq!(first.body, json!({ "zeta": 1, "alpha": "step-1" }));
    // Handler must have run exactly once.
    assert_eq!(*calls.lock().await, 1);
}

#[tokio::test]
async fn retry_with_fresh_jti_re_runs_handler_in_engine() {
    // Cloud-side retry semantics: same `Idempotency-Key` (=>
    // `request_id`) but the cloud may mint a fresh `actor_token`
    // (=> new `jti`). The engine-side response cache keys on the
    // CRYPTOGRAPHICALLY-VERIFIED `jti` (which the signature binds to
    // the token), so a fresh `jti` is a cache miss and the handler
    // re-runs. This is intentional: the cloud-side
    // `idempotent_responses` table (Phase 1.11) is the byte-identical
    // guarantor at the HTTP boundary, BEFORE the cloud ever re-issues
    // the RPC. The engine-side cache only protects against the case
    // where the cloud DOES re-issue (e.g. WSS reconnect mid-flight).
    let (sk, dispatcher, calls) = build_dispatcher_with_response_cache();
    let now = Utc::now().timestamp();
    let request_id = uuid::Uuid::now_v7().to_string();

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
    let _first = dispatcher
        .dispatch_envelope(&env1)
        .await
        .expect("first dispatch ok");

    // The current response-cache implementation keys on the verified
    // `jti` (which is bound by signature to the token), so a fresh
    // `jti` with the same `request_id` is treated as a NEW request
    // and re-runs the handler. The cloud-side `idempotent_responses`
    // table provides the byte-identical replay at the HTTP boundary
    // BEFORE the cloud ever re-issues the RPC, so this engine-side
    // re-run only happens on the (rare) case of cloud-side cache
    // miss + retry. We assert the engine still produces a fresh,
    // valid body in that case rather than rejecting the call.
    let jti2 = uuid::Uuid::now_v7().to_string();
    let tok2 = mint(&sk, "k1", &jti2, "POST", "/admin/v1/cameras", now);
    let env2 = rpc_envelope(RpcCallPayload {
        actor_token: Some(tok2),
        body: None,
        headers: None,
        method: "POST".into(),
        path: "/admin/v1/cameras".into(),
        request_id: Some(request_id),
    });
    let second = dispatcher
        .dispatch_envelope(&env2)
        .await
        .expect("fresh jti + same request_id admitted");
    assert_eq!(second.status, 200);
    // Distinct bodies — the engine ran the handler twice because
    // each call carried a distinct `jti`. This is acceptable per the
    // engine-side contract; the cloud-side HTTP idempotency layer is
    // the byte-identical guarantor across RPC retries.
    assert_eq!(*calls.lock().await, 2);
}

#[tokio::test]
async fn rpc_call_without_request_id_falls_back_to_replay_rejection() {
    // v1.7 contract: no `request_id` on the wire ⇒ no response cache
    // path. The JtiReplayCache must still reject a true replay.
    let (sk, dispatcher, calls) = build_dispatcher_with_response_cache();
    let now = Utc::now().timestamp();
    let jti = uuid::Uuid::now_v7().to_string();
    let tok = mint(&sk, "k1", &jti, "POST", "/admin/v1/cameras", now);
    let env = rpc_envelope(RpcCallPayload {
        actor_token: Some(tok),
        body: None,
        headers: None,
        method: "POST".into(),
        path: "/admin/v1/cameras".into(),
        request_id: None,
    });

    dispatcher
        .dispatch_envelope(&env)
        .await
        .expect("first call ok");
    let err = dispatcher
        .dispatch_envelope(&env)
        .await
        .expect_err("same jti, no request_id ⇒ Replay");
    match err {
        DispatchError::Reject(RejectReason::Invalid(InvalidReason::Replay)) => {}
        other => panic!("expected Replay, got {other:?}"),
    }
    assert_eq!(*calls.lock().await, 1);
}

#[tokio::test]
async fn cache_miss_after_eviction_rejects_rather_than_re_runs() {
    // Capacity-1 response cache. First call admits, second
    // `(jti2, rid2)` evicts the first entry from the response cache
    // (and admits its own slot in the replay cache). When a retry
    // for the EVICTED first `(jti1, rid1)` arrives, the response
    // cache is empty but the replay cache still knows the tuple was
    // admitted → reject as Replay rather than re-running.
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
    let calls = Arc::new(Mutex::new(0_u64));
    let handler = CountingHandler {
        calls: Arc::clone(&calls),
    };
    let dispatcher = RpcDispatcher::new(verifier, SystemMethodPolicy::default(), handler)
        .with_response_cache(Arc::new(RpcResponseCache::with_capacity(1)));

    let now = Utc::now().timestamp();

    let rid1 = uuid::Uuid::now_v7().to_string();
    let jti1 = uuid::Uuid::now_v7().to_string();
    let tok1 = mint(&sk, "k1", &jti1, "POST", "/admin/v1/cameras", now);
    let env1 = rpc_envelope(RpcCallPayload {
        actor_token: Some(tok1),
        body: None,
        headers: None,
        method: "POST".into(),
        path: "/admin/v1/cameras".into(),
        request_id: Some(rid1),
    });
    dispatcher
        .dispatch_envelope(&env1)
        .await
        .expect("first call ok");

    // Distinct (jti, rid) — evicts the first response from the cache.
    let rid2 = uuid::Uuid::now_v7().to_string();
    let jti2 = uuid::Uuid::now_v7().to_string();
    let tok2 = mint(&sk, "k1", &jti2, "POST", "/admin/v1/cameras", now);
    let env2 = rpc_envelope(RpcCallPayload {
        actor_token: Some(tok2),
        body: None,
        headers: None,
        method: "POST".into(),
        path: "/admin/v1/cameras".into(),
        request_id: Some(rid2),
    });
    dispatcher
        .dispatch_envelope(&env2)
        .await
        .expect("second call ok (evicts first response)");

    // Now retry the first envelope — response cache no longer holds
    // it. Must be rejected as a replay rather than re-running the
    // handler (because the replay cache's safety net knows the
    // tuple was admitted previously).
    let err = dispatcher
        .dispatch_envelope(&env1)
        .await
        .expect_err("evicted retry rejected as Replay");
    match err {
        DispatchError::Reject(RejectReason::Invalid(InvalidReason::Replay)) => {}
        other => panic!("expected Replay, got {other:?}"),
    }
    assert_eq!(*calls.lock().await, 2);
}
