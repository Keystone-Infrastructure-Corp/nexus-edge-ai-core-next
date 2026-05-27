//! # nexus-cloud-client
//!
//! Edge-side client for the cloud-console. Phase 1.7 lands the core surface:
//!
//! * [`actor_token::Verifier`] — Ed25519 JWT verifier with claim checks +
//!   ±30 s clock-skew tolerance per `docs/ARCHITECTURE.md §3.7` (cloud
//!   repo).
//! * [`jti_cache::JtiReplayCache`] — bounded (~10 000-entry) replay-
//!   protection cache keyed by `jti`. Per Phase 1.16 it MAY later be
//!   widened to `(jti, request_id)` once the engine surfaces `request_id`.
//! * [`dispatcher::RpcDispatcher`] — wraps a [`dispatcher::Handler`] trait
//!   impl, verifies the `actor_token` on every state-mutating `rpc_call`,
//!   and consults a `system:`-sub method whitelist before dispatch.
//! * [`enrollment::EnrollmentClient`] — POSTs the enrollment CSR to
//!   `/v1/cores/enroll`; Phase 1.7 ships the shape, the wire is wired in
//!   Phase 1.11 (cloud enrollment HTTP).
//! * [`tunnel::TunnelClient`] — WSS client over `wss://gateway/v1/tunnel`;
//!   Phase 1.7 ships the type contract, the body lands in Phase 1.11.
//! * [`entitlements::EntitlementCache`] — persists the most recent
//!   `entitlement_update` JWT so the engine can apply quota at startup
//!   even before the first heartbeat round-trip.
//! * [`sink::CloudConsoleSink`] — Phase 1.7 entry point; the engine routes
//!   alerts through here once Phase 1.11 wires the tunnel.
//!
//! ## Repo boundary
//!
//! Per `nexus-cloud-console/docs/REPO_BOUNDARY.md` R1 this crate MUST NOT
//! import any service from the cloud-console repo. The only contract is
//! the wire envelope in [`nexus_cloud_protocol::v1`], which this repo
//! vendors as a byte-identical mirror of `proto/v1.json`.

#![forbid(unsafe_code)]

pub mod actor_token;
pub mod csr;
pub mod dispatcher;
pub mod enrollment;
pub mod entitlements;
pub mod error;
pub mod jti_cache;
pub mod response_cache;
pub mod sink;
pub mod tunnel;

pub use actor_token::{EnvelopeContext, TrustedKey, VerifiedActor, Verifier, VerifierBuilder};
pub use csr::{generate_keypair_and_csr, CsrBundle, CsrError};
pub use dispatcher::{AuditSink, Handler, NullAuditSink, RpcDispatcher, SystemMethodPolicy};
pub use enrollment::{EnrollmentClient, EnrollmentError, EnrollmentRequest, EnrollmentResponse};
pub use error::{DispatchError, InvalidReason, RejectReason};
pub use jti_cache::JtiReplayCache;
pub use response_cache::RpcResponseCache;
pub use tunnel::{TunnelClient, TunnelError, TunnelHandle};
