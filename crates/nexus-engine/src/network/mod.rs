//! M-Admin Network — physical NIC enumeration + OS-level
//! network configuration writer.
//!
//! Two responsibilities, deliberately split into submodules so
//! the read-only "Phase A" surface stays decoupled from the
//! privileged "Phase B" mutation path:
//!
//! 1. [`enumerate`] — pure read, cross-platform via `if-addrs`
//!    augmented with `/sys/class/net/*` reads on Linux. Backs
//!    `GET /v1/admin/network/interfaces` and the NIC-aware
//!    `host:port` dropdowns in the Server Settings page. No
//!    elevation needed.
//!
//! 2. [`plan`] + [`apply`] — netplan YAML round-trip (Ubuntu
//!    24.04 target only) plus the small, narrowly-scoped
//!    helper-binary protocol the engine uses to actually mutate
//!    `/etc/netplan/90-nexus.yaml` and run `netplan try`. The
//!    engine runs as `nexus_admin` (no CAP_NET_ADMIN) and
//!    shells out via `sudo -n /usr/local/lib/nexus/nexus-netd
//!    <apply|confirm|rollback>` — the sudoers entry is the
//!    privilege boundary, not the engine binary itself.
//!
//! ## Why this split
//!
//! Phase A is something every install can do today, including
//! the macOS dev box. Phase B has a hard privilege requirement
//! and only runs on the K13 Ubuntu 24.04 production tiers. By
//! gating the mutation path behind a single OS check + sudo
//! shell-out we keep the engine binary itself unprivileged and
//! portable; ops can audit exactly one helper binary and one
//! sudoers entry instead of grep-ing every code path.

pub mod apply;
pub mod enumerate;
pub mod plan;
pub mod roles;

pub use enumerate::{list_interfaces, NetworkInterface};
pub use plan::NetplanPlan;
pub use roles::camera_bind_ipv4;
