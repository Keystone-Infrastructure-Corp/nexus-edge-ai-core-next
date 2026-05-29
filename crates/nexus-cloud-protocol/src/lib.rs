//! # nexus-cloud-protocol (edge side)
//!
//! Typed Rust view of the cloud-side wire schema
//! (`nexus-cloud-console/proto/v1.json`) — the WebSocket envelope and
//! every message kind the engine speaks (see the cloud-console
//! `docs/WIRE_PROTOCOL.md` §4).
//!
//! ## Repo boundary
//!
//! This crate is the edge mirror of the cloud-console crate of the same
//! name. The edge does NOT vendor the JSON schema itself; it carries
//! only the *generated* Rust bindings at
//! [`src/v1.rs`](src/v1.rs), kept byte-identical to the cloud's
//! generated source via the cloud-console `cargo xtask
//! sync-cloud-protocol --core <path>` command, which also writes a
//! companion `v1.CHECKSUM` file that CI verifies on every build.
//! Per REPO_BOUNDARY R1, neither repo imports a `nexus-*` crate from
//! the other — both consume the schema independently.

#![forbid(unsafe_code)]

/// Wire-protocol version 1. The cloud-console `cargo xtask
/// sync-cloud-protocol --core <path>` writes this file from the
/// canonical `proto/v1.json` in that repo, alongside the companion
/// `v1.CHECKSUM` (SHA-256 of the source schema at the time of last
/// sync).
pub mod v1 {
    #![allow(clippy::pub_underscore_fields)]
    #![allow(clippy::struct_excessive_bools)]
    #![allow(clippy::large_enum_variant)]
    #![allow(clippy::doc_markdown)]
    #![allow(clippy::derive_partial_eq_without_eq)]
    #![allow(missing_docs)]

    include!("v1.rs");
}
