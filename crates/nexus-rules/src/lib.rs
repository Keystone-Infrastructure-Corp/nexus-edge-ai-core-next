//! Rule evaluator. Currently a single backend ([`CelEngine`]) behind a
//! [`RuleEngine`] trait so future engines (Wasm, hosted JS, etc.) can
//! drop in without churn at the call site.
//!
//! Pipeline behaviour (gates, cooldowns, per-track state) is handled by
//! [`RuleEvaluator`] — the engine implementations only answer "does this
//! object match the predicate?".

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use cel_interpreter::{Context, Program, Value as CelValue};
use chrono::Utc;
use nexus_config::{RuleConfig, RulesBackendKind, RulesConfig, ZoneConfig};
use nexus_types::{AlertEvent, Artifacts, CameraId, FrameId, Severity, TraceId, TrackedObject};
use parking_lot::Mutex;
use serde_json::Value as JsonValue;
use thiserror::Error;
use tracing::{debug, warn};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum RulesError {
    #[error("compile: rule {0}: {1}")]
    Compile(String, String),
    #[error("evaluate: rule {0}: {1}")]
    Evaluate(String, String),
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait RuleEngine: Send + Sync {
    /// Compile a rule. Called once per rule on load / reload.
    fn compile(&self, rule: &RuleConfig) -> Result<CompiledRule, RulesError>;

    /// Evaluate a compiled rule against an object. Pure — no side effects.
    fn matches(
        &self,
        compiled: &CompiledRule,
        object: &TrackedObject,
        camera_id: CameraId,
    ) -> Result<bool, RulesError>;

    fn kind(&self) -> RulesBackendKind;
}

/// Output of `compile`. Holds the parsed program alongside the original
/// config — the pipeline needs the gates.
pub struct CompiledRule {
    pub config: RuleConfig,
    program: ProgramRepr,
}

enum ProgramRepr {
    Cel(Program),
}

// ---------------------------------------------------------------------------
// CEL engine
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct CelEngine;

impl CelEngine {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RuleEngine for CelEngine {
    fn compile(&self, rule: &RuleConfig) -> Result<CompiledRule, RulesError> {
        let prog = Program::compile(&rule.predicate.when)
            .map_err(|e| RulesError::Compile(rule.id.clone(), e.to_string()))?;
        Ok(CompiledRule {
            config: rule.clone(),
            program: ProgramRepr::Cel(prog),
        })
    }

    fn matches(
        &self,
        compiled: &CompiledRule,
        object: &TrackedObject,
        camera_id: CameraId,
    ) -> Result<bool, RulesError> {
        let ProgramRepr::Cel(program) = &compiled.program;

        let mut ctx = Context::default();
        ctx.add_variable("object", object_to_cel(object))
            .map_err(|e| RulesError::Evaluate(compiled.config.id.clone(), e.to_string()))?;
        ctx.add_variable("camera", camera_to_cel(camera_id))
            .map_err(|e| RulesError::Evaluate(compiled.config.id.clone(), e.to_string()))?;
        ctx.add_variable("now", now_to_cel())
            .map_err(|e| RulesError::Evaluate(compiled.config.id.clone(), e.to_string()))?;

        match program.execute(&ctx) {
            Ok(CelValue::Bool(b)) => Ok(b),
            Ok(other) => Err(RulesError::Evaluate(
                compiled.config.id.clone(),
                format!("rule did not return Bool, got {:?}", other),
            )),
            Err(e) => Err(RulesError::Evaluate(
                compiled.config.id.clone(),
                e.to_string(),
            )),
        }
    }

    fn kind(&self) -> RulesBackendKind {
        RulesBackendKind::Cel
    }
}

fn object_to_cel(o: &TrackedObject) -> CelValue {
    let v = serde_json::json!({
        "label": o.label,
        "confidence": o.confidence,
        "track_id": o.track_id,
        "age_ms": o.age_ms,
        "age_frames": o.age_frames,
        "box": {
            "x1": o.bbox.x1, "y1": o.bbox.y1,
            "x2": o.bbox.x2, "y2": o.bbox.y2,
            "width": o.bbox.width(), "height": o.bbox.height(),
        },
        "attributes": o.attributes,
    });
    json_to_cel(&v)
}

fn camera_to_cel(id: CameraId) -> CelValue {
    json_to_cel(&serde_json::json!({ "id": id }))
}

fn now_to_cel() -> CelValue {
    let now = Utc::now();
    json_to_cel(&serde_json::json!({
        "unix_ms": now.timestamp_millis(),
        "hour": now.format("%H").to_string().parse::<i64>().unwrap_or(0),
        "day_of_week": now.format("%u").to_string().parse::<i64>().unwrap_or(0),
    }))
}

fn json_to_cel(v: &JsonValue) -> CelValue {
    // cel-interpreter 0.10 does not implement `From<serde_json::Value>` for
    // `Value`, so walk the tree manually. Mapping is straightforward: numbers
    // become Int/UInt/Float depending on shape, objects become Maps keyed by
    // string. Anything we can't represent (e.g. JSON `null`) becomes
    // `CelValue::Null`.
    match v {
        JsonValue::Null => CelValue::Null,
        JsonValue::Bool(b) => CelValue::Bool(*b),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                CelValue::Int(i)
            } else if let Some(u) = n.as_u64() {
                CelValue::UInt(u)
            } else if let Some(f) = n.as_f64() {
                CelValue::Float(f)
            } else {
                CelValue::Null
            }
        }
        JsonValue::String(s) => CelValue::String(std::sync::Arc::new(s.clone())),
        JsonValue::Array(items) => {
            let converted: Vec<CelValue> = items.iter().map(json_to_cel).collect();
            CelValue::from(converted)
        }
        JsonValue::Object(map) => {
            let entries: std::collections::HashMap<String, CelValue> = map
                .iter()
                .map(|(k, val)| (k.clone(), json_to_cel(val)))
                .collect();
            CelValue::from(entries)
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline-facing wrapper: gates, cooldowns, per-track state, hot-reload.
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct TrackState {
    consecutive_hits: u32,
    last_emitted_unix_ms: i64,
}

pub struct RuleEvaluator {
    engine: Arc<dyn RuleEngine>,
    rules: ArcSwap<Vec<Arc<CompiledRule>>>,
    /// (rule_id, camera_id, track_id) -> per-track state.
    track_state: Mutex<HashMap<(String, CameraId, u64), TrackState>>,
}

impl RuleEvaluator {
    pub fn new(cfg: &RulesConfig, rules: &[RuleConfig]) -> Result<Self, RulesError> {
        let engine: Arc<dyn RuleEngine> = match cfg.backend {
            RulesBackendKind::Cel => Arc::new(CelEngine::new()),
        };
        let compiled = compile_all(&*engine, rules)?;
        Ok(Self {
            engine,
            rules: ArcSwap::from_pointee(compiled),
            track_state: Mutex::new(HashMap::new()),
        })
    }

    /// Atomic hot-reload. Atomic from readers' POV; per-track state is preserved.
    pub fn reload(&self, rules: &[RuleConfig]) -> Result<(), RulesError> {
        let compiled = compile_all(&*self.engine, rules)?;
        self.rules.store(Arc::new(compiled));
        Ok(())
    }

    pub fn engine_kind(&self) -> RulesBackendKind {
        self.engine.kind()
    }

    /// Evaluate every rule against every tracked object on a frame.
    /// Returns the events that should be persisted + published.
    ///
    /// `frame_width` / `frame_height` are the pixel dimensions of the
    /// frame the objects were detected on; they're needed so the
    /// per-rule zone gate can normalise the object's bbox centre
    /// before doing point-in-polygon. The supervisor already has
    /// these in `Frame.width` / `Frame.height` — pass them through
    /// rather than threading the whole `Frame`, so this crate stays
    /// free of any frame/image dependency.
    ///
    /// `camera_zones` is the **full** zone list configured on the
    /// camera that produced `objects`. Zones are looked up by `id`
    /// against `rule.zones` (the rule stores only ids); a rule with
    /// no `zones` set is unaffected by this argument.
    #[allow(clippy::too_many_arguments)] // 8 args is the natural shape: rule eval inherently needs frame
                                         // dims + zones + identifiers; bundling them would just push the
                                         // boilerplate to every caller.
    pub fn evaluate(
        &self,
        camera_id: CameraId,
        frame_id: FrameId,
        trace_id: &TraceId,
        frame_width: u32,
        frame_height: u32,
        camera_zones: &[ZoneConfig],
        objects: &[TrackedObject],
    ) -> Vec<AlertEvent> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let rules = self.rules.load();
        let mut out = Vec::new();
        let mut state = self.track_state.lock();

        // Pre-resolve frame dims into f32 once; the per-object
        // normalise math doesn't need anything else.
        let fw = frame_width.max(1) as f32;
        let fh = frame_height.max(1) as f32;

        for rule in rules.iter() {
            let cfg = &rule.config;
            if !cfg.enabled {
                continue;
            }
            if let Some(filter) = &cfg.gates.camera_filter {
                if !filter.contains(&camera_id) {
                    continue;
                }
            }

            // Resolve the rule's zone allow-list into the actual
            // polygons exactly once per (rule, frame) — not per
            // (rule, object). The allow-list is typically tiny
            // (<10 entries) but we still pay an O(rules * objects)
            // multiplier in the inner loop without this.
            //
            // Tri-state encoding:
            //   * `None`     ⇒ no zone gate (rule fires anywhere)
            //   * `Some(vs)` ⇒ object must fall inside at least one
            //                  zone in `vs` (may be empty if every
            //                  configured id failed to resolve —
            //                  see below).
            //
            // Missing ids are silently skipped per-id (we log once
            // at resolution time so misconfigurations are visible).
            // But if `cfg.zones` was set + non-empty AND every id
            // failed to resolve (e.g. the operator deleted every
            // referenced zone), we keep `Some(vec![])` rather than
            // collapsing to `None` — otherwise the rule would
            // silently widen to fire on the whole frame, which is
            // the worst possible failure mode (false-positive
            // security alerts). Empty Vec ⇒ inside-any test
            // returns false ⇒ rule suppresses everywhere on this
            // camera until the operator fixes the config.
            let rule_zones: Option<Vec<&ZoneConfig>> = match &cfg.gates.zones {
                None => None,
                Some(ids) if ids.is_empty() => None,
                Some(ids) => {
                    let mut resolved: Vec<&ZoneConfig> = Vec::with_capacity(ids.len());
                    for id in ids {
                        if let Some(z) = camera_zones.iter().find(|z| &z.id == id) {
                            resolved.push(z);
                        } else {
                            debug!(rule = %cfg.id, zone = %id, camera = camera_id,
                                "rule references unknown zone id; skipping for this camera");
                        }
                    }
                    Some(resolved)
                }
            };

            for o in objects {
                if o.age_ms < cfg.debounce.min_track_age_ms {
                    continue;
                }

                // Zone gate (if any): object's bbox centre, normalised
                // by the current frame dims, must fall inside at least
                // one of the rule's allow-listed zones on this camera.
                if let Some(zones) = &rule_zones {
                    let (cx, cy) = o.bbox.center();
                    let nx = (cx / fw).clamp(0.0, 1.0);
                    let ny = (cy / fh).clamp(0.0, 1.0);
                    let inside_any = zones.iter().any(|z| point_in_polygon(nx, ny, &z.polygon));
                    if !inside_any {
                        continue;
                    }
                }

                let matched = match self.engine.matches(rule, o, camera_id) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(rule = %cfg.id, "rule eval failed: {e}");
                        continue;
                    }
                };

                let key = (cfg.id.clone(), camera_id, o.track_id);
                let entry = state.entry(key.clone()).or_default();

                if !matched {
                    entry.consecutive_hits = 0;
                    continue;
                }
                entry.consecutive_hits = entry.consecutive_hits.saturating_add(1);

                if entry.consecutive_hits < cfg.debounce.consecutive_frames {
                    continue;
                }
                if now_ms - entry.last_emitted_unix_ms < cfg.debounce.cooldown_ms as i64 {
                    continue;
                }
                entry.last_emitted_unix_ms = now_ms;

                let severity = parse_severity(&cfg.predicate.severity);
                debug!(rule = %cfg.id, label = %o.label, "rule fired");
                out.push(AlertEvent {
                    event_id: Uuid::now_v7(),
                    camera_id,
                    rule_id: cfg.id.clone(),
                    track_id: Some(o.track_id),
                    label: o.label.clone(),
                    severity,
                    bbox: Some(o.bbox),
                    frame_id,
                    captured_at: Utc::now(),
                    trace_id: trace_id.clone(),
                    artifacts: Artifacts::default(),
                    context: Default::default(),
                });
            }
        }
        out
    }
}

/// Even-odd winding test on a normalised (0..1) polygon. Returns
/// false if the polygon has fewer than 3 vertices. Duplicated here
/// instead of pulled from `nexus-tracker` so the dependency graph
/// stays `rules ← {config, types}` only — `nexus-tracker` already
/// depends on `nexus-rules` indirectly via the pipeline, and we do
/// not want a cycle.
fn point_in_polygon(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    if poly.len() < 3 {
        return false;
    }
    let mut inside = false;
    let n = poly.len();
    let xd = x as f64;
    let yd = y as f64;
    for i in 0..n {
        let (p1x, p1y) = poly[i];
        let (p2x, p2y) = poly[(i + 1) % n];
        let p1x = p1x as f64;
        let p1y = p1y as f64;
        let p2x = p2x as f64;
        let p2y = p2y as f64;
        let intersects = ((p1y > yd) != (p2y > yd))
            && (xd < ((p2x - p1x) * (yd - p1y) / ((p2y - p1y) + 1e-9) + p1x));
        if intersects {
            inside = !inside;
        }
    }
    inside
}

fn compile_all(
    engine: &dyn RuleEngine,
    rules: &[RuleConfig],
) -> Result<Vec<Arc<CompiledRule>>, RulesError> {
    let mut compiled = Vec::with_capacity(rules.len());
    for r in rules {
        compiled.push(Arc::new(engine.compile(r)?));
    }
    Ok(compiled)
}

fn parse_severity(s: &str) -> Severity {
    match s.to_ascii_lowercase().as_str() {
        "critical" => Severity::Critical,
        "high" => Severity::High,
        "low" => Severity::Low,
        _ => Severity::Medium,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_types::BBox;

    fn obj(label: &str, conf: f32, age_ms: u64) -> TrackedObject {
        TrackedObject {
            track_id: 1,
            label: label.into(),
            confidence: conf,
            bbox: BBox {
                x1: 0.0,
                y1: 0.0,
                x2: 10.0,
                y2: 10.0,
            },
            age_frames: 5,
            age_ms,
            attributes: Default::default(),
        }
    }

    #[test]
    fn cel_label_match() {
        let cfg = RuleConfig {
            id: "r1".into(),
            name: "person".into(),
            predicate: nexus_config::RulePredicate {
                when: "object.label == 'person'".into(),
                severity: "high".into(),
            },
            gates: nexus_config::RuleGates {
                camera_filter: None,
                zones: None,
            },
            debounce: nexus_config::RuleDebounce {
                min_track_age_ms: 0,
                consecutive_frames: 1,
                cooldown_ms: 0,
            },
            enabled: true,
        };
        let eng = CelEngine::new();
        let compiled = eng.compile(&cfg).unwrap();
        assert!(eng
            .matches(&compiled, &obj("person", 0.9, 1000), 1)
            .unwrap());
        assert!(!eng.matches(&compiled, &obj("dog", 0.9, 1000), 1).unwrap());
    }

    fn obj_at_pixels(x1: f32, y1: f32, x2: f32, y2: f32) -> TrackedObject {
        TrackedObject {
            track_id: 7,
            label: "person".into(),
            confidence: 0.9,
            bbox: BBox { x1, y1, x2, y2 },
            age_frames: 10,
            age_ms: 1000,
            attributes: Default::default(),
        }
    }

    fn unit_rules_cfg() -> RulesConfig {
        RulesConfig {
            backend: RulesBackendKind::Cel,
            inline: vec![],
        }
    }

    fn rule_with_zones(zones: Option<Vec<String>>) -> RuleConfig {
        RuleConfig {
            id: "zone_gated".into(),
            name: "zone-gated person".into(),
            predicate: nexus_config::RulePredicate {
                when: "object.label == 'person'".into(),
                severity: "high".into(),
            },
            gates: nexus_config::RuleGates {
                camera_filter: None,
                zones,
            },
            debounce: nexus_config::RuleDebounce {
                min_track_age_ms: 0,
                consecutive_frames: 1,
                cooldown_ms: 0,
            },
            enabled: true,
        }
    }

    /// A zone covering the top-left quadrant of a normalised frame
    /// (x:0..0.5, y:0..0.5). Lets the tests verify "object inside
    /// quad" vs "object outside quad" with one shared shape.
    fn top_left_quad_zone() -> ZoneConfig {
        ZoneConfig {
            id: "z_topleft".into(),
            name: "top-left".into(),
            polygon: vec![(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)],
            kind: nexus_config::ZoneKind::Inclusion,
        }
    }

    #[test]
    fn zone_gate_fires_when_object_centre_inside_listed_zone() {
        // 100×100 frame, object spans (10,10)-(40,40) so centre is
        // (25,25) → normalised (0.25, 0.25) → inside top-left quad.
        let rule = rule_with_zones(Some(vec!["z_topleft".into()]));
        let ev = RuleEvaluator::new(&unit_rules_cfg(), &[rule]).unwrap();
        let zones = vec![top_left_quad_zone()];
        let alerts = ev.evaluate(
            1,
            42,
            &"trace-1".into(),
            100,
            100,
            &zones,
            &[obj_at_pixels(10.0, 10.0, 40.0, 40.0)],
        );
        assert_eq!(alerts.len(), 1, "object inside listed zone should fire");
    }

    #[test]
    fn zone_gate_suppresses_when_object_centre_outside_listed_zone() {
        // Same zone, but object centre is (75,75) — outside top-left.
        let rule = rule_with_zones(Some(vec!["z_topleft".into()]));
        let ev = RuleEvaluator::new(&unit_rules_cfg(), &[rule]).unwrap();
        let zones = vec![top_left_quad_zone()];
        let alerts = ev.evaluate(
            1,
            42,
            &"trace-2".into(),
            100,
            100,
            &zones,
            &[obj_at_pixels(60.0, 60.0, 90.0, 90.0)],
        );
        assert!(
            alerts.is_empty(),
            "object outside listed zone should be suppressed, got {:?}",
            alerts
        );
    }

    #[test]
    fn zone_gate_with_none_or_empty_does_not_filter() {
        // No `zones` ⇒ rule fires anywhere.
        let rule_a = rule_with_zones(None);
        let ev_a = RuleEvaluator::new(&unit_rules_cfg(), &[rule_a]).unwrap();
        let zones = vec![top_left_quad_zone()];
        let alerts_a = ev_a.evaluate(
            1,
            1,
            &"t".into(),
            100,
            100,
            &zones,
            &[obj_at_pixels(70.0, 70.0, 80.0, 80.0)],
        );
        assert_eq!(alerts_a.len(), 1, "None zones ⇒ rule fires anywhere");

        // Empty Vec is treated the same as None — the loader and the
        // UI may send either depending on whether the user touched
        // the field; both must mean "no gate".
        let rule_b = rule_with_zones(Some(vec![]));
        let ev_b = RuleEvaluator::new(&unit_rules_cfg(), &[rule_b]).unwrap();
        let alerts_b = ev_b.evaluate(
            1,
            1,
            &"t".into(),
            100,
            100,
            &zones,
            &[obj_at_pixels(70.0, 70.0, 80.0, 80.0)],
        );
        assert_eq!(
            alerts_b.len(),
            1,
            "Empty zones Vec ⇒ rule still fires anywhere"
        );
    }

    #[test]
    fn zone_gate_with_unknown_zone_id_suppresses_everywhere() {
        // Rule references a zone the camera doesn't define — the
        // operator likely deleted it. Better to silently suppress
        // than to fire false-positively over the whole frame.
        let rule = rule_with_zones(Some(vec!["z_ghost".into()]));
        let ev = RuleEvaluator::new(&unit_rules_cfg(), &[rule]).unwrap();
        let zones = vec![top_left_quad_zone()]; // no z_ghost
        let alerts = ev.evaluate(
            1,
            1,
            &"t".into(),
            100,
            100,
            &zones,
            &[obj_at_pixels(10.0, 10.0, 40.0, 40.0)], // would fire without gate
        );
        assert!(
            alerts.is_empty(),
            "unresolved zone id ⇒ suppress, not fall back to no-gate"
        );
    }
}
