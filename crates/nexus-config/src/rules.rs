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
