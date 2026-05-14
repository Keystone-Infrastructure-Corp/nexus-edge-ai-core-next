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
use nexus_config::{RuleConfig, RulesBackendKind, RulesConfig};
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
        let prog = Program::compile(&rule.when)
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
    pub fn evaluate(
        &self,
        camera_id: CameraId,
        frame_id: FrameId,
        trace_id: &TraceId,
        objects: &[TrackedObject],
    ) -> Vec<AlertEvent> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let rules = self.rules.load();
        let mut out = Vec::new();
        let mut state = self.track_state.lock();

        for rule in rules.iter() {
            let cfg = &rule.config;
            if !cfg.enabled {
                continue;
            }
            if let Some(filter) = &cfg.camera_filter {
                if !filter.contains(&camera_id) {
                    continue;
                }
            }

            for o in objects {
                if o.age_ms < cfg.min_track_age_ms {
                    continue;
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

                if entry.consecutive_hits < cfg.consecutive_frames {
                    continue;
                }
                if now_ms - entry.last_emitted_unix_ms < cfg.cooldown_ms as i64 {
                    continue;
                }
                entry.last_emitted_unix_ms = now_ms;

                let severity = parse_severity(&cfg.severity);
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
            camera_filter: None,
            when: "object.label == 'person'".into(),
            severity: "high".into(),
            min_track_age_ms: 0,
            consecutive_frames: 1,
            cooldown_ms: 0,
            enabled: true,
        };
        let eng = CelEngine::new();
        let compiled = eng.compile(&cfg).unwrap();
        assert!(eng
            .matches(&compiled, &obj("person", 0.9, 1000), 1)
            .unwrap());
        assert!(!eng.matches(&compiled, &obj("dog", 0.9, 1000), 1).unwrap());
    }
}
