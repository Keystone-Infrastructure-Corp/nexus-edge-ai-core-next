//! M6 — local user authentication primitives.
//!
//! Pure, self-contained sub-modules consumed by the eventual
//! `auth::login` / `auth::sessions` handlers:
//!
//! * [`passwords`] — argon2id hashing + password policy
//!   (Phase 2 Step 2.2).
//! * [`lockout`] — failed-login lockout FSM
//!   (Phase 2 Step 2.3).
//!
//! Future siblings (planned in [`docs/M6_IDENTITY.md`](../../../docs/M6_IDENTITY.md)):
//!
//! * `sessions` — HS256 JWT issue/verify + refresh-token
//!   rotation (Phase 2 Step 2.4).
//! * `require_role` — axum extractor that pulls the session
//!   off the request, decodes it, and asserts a minimum role
//!   (Phase 2 Step 2.5).
//!
//! Keeping each concern as a tiny leaf module under `auth/`
//! lets the login handler in `api.rs` compose them without
//! pulling in a god-module.

// Until the login handler in `api.rs` (Phase 2 Step 2.7) and
// the admin user-CRUD handlers (Step 2.8) consume these
// functions, every public item below is unreferenced in the
// engine binary. Tests inside each module exercise the API end
// to end; we just need to silence dead_code until the wiring
// catches up. Drop these allows the moment 2.7 lands.
#[allow(dead_code)]
pub mod passwords;
#[allow(dead_code)]
pub mod lockout;
