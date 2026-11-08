//! Rules

use nexus_types::CameraId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulesConfig {
    #[serde(default)]
    pub backend: RulesBackendKind,
    /// Inline rules from TOML — useful for smoke tests; production rules live in the DB.
    #[serde(default)]
    pub inline: Vec<RuleConfig>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RulesBackendKind {
    #[default]
    Cel,
}

/// The CEL predicate plus its severity tag — i.e. "what is this
/// rule actually checking, and how loudly does it alert". Grouped
/// so a refactor that adds a sibling predicate field (alternate
/// expression language, alternate severity ramp) lands in one
/// place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulePredicate {
    /// CEL expression evaluated against the per-frame `object` /
    /// `camera` / `now` context.
    pub when: String,
    pub severity: String,
}

/// Scope filters — which cameras + zones the rule applies to.
/// Both gates short-circuit at the start of the evaluator.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleGates {
    #[serde(default)]
    pub camera_filter: Option<Vec<CameraId>>,
    /// Zone-id allow-list. When `Some` and non-empty, an object only
    /// matches the rule if its bbox centre falls inside at least one
    /// zone whose `id` appears in this list AND that zone is defined
    /// on the camera producing the event. `None` or empty = no zone
    /// gate (rule fires anywhere in the frame).
    ///
    /// The pipeline looks up the zones on the camera at evaluation
    /// time so a rule transparently follows zone-polygon edits — the
    /// rule config only stores ids, never the polygons themselves.
    #[serde(default)]
    pub zones: Option<Vec<String>>,
}

/// Debounce + cooldown — the three knobs that suppress runaway
/// alerts on noisy detectors. All three default to the
/// production-tested values from the original flat config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDebounce {
    #[serde(default = "default_min_track_age_ms")]
    pub min_track_age_ms: u64,
    #[serde(default = "default_consecutive_frames")]
    pub consecutive_frames: u32,
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
}

impl Default for RuleDebounce {
    fn default() -> Self {
        Self {
            min_track_age_ms: default_min_track_age_ms(),
            consecutive_frames: default_consecutive_frames(),
            cooldown_ms: default_cooldown_ms(),
        }
    }
}

/// One configured alerting rule. Wire shape is flat — `predicate`,
/// `gates`, and `debounce` are `#[serde(flatten)]`'d so every
/// existing TOML rule and every payload the admin UI sends remains
/// bit-for-bit compatible. The nested Rust groups are purely a
/// code-organisation refactor: the supervisor / preview pipeline
/// can take `&RulePredicate` when it only needs the CEL, and
/// readers can tell at a glance which fields belong to the
/// scope-gate vs. the debounce ladder vs. the predicate itself.
///
/// Note: `#[serde(deny_unknown_fields)]` is intentionally omitted
/// — it's incompatible with `#[serde(flatten)]` (same trade-off as
/// `CameraConfig`; see its doc-comment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleConfig {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub predicate: RulePredicate,
    #[serde(flatten)]
    pub gates: RuleGates,
    #[serde(flatten)]
    pub debounce: RuleDebounce,
    #[serde(default = "crate::inference::default_true")]
    pub enabled: bool,
    /// M7 per-rule sink routing. The `"<kind>:<name>"` ids of the
    /// alert-delivery sinks this rule's matches are enqueued to.
    /// Empty (the default) routes to **every** configured sink, so
    /// existing rules keep delivering everywhere. A non-empty list
    /// restricts delivery to the named sinks (filtered at dispatch
    /// time to those that actually exist, so a stale id never
    /// produces an undeliverable outbox row). This is distinct from
    /// the per-rule delivery *policy* (`rules.delivery_policy_json`,
    /// which gates whether/when delivery happens): routing decides
    /// *which* sinks, the policy decides *if* and *when*.
    ///
    /// Lives inside `rules.config_json` (no schema migration). Old
    /// payloads without the field deserialize to an empty list via
    /// `#[serde(default)]`, preserving the route-to-all behaviour.
    #[serde(default)]
    pub sinks: Vec<String>,
    /// Phase 8 — when `true`, alerts from this rule are sent to the cloud
    /// with `verification_state = candidate` so the cloud VLM
    /// behavior-verifier adjudicates them (advancing to `verified` /
    /// `dismissed` / `review`), instead of the default `verified` that
    /// shows in the console immediately. Only meaningful when the org is
    /// on a tier with cloud verification; otherwise the alert simply stays
    /// `candidate`. Lives inside `rules.config_json` (no schema
    /// migration); old payloads without the field deserialize to `false`.
    #[serde(default)]
    pub verify: bool,
    /// SPEC-086 / ADR-095 — play stored talk-down audio when this rule fires.
    /// Lives inside `rules.config_json` (no migration); old payloads read `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub talk_down: Option<RuleTalkDown>,
}

/// SPEC-086 / ADR-095 — what a rule plays when it fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleTalkDown {
    /// 1-3 steps; step n+1 plays when the rule fires again within
    /// `escalate_within_secs` of step n ending.
    pub steps: Vec<TalkDownStep>,
    /// 10..=300.
    #[serde(default = "d_escalate")]
    pub escalate_within_secs: u32,
    /// 10..=3600.
    #[serde(default = "d_cooldown")]
    pub cooldown_secs: u32,
    /// `None` = the delivery schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<nexus_types::DeliverySchedule>,
    /// Session volume 0-100; `None` = 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    /// Only for rules that live on one core; `None` = the camera's speaker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<crate::SpeakerId>,
    /// A `verify` rule plays only when this is set (ADR-095).
    #[serde(default)]
    pub play_before_verification: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TalkDownStep {
    pub audio_uid: String,
    /// 1..=3.
    #[serde(default = "d_repeat")]
    pub repeat: u8,
}

fn d_escalate() -> u32 {
    30
}
fn d_cooldown() -> u32 {
    60
}
fn d_repeat() -> u8 {
    1
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Shape check for a rule's talk-down action. `fleet_scope_is_core` is true
/// for local rules and core-scoped fleet applies — the only places a
/// `speaker_id` (local to one core) is meaningful. A uid unknown on this core
/// is accepted: rules and audio arrive in separate pushes.
pub fn validate_rule_talk_down(td: &RuleTalkDown, fleet_scope_is_core: bool) -> Result<(), String> {
    if !(1..=3).contains(&td.steps.len()) {
        return Err(format!("talk_down needs 1-3 steps, got {}", td.steps.len()));
    }
    for (i, st) in td.steps.iter().enumerate() {
        if !is_uuid(&st.audio_uid) {
            return Err(format!("talk_down step {}: audio_uid is not a UUID", i + 1));
        }
        if !(1..=3).contains(&st.repeat) {
            return Err(format!("talk_down step {}: repeat must be 1-3", i + 1));
        }
    }
    if !(10..=300).contains(&td.escalate_within_secs) {
        return Err("talk_down escalate_within_secs must be 10-300".into());
    }
    if !(10..=3600).contains(&td.cooldown_secs) {
        return Err("talk_down cooldown_secs must be 10-3600".into());
    }
    if td.volume.is_some_and(|v| v > 100) {
        return Err("talk_down volume must be 0-100".into());
    }
    if let Some(sch) = &td.schedule {
        sch.validate()
            .map_err(|e| format!("talk_down schedule: {e}"))?;
    }
    if td.speaker_id.is_some() && !fleet_scope_is_core {
        return Err("talk_down speaker_id is only allowed on core-scoped or local rules".into());
    }
    Ok(())
}

impl RuleConfig {
    /// [`validate_rule_talk_down`] plus the D10 check that needs the rule's own
    /// cooldown: the escalation window must exceed it or the rule can never escalate.
    pub fn validate_talk_down(&self, fleet_scope_is_core: bool) -> Result<(), String> {
        let Some(td) = &self.talk_down else {
            return Ok(());
        };
        validate_rule_talk_down(td, fleet_scope_is_core)?;
        let cooldown_secs = self.debounce.cooldown_ms / 1000;
        if u64::from(td.escalate_within_secs) <= cooldown_secs {
            return Err(format!(
                "talk_down escalate_within_secs ({}) must be greater than the rule's cooldown ({cooldown_secs} s)",
                td.escalate_within_secs
            ));
        }
        Ok(())
    }
}

fn default_min_track_age_ms() -> u64 {
    500
}
fn default_consecutive_frames() -> u32 {
    2
}
fn default_cooldown_ms() -> u64 {
    30_000
}

#[cfg(test)]
mod talk_down_tests {
    use super::*;

    const UID: &str = "0190f3a2-7b1c-7e0a-9c55-3f2a1b4c5d6e";

    fn td() -> RuleTalkDown {
        serde_json::from_value(serde_json::json!({"steps": [{"audio_uid": UID}]})).unwrap()
    }

    fn rule(json: serde_json::Value) -> RuleConfig {
        let mut base =
            serde_json::json!({"id": "r", "name": "n", "when": "true", "severity": "low"});
        base.as_object_mut()
            .unwrap()
            .extend(json.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn old_payload_reads_none_and_writes_no_key() {
        let r = rule(serde_json::json!({}));
        assert!(r.talk_down.is_none());
        assert!(serde_json::to_value(&r).unwrap().get("talk_down").is_none());
    }

    #[test]
    fn defaults_apply() {
        let t = td();
        assert_eq!((t.escalate_within_secs, t.cooldown_secs), (30, 60));
        assert_eq!(t.steps[0].repeat, 1);
        assert!(!t.play_before_verification);
        assert!(validate_rule_talk_down(&t, false).is_ok());
    }

    #[test]
    fn refuses_each_violation() {
        let bad = |f: &dyn Fn(&mut RuleTalkDown), core: bool| {
            let mut t = td();
            f(&mut t);
            validate_rule_talk_down(&t, core).is_err()
        };
        let step = || td().steps[0].clone();
        assert!(bad(&|t| t.steps.clear(), false));
        assert!(bad(&|t| t.steps = vec![step(); 4], false));
        assert!(bad(&|t| t.steps[0].audio_uid = "nope".into(), false));
        assert!(bad(&|t| t.steps[0].repeat = 0, false));
        assert!(bad(&|t| t.steps[0].repeat = 4, false));
        assert!(bad(&|t| t.escalate_within_secs = 9, false));
        assert!(bad(&|t| t.escalate_within_secs = 301, false));
        assert!(bad(&|t| t.cooldown_secs = 9, false));
        assert!(bad(&|t| t.cooldown_secs = 3601, false));
        assert!(bad(&|t| t.volume = Some(101), false));
        assert!(bad(
            &|t| t.schedule = Some(nexus_types::DeliverySchedule { grid: vec![] }),
            false
        ));
        assert!(bad(&|t| t.speaker_id = Some(1), false));
        assert!(!bad(&|t| t.speaker_id = Some(1), true));
        assert!(!bad(
            &|t| t.schedule = Some(nexus_types::DeliverySchedule::always()),
            false
        ));
    }

    #[test]
    fn escalate_window_must_exceed_rule_cooldown() {
        let r = rule(serde_json::json!({
            "cooldown_ms": 30000,
            "talk_down": {"steps": [{"audio_uid": UID}], "escalate_within_secs": 30}
        }));
        let e = r.validate_talk_down(false).unwrap_err();
        assert!(e.contains("30") && e.contains("cooldown"), "{e}");
        let ok = rule(serde_json::json!({
            "cooldown_ms": 29000,
            "talk_down": {"steps": [{"audio_uid": UID}], "escalate_within_secs": 30}
        }));
        assert!(ok.validate_talk_down(false).is_ok());
    }
}
