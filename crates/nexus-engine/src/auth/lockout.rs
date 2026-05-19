//! M6 Phase 2 Step 2.3 — login-failure lockout state machine.
//!
//! Pure, allocation-free, deterministic. Given the current
//! wall-clock time, a user's current lockout state from the
//! `users` row, and the operator-tunable [`LockoutConfig`], it
//! answers exactly two questions:
//!
//! 1. **[`evaluate`]** — "Is this user locked *right now*?"
//!    Used by the login handler BEFORE attempting password
//!    verification so a locked user never causes the ~100 ms
//!    argon2 verify (which would otherwise be a timing
//!    side-channel for whether the lockout was honoured).
//!
//! 2. **[`evaluate_failure`]** — "This attempt just failed —
//!    what should I write to `users.failed_login_count` and
//!    `users.locked_until`?" Returns a [`FailureOutcome`] the
//!    login handler hands straight to
//!    `Store::record_login_failure`.
//!
//! ## Policy, in plain English
//!
//! * Defaults match the M6 spec: 5 failed attempts within a
//!   15-minute rolling window trip a 15-minute lockout. All
//!   three numbers come from [`LockoutConfig`] and are
//!   operator-tunable per deployment.
//! * The window is "rolling" in the sense that an attempt
//!   older than `window_secs` is effectively forgotten — the
//!   next failure restarts the counter at 1, not at
//!   `failed_login_count + 1`. This matches OWASP's
//!   "Authentication Cheat Sheet" recommendation against
//!   permanent counters that grind through false-positives.
//! * The lockout is *time-bound*, not click-to-unlock. The
//!   user (or anyone holding their session) regains access
//!   automatically at `locked_until`. Admin can also clear it
//!   early via `POST /api/v1/admin/users/:id/unlock`
//!   (Phase 2 Step 2.8).
//! * Successful login during the lockout is impossible — the
//!   login handler returns the same 401 invalid_credentials
//!   it would for a wrong password, deliberately ambiguous so
//!   the caller cannot enumerate which users are locked.
//!
//! ## What this module does NOT do
//!
//! * **Decide whether to log to `audit_log`.** That's the
//!   login handler's job. The FSM surfaces
//!   [`FailureOutcome::lockout_tripped`] so the handler can
//!   write a `lockout.tripped` audit row (alongside the
//!   standard `login.failure` row) on the attempt that flips
//!   the user into the locked state.
//! * **Compute IP-level or system-wide rate limiting.** That's
//!   a separate cross-cutting concern; the FSM is per-user.
//! * **Touch the database.** All state lives on the [`User`]
//!   row the caller supplies.

use chrono::{DateTime, Duration, Utc};
use nexus_config::LockoutConfig;
use nexus_store::User;

/// Outcome of [`evaluate`] — the current lockout state for a user
/// at a given `now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockState {
    /// The user is not currently locked and a login attempt
    /// should proceed to password verification.
    Unlocked,
    /// The user is locked until `until`. Reject the login
    /// attempt with the same 401 shape as a bad password
    /// (deliberately ambiguous). `until` is exposed so the
    /// admin UI can render a countdown — it's NEVER sent to
    /// the failed-login response body.
    Locked { until: DateTime<Utc> },
}

/// Outcome of [`evaluate_failure`] — exactly what
/// `Store::record_login_failure(id, new_count, lock_until)`
/// should be called with after a failed attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureOutcome {
    /// What to write to `users.failed_login_count`. Either
    /// `previous_count + 1` (window still open) or `1` (window
    /// expired, this is the first failure of a fresh window).
    pub new_count: i64,
    /// What to write to `users.locked_until`. `Some(t)` exactly
    /// on the attempt that trips the lockout; `None` on every
    /// other failed attempt.
    pub lock_until: Option<DateTime<Utc>>,
    /// `true` iff `lock_until.is_some()`. Convenience for the
    /// login handler's audit-row branch.
    pub lockout_tripped: bool,
}

/// Compute the user's current lock state at `now`. Returns
/// `Unlocked` if `locked_until` is null OR `<= now`.
///
/// Note the `<=`, not `<` — at exactly `t == locked_until` we
/// consider the user unlocked. This avoids a one-tick window
/// where a user who polled at the wrong millisecond would see
/// "still locked" for the same instant another caller would
/// see "now free".
pub fn evaluate(now: DateTime<Utc>, user: &User) -> LockState {
    match user.locked_until {
        Some(t) if t > now => LockState::Locked { until: t },
        _ => LockState::Unlocked,
    }
}

/// Given the user's state BEFORE this failed attempt and the
/// policy `cfg`, compute the counter + lock_until to persist.
///
/// Pure: no I/O, no allocations, no clock reads. The caller
/// supplies `now`.
///
/// Logic:
///
/// 1. Look at `user.last_failed_login_at`:
///    * If it's `None` (first ever failure) OR it's older than
///      `cfg.window_secs`, this attempt starts a fresh window
///      with `new_count = 1`.
///    * Otherwise the window is still open and
///      `new_count = user.failed_login_count + 1`.
///
/// 2. If `new_count >= cfg.max_attempts`, this attempt trips
///    the lockout: `lock_until = Some(now + cfg.lockout_secs)`.
///    Otherwise `lock_until = None`.
///
/// The fresh-window reset is what makes the policy a "5 fails
/// in 15 min" rolling window rather than an absolute "5 fails
/// total ever". Without the reset, four fails six months ago
/// would tip into a lockout after a single typo today.
pub fn evaluate_failure(
    now: DateTime<Utc>,
    user: &User,
    cfg: &LockoutConfig,
) -> FailureOutcome {
    let window = Duration::seconds(cfg.window_secs as i64);
    let window_open = user
        .last_failed_login_at
        .map(|t| now.signed_duration_since(t) <= window)
        .unwrap_or(false);
    let new_count = if window_open {
        user.failed_login_count.saturating_add(1)
    } else {
        1
    };
    let trips_lock = new_count >= cfg.max_attempts as i64;
    let lock_until = if trips_lock {
        Some(now + Duration::seconds(cfg.lockout_secs as i64))
    } else {
        None
    };
    FailureOutcome {
        new_count,
        lock_until,
        lockout_tripped: trips_lock,
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use nexus_store::User;
    use nexus_types::Role;

    /// Default LockoutConfig matches the M6 spec
    /// (5 fails in 15 min → 15 min lock). All tests use this
    /// unless they're explicitly varying it.
    fn cfg() -> LockoutConfig {
        LockoutConfig::default()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 19, 12, 0, 0).unwrap()
    }

    /// Build a baseline User struct that the tests can mutate.
    /// Everything other than the lockout fields is fixed.
    fn user_with(
        failed_login_count: i64,
        last_failed_login_at: Option<DateTime<Utc>>,
        locked_until: Option<DateTime<Utc>>,
    ) -> User {
        User {
            id: 42,
            username: "alice".into(),
            role: Role::Viewer,
            force_password_reset: false,
            disabled: false,
            failed_login_count,
            locked_until,
            last_login_at: None,
            last_failed_login_at,
            created_at: now(),
            updated_at: now(),
            deleted_at: None,
            has_oidc: false,
            has_password: true,
        }
    }

    #[test]
    fn evaluate_returns_unlocked_when_locked_until_is_none() {
        assert_eq!(evaluate(now(), &user_with(0, None, None)), LockState::Unlocked);
    }

    #[test]
    fn evaluate_returns_unlocked_when_locked_until_is_in_the_past() {
        let past = now() - Duration::seconds(1);
        assert_eq!(evaluate(now(), &user_with(5, None, Some(past))), LockState::Unlocked);
    }

    #[test]
    fn evaluate_returns_unlocked_when_locked_until_equals_now() {
        // Boundary: `>` not `>=`. Equality means just-expired.
        let t = now();
        assert_eq!(evaluate(t, &user_with(5, None, Some(t))), LockState::Unlocked);
    }

    #[test]
    fn evaluate_returns_locked_when_locked_until_is_in_the_future() {
        let future = now() + Duration::minutes(10);
        assert_eq!(
            evaluate(now(), &user_with(5, None, Some(future))),
            LockState::Locked { until: future },
        );
    }

    #[test]
    fn first_ever_failure_starts_window_at_one() {
        let user = user_with(0, None, None);
        let out = evaluate_failure(now(), &user, &cfg());
        assert_eq!(out.new_count, 1);
        assert_eq!(out.lock_until, None);
        assert!(!out.lockout_tripped);
    }

    #[test]
    fn failure_inside_window_increments_counter() {
        let recent = now() - Duration::minutes(5);
        let user = user_with(2, Some(recent), None);
        let out = evaluate_failure(now(), &user, &cfg());
        assert_eq!(out.new_count, 3, "2 + 1 = 3");
        assert_eq!(out.lock_until, None);
        assert!(!out.lockout_tripped);
    }

    #[test]
    fn failure_after_window_expiry_resets_counter_to_one() {
        // Window default is 900 s (15 min); previous failure
        // 16 min ago.
        let stale = now() - Duration::minutes(16);
        let user = user_with(4, Some(stale), None);
        let out = evaluate_failure(now(), &user, &cfg());
        assert_eq!(out.new_count, 1, "window expired, fresh start");
        assert_eq!(out.lock_until, None);
        assert!(!out.lockout_tripped);
    }

    #[test]
    fn failure_at_exact_window_boundary_keeps_window_open() {
        // Boundary check: `now - last == window` keeps the
        // window open (`<=`, not `<`). This avoids a one-second
        // hole where a fast attacker could pump max_attempts
        // failures exactly window_secs apart and never trip
        // the lock.
        let cfg = cfg();
        let boundary = now() - Duration::seconds(cfg.window_secs as i64);
        let user = user_with(2, Some(boundary), None);
        let out = evaluate_failure(now(), &user, &cfg);
        assert_eq!(out.new_count, 3, "boundary still inside window");
    }

    #[test]
    fn fifth_failure_inside_window_trips_the_lockout() {
        let recent = now() - Duration::minutes(5);
        let user = user_with(4, Some(recent), None);
        let out = evaluate_failure(now(), &user, &cfg());
        assert_eq!(out.new_count, 5);
        assert!(out.lockout_tripped);
        // 15-minute lockout per the default.
        assert_eq!(out.lock_until, Some(now() + Duration::minutes(15)));
    }

    #[test]
    fn lockout_uses_cfg_lockout_secs_not_window_secs() {
        // Vary lockout_secs independently from window_secs to
        // catch a swap.
        let cfg = LockoutConfig {
            max_attempts: 3,
            window_secs: 60,
            lockout_secs: 7200, // 2 hours
        };
        let recent = now() - Duration::seconds(10);
        let user = user_with(2, Some(recent), None);
        let out = evaluate_failure(now(), &user, &cfg);
        assert!(out.lockout_tripped);
        assert_eq!(out.lock_until, Some(now() + Duration::hours(2)));
    }

    #[test]
    fn failures_beyond_max_attempts_keep_tripping_with_fresh_lock_until() {
        // If the user keeps trying after the lock trips (which
        // the login handler shouldn't let them do — `evaluate`
        // returns Locked first — but defence in depth), each
        // additional failure refreshes lock_until. This is
        // intentional: an attacker can't drain the lock by
        // hammering during it.
        let recent = now() - Duration::minutes(1);
        let user = user_with(5, Some(recent), Some(now() + Duration::minutes(10)));
        let out = evaluate_failure(now(), &user, &cfg());
        assert_eq!(out.new_count, 6);
        assert!(out.lockout_tripped);
        assert_eq!(out.lock_until, Some(now() + Duration::minutes(15)));
    }

    #[test]
    fn evaluate_failure_does_not_mutate_user() {
        // Smoke-test that the function is pure: pass a user,
        // get an outcome, original user is byte-identical.
        let recent = now() - Duration::minutes(5);
        let user = user_with(2, Some(recent), None);
        let user_before = user.clone();
        let _ = evaluate_failure(now(), &user, &cfg());
        assert_eq!(user, user_before);
    }

    #[test]
    fn evaluate_does_not_mutate_user() {
        let user = user_with(3, None, Some(now() + Duration::minutes(5)));
        let user_before = user.clone();
        let _ = evaluate(now(), &user);
        assert_eq!(user, user_before);
    }

    #[test]
    fn saturating_add_protects_against_pathological_counter() {
        // Belt-and-suspenders: a corrupt DB row with
        // failed_login_count = i64::MAX should not panic.
        // Production should never see this; the FSM still
        // hands back a sane outcome.
        let recent = now() - Duration::minutes(1);
        let user = user_with(i64::MAX, Some(recent), None);
        let out = evaluate_failure(now(), &user, &cfg());
        assert_eq!(out.new_count, i64::MAX, "saturated, not panicked");
        assert!(out.lockout_tripped, "MAX is >= 5, so locked");
    }
}
