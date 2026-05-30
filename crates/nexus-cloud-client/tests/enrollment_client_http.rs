//! Phase 1.7 — `EnrollmentClient` HTTP body acceptance.
//!
//! Verifies the wire shape matches the cloud-side `EnrollRequest` /
//! `EnrollResponse` in
//! `nexus-cloud-console/services/enrollment-svc/src/handlers.rs`
//! exactly, and that the client surfaces non-2xx responses through
//! [`EnrollmentError::BadStatus`].

use nexus_cloud_client::enrollment::{EnrollmentClient, EnrollmentError, EnrollmentRequest};
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn enroll_roundtrips_against_canonical_cloud_shape() {
    let server = MockServer::start().await;

    // Cloud-side response shape — verbatim from
    // `services/enrollment-svc/src/handlers.rs::EnrollResponse`.
    let response = json!({
        "cert_pem": "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----\n",
        "ca_chain_pem": "-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----\n",
        "entitlement_jwt": "header.payload.sig",
        "gateway_url": "wss://gateway.example.com/v1/tunnel",
        "core_id": "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c0",
    });

    // The request body must be exactly `{code, csr_pem, fingerprint}` —
    // no extra fields. wiremock's `body_json` does deep equality so an
    // accidental `tier: "t36"` (the pre-Phase-1.7 shape) would fail.
    let request_body = json!({
        "code": "XJ4K-PMQ7-9NAB",
        "csr_pem": "-----BEGIN CERTIFICATE REQUEST-----\ncsr\n-----END CERTIFICATE REQUEST-----\n",
        "fingerprint": "tpm:0xdeadbeef",
    });

    Mock::given(method("POST"))
        .and(path("/v1/enroll"))
        .and(body_json(&request_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response))
        .expect(1)
        .mount(&server)
        .await;

    let client = EnrollmentClient::new(server.uri());
    let req = EnrollmentRequest {
        code: "XJ4K-PMQ7-9NAB".to_string(),
        csr_pem: "-----BEGIN CERTIFICATE REQUEST-----\ncsr\n-----END CERTIFICATE REQUEST-----\n"
            .to_string(),
        fingerprint: "tpm:0xdeadbeef".to_string(),
        server_csr_pem: None,
        server_dns_sans: Vec::new(),
        server_ip_sans: Vec::new(),
    };

    let resp = client.enroll(&req).await.expect("enrollment succeeds");

    assert_eq!(resp.core_id, "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c0");
    assert_eq!(resp.gateway_url, "wss://gateway.example.com/v1/tunnel");
    assert_eq!(resp.entitlement_jwt, "header.payload.sig");
    assert!(resp.cert_pem.contains("leaf"));
    assert!(resp.ca_chain_pem.contains("ca"));
    // Forward-compat field — cloud does not bundle it today.
    assert!(resp.entitlement_signing_key_pem.is_none());
    assert!(resp.entitlement_signing_kid.is_none());
}

#[tokio::test]
async fn enroll_surfaces_server_error_payload() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/enroll"))
        .respond_with(
            ResponseTemplate::new(409).set_body_json(json!({"error": "code_already_redeemed"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = EnrollmentClient::new(server.uri());
    let err = client
        .enroll(&EnrollmentRequest {
            code: "USED".into(),
            csr_pem: "csr".into(),
            fingerprint: "fp".into(),
            server_csr_pem: None,
            server_dns_sans: Vec::new(),
            server_ip_sans: Vec::new(),
        })
        .await
        .expect_err("non-2xx surfaces");

    match err {
        EnrollmentError::BadStatus { status, body } => {
            assert_eq!(status, 409);
            assert!(body.contains("code_already_redeemed"));
        }
        other => panic!("expected BadStatus, got {other:?}"),
    }
}

#[tokio::test]
async fn enroll_handles_forward_compat_actor_token_key_bundle() {
    // The moment the cloud-side enrollment-svc starts bundling the
    // entitlement signing key into its response, the engine MUST
    // surface it. This test pre-validates the deserialise path so
    // adding the field on the cloud side is a one-PR change.
    let server = MockServer::start().await;

    let response = json!({
        "cert_pem": "leaf",
        "ca_chain_pem": "ca",
        "entitlement_jwt": "jwt",
        "gateway_url": "wss://g",
        "core_id": "0190f7be-7c6a-7d4f-8f01-d9b1f0c0c0c0",
        "entitlement_signing_key_pem":
            "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA...\n-----END PUBLIC KEY-----\n",
        "entitlement_signing_kid": "ent-2026-01"
    });

    Mock::given(method("POST"))
        .and(path("/v1/enroll"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response))
        .mount(&server)
        .await;

    let client = EnrollmentClient::new(server.uri());
    let resp = client
        .enroll(&EnrollmentRequest {
            code: "X".into(),
            csr_pem: "Y".into(),
            fingerprint: "Z".into(),
            server_csr_pem: None,
            server_dns_sans: Vec::new(),
            server_ip_sans: Vec::new(),
        })
        .await
        .expect("enrollment succeeds");

    assert_eq!(resp.entitlement_signing_kid.as_deref(), Some("ent-2026-01"));
    assert!(resp
        .entitlement_signing_key_pem
        .as_deref()
        .map(|s| s.contains("BEGIN PUBLIC KEY"))
        .unwrap_or(false));
}
