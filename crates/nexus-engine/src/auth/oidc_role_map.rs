//! M6 Phase 3 Step 3.2 — OIDC role mapping.
//!
//! Pure function that turns a verified [`IdTokenClaims`] +
//! [`OidcConfig`] into a Nexus [`Role`]. Lives in its own
//! sub-module so the Step 3.3 auth-code handler can call it
//! without dragging discovery/JWKS plumbing into scope, and so
//! the role-mapping policy stays unit-testable in isolation.
//!
//! Algorithm — deliberately conservative:
//!
//! 1. Walk `cfg.role_claims` in order. Each entry is a JSON
//!    pointer-style key into [`IdTokenClaims::extra`]. The
//!    first key that resolves to a non-null value wins;
//!    later keys are ignored even if also present.
//!    *Rationale:* the operator configured this order on
//!    purpose — surprising them by stitching results across
//!    multiple claims is worse than honouring the first hit.
//!
//! 2. Collect the candidate values. We accept:
//!      * a single string  (`"groups": "nexus-admins"`),
//!      * an array of strings (`"groups": ["a", "b"]`).
//!
//!    Any other shape (object, number, mixed array) is
//!    discarded — IdPs that ship structured group payloads
//!    must flatten before signing, by configuration on their
//!    side.
//!
//! 3. Check the candidate values against
//!    `cfg.role_map.admin`, then `.operator`, then `.viewer`.
//!    Highest-privilege match wins. Equality only — regex /
//!    glob is deferred until an operator actually asks; every
//!    real-world IdP we expect (Authentik, Entra, Auth0,
//!    Keycloak) emits literal group names.
//!
//! 4. If no value matches: return [`Role::Viewer`] unless
//!    `cfg.deny_unmapped` is set, in which case return
//!    [`MapError::Unmapped`]. The Step 3.3 handler renders
//!    the latter as `403 {"error":"unmapped_role"}`.
//!
//! Edge cases verified by the test suite below:
//!
//! * Empty role_map AND `deny_unmapped = false` → every
//!   authenticated user lands as viewer (read-only). This is
//!   the safe default for a fresh OIDC install — operators
//!   add admin/operator mappings deliberately.
//! * Empty role_map AND `deny_unmapped = true` → every user
//!   is rejected. This is the lock-down posture for the
//!   commissioning window (no one can log in until you wire
//!   the groups).
//! * A user with both an admin-mapped group AND an
//!   operator-mapped group wins admin. Tested explicitly.
//! * A user with only viewer-mapped groups + admin in a
//!   *later* role_claims entry that isn't checked because
//!   the first claim resolved → wins viewer, not admin. This
//!   is the "first claim wins" pin that prevents claim
//!   stitching from silently elevating privilege.

use std::collections::HashSet;

use nexus_config::OidcConfig;
use nexus_types::Role;
use serde_json::Value;

use super::oidc::IdTokenClaims;

/// Failure modes for [`map_role`]. Stable, machine-friendly
/// tags — the Step 3.3 handler will map these straight to HTTP
/// response bodies without further translation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MapError {
    /// Claims did not match any role_map entry AND
    /// `cfg.deny_unmapped` is true.
    #[error("unmapped_role: no configured role_map entry matched any value in the resolved claim")]
    Unmapped,
}

/// Map a verified ID token to a Nexus [`Role`] per the policy
/// described in the module-level docstring.
pub fn map_role(claims: &IdTokenClaims, cfg: &OidcConfig) -> Result<Role, MapError> {
    let values = resolve_first_claim(claims, &cfg.role_claims);

    // Build a quick membership set so the role lookups are
    // O(values * map_size_per_role) rather than O(values^2).
    let values_set: HashSet<&str> = values.iter().map(String::as_str).collect();

    if has_match(&cfg.role_map.admin, &values_set) {
        return Ok(Role::Admin);
    }
    if has_match(&cfg.role_map.operator, &values_set) {
        return Ok(Role::Operator);
    }
    if has_match(&cfg.role_map.viewer, &values_set) {
        return Ok(Role::Viewer);
    }

    if cfg.deny_unmapped {
        Err(MapError::Unmapped)
    } else {
        Ok(Role::Viewer)
    }
}

/// Walk `claim_paths` in order; return the value list from the
/// first key that resolves to a string or array-of-strings.
/// Empty list = no claim resolved (or it resolved to an
/// unsupported shape).
fn resolve_first_claim(claims: &IdTokenClaims, claim_paths: &[String]) -> Vec<String> {
    for path in claim_paths {
        let Some(v) = claims.extra.get(path) else {
            continue;
        };
        match v {
            Value::String(s) => return vec![s.clone()],
            Value::Array(items) => {
                // Only accept arrays of strings. A mixed array
                // (string + number, etc.) is discarded entirely
                // — silently picking the strings out would let
                // a poorly-configured IdP elevate a user by
                // accident.
                let all_strings = items.iter().all(|i| i.is_string());
                if !all_strings || items.is_empty() {
                    continue;
                }
                return items
                    .iter()
                    .filter_map(|i| i.as_str().map(str::to_string))
                    .collect();
            }
            // Numbers, booleans, objects, null → not a role
            // claim shape we accept. Try the next key.
            _ => continue,
        }
    }
    Vec::new()
}

/// True iff any configured `allow` value is present in the
/// user's claim values. Equality match, case-sensitive — every
/// real IdP normalises group names to a stable casing, and
/// case-folding here would mask config typos.
fn has_match(allow: &[String], values: &HashSet<&str>) -> bool {
    allow.iter().any(|a| values.contains(a.as_str()))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::OidcRoleMap;
    use serde_json::json;
    use std::collections::HashMap;

    /// Build a claim set with the given `extra` payload. The
    /// iss/aud/sub/exp values are fillers — `map_role` never
    /// looks at them.
    fn claims_with_extra(extra: HashMap<String, Value>) -> IdTokenClaims {
        IdTokenClaims {
            iss: "https://idp.test/".into(),
            aud: json!("nexus-client-id"),
            sub: "user-abc".into(),
            exp: 9_999_999_999,
            nbf: None,
            iat: None,
            extra,
        }
    }

    fn default_cfg(role_map: OidcRoleMap, deny_unmapped: bool) -> OidcConfig {
        OidcConfig {
            issuer: "https://idp.test/".into(),
            audience: "nexus-client-id".into(),
            jwks_uri: None,
            client_id: None,
            display_name: None,
            scopes: vec!["openid".into(), "profile".into(), "groups".into()],
            // Default lookup order matches the production
            // default in `default_oidc_role_claims`.
            role_claims: vec![
                "groups".to_string(),
                "roles".to_string(),
                "https://nexus.local/role".to_string(),
            ],
            role_map,
            deny_unmapped,
            redirect_uri: None,
            client_secret_file: None,
            client_secret_env: None,
        }
    }

    fn role_map(admin: &[&str], operator: &[&str], viewer: &[&str]) -> OidcRoleMap {
        OidcRoleMap {
            admin: admin.iter().map(|s| s.to_string()).collect(),
            operator: operator.iter().map(|s| s.to_string()).collect(),
            viewer: viewer.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn extra_groups_array(groups: &[&str]) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert(
            "groups".to_string(),
            Value::Array(groups.iter().map(|g| json!(g)).collect()),
        );
        m
    }

    // ---- first-claim-wins / lookup order --------------------------------

    #[test]
    fn groups_claim_array_maps_to_admin() {
        let cfg = default_cfg(role_map(&["nexus-admins"], &[], &[]), false);
        let claims = claims_with_extra(extra_groups_array(&["nexus-admins", "everyone"]));
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Admin);
    }

    #[test]
    fn groups_claim_single_string_maps_to_operator() {
        let cfg = default_cfg(role_map(&[], &["nexus-operators"], &[]), false);
        let mut extra = HashMap::new();
        extra.insert("groups".to_string(), json!("nexus-operators"));
        let claims = claims_with_extra(extra);
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Operator);
    }

    #[test]
    fn first_claim_wins_even_if_later_claim_would_promote() {
        // groups → matches viewer; roles → would match admin.
        // First claim wins: result is viewer, NOT admin. This
        // is the policy pin that prevents claim stitching.
        let cfg = default_cfg(role_map(&["nexus-admins"], &[], &["read-only"]), false);
        let mut extra = HashMap::new();
        extra.insert("groups".to_string(), json!(["read-only"]));
        extra.insert("roles".to_string(), json!(["nexus-admins"]));
        let claims = claims_with_extra(extra);
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Viewer);
    }

    #[test]
    fn falls_through_to_second_claim_when_first_is_absent() {
        let cfg = default_cfg(role_map(&["nexus-admins"], &[], &[]), false);
        let mut extra = HashMap::new();
        extra.insert("roles".to_string(), json!(["nexus-admins"]));
        let claims = claims_with_extra(extra);
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Admin);
    }

    #[test]
    fn falls_through_to_custom_url_claim() {
        let cfg = default_cfg(role_map(&[], &["ops"], &[]), false);
        let mut extra = HashMap::new();
        extra.insert("https://nexus.local/role".to_string(), json!("ops"));
        let claims = claims_with_extra(extra);
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Operator);
    }

    // ---- privilege precedence -------------------------------------------

    #[test]
    fn admin_wins_over_operator_when_user_is_in_both() {
        let cfg = default_cfg(
            role_map(&["nexus-admins"], &["nexus-operators"], &[]),
            false,
        );
        let claims = claims_with_extra(extra_groups_array(&["nexus-operators", "nexus-admins"]));
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Admin);
    }

    #[test]
    fn operator_wins_over_viewer_when_user_is_in_both() {
        let cfg = default_cfg(role_map(&[], &["ops"], &["readers"]), false);
        let claims = claims_with_extra(extra_groups_array(&["readers", "ops"]));
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Operator);
    }

    // ---- unmapped fallback ----------------------------------------------

    #[test]
    fn unmapped_falls_to_viewer_when_deny_unmapped_false() {
        let cfg = default_cfg(role_map(&["nexus-admins"], &[], &[]), false);
        let claims = claims_with_extra(extra_groups_array(&["random-group"]));
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Viewer);
    }

    #[test]
    fn unmapped_rejects_when_deny_unmapped_true() {
        let cfg = default_cfg(role_map(&["nexus-admins"], &[], &[]), true);
        let claims = claims_with_extra(extra_groups_array(&["random-group"]));
        assert_eq!(map_role(&claims, &cfg), Err(MapError::Unmapped));
    }

    #[test]
    fn no_role_claim_present_falls_to_viewer() {
        let cfg = default_cfg(role_map(&["nexus-admins"], &[], &[]), false);
        // None of the configured role_claims are present in
        // `extra`. With deny_unmapped=false the user becomes a
        // viewer.
        let claims = claims_with_extra(HashMap::new());
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Viewer);
    }

    #[test]
    fn no_role_claim_present_rejects_when_deny_unmapped_true() {
        let cfg = default_cfg(role_map(&["nexus-admins"], &[], &[]), true);
        let claims = claims_with_extra(HashMap::new());
        assert_eq!(map_role(&claims, &cfg), Err(MapError::Unmapped));
    }

    #[test]
    fn empty_role_map_with_deny_unmapped_false_admits_as_viewer() {
        // Fresh OIDC install — admin hasn't wired any groups
        // yet. Posture: anyone the IdP signs in lands as
        // read-only.
        let cfg = default_cfg(OidcRoleMap::default(), false);
        let claims = claims_with_extra(extra_groups_array(&["literally-anything"]));
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Viewer);
    }

    #[test]
    fn empty_role_map_with_deny_unmapped_true_locks_everyone_out() {
        // Commissioning window — block all logins until an
        // operator wires the groups.
        let cfg = default_cfg(OidcRoleMap::default(), true);
        let claims = claims_with_extra(extra_groups_array(&["nexus-admins"]));
        assert_eq!(map_role(&claims, &cfg), Err(MapError::Unmapped));
    }

    // ---- claim-shape filtering ------------------------------------------

    #[test]
    fn empty_array_in_first_claim_falls_through_to_second() {
        let cfg = default_cfg(role_map(&["nexus-admins"], &[], &[]), false);
        let mut extra = HashMap::new();
        // groups is present but empty — that's not a role
        // payload. Walk to `roles`.
        extra.insert("groups".to_string(), json!([]));
        extra.insert("roles".to_string(), json!(["nexus-admins"]));
        let claims = claims_with_extra(extra);
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Admin);
    }

    #[test]
    fn mixed_type_array_is_rejected_and_falls_through() {
        let cfg = default_cfg(role_map(&["nexus-admins"], &[], &[]), false);
        let mut extra = HashMap::new();
        // Mixed string + number. We do NOT pick the strings
        // out — that would let a typo'd IdP config silently
        // elevate.
        extra.insert("groups".to_string(), json!(["nexus-admins", 42]));
        extra.insert("roles".to_string(), json!(["nexus-admins"]));
        let claims = claims_with_extra(extra);
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Admin);
    }

    #[test]
    fn object_in_role_claim_is_rejected_and_falls_through() {
        let cfg = default_cfg(role_map(&[], &["ops"], &[]), false);
        let mut extra = HashMap::new();
        extra.insert("groups".to_string(), json!({"nested": "object"}));
        extra.insert("roles".to_string(), json!(["ops"]));
        let claims = claims_with_extra(extra);
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Operator);
    }

    #[test]
    fn null_in_role_claim_is_rejected_and_falls_through() {
        let cfg = default_cfg(role_map(&["nexus-admins"], &[], &[]), false);
        let mut extra = HashMap::new();
        extra.insert("groups".to_string(), Value::Null);
        extra.insert("roles".to_string(), json!(["nexus-admins"]));
        let claims = claims_with_extra(extra);
        assert_eq!(map_role(&claims, &cfg).unwrap(), Role::Admin);
    }

    #[test]
    fn match_is_case_sensitive() {
        // "NEXUS-ADMINS" is NOT the same as "nexus-admins" —
        // case-folding would mask config typos.
        let cfg = default_cfg(role_map(&["nexus-admins"], &[], &[]), true);
        let claims = claims_with_extra(extra_groups_array(&["NEXUS-ADMINS"]));
        assert_eq!(map_role(&claims, &cfg), Err(MapError::Unmapped));
    }
}
