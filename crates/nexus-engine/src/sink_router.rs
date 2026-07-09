//! M7 per-rule sink routing — engine-side resolver.
//!
//! Concrete [`nexus_pipeline::SinkRouter`] that the per-camera
//! supervisor consults to decide which alert-delivery sinks each
//! recorded alert is enqueued into the `alert_sink_outbox` for.
//!
//! Resolution combines two live sources:
//!   * the rule's `sinks` allow-list, read from the hot-reloaded
//!     [`RuleEvaluator`] (`""` / empty = route to all configured
//!     sinks);
//!   * the set of *configured* sink ids, read from the live
//!     [`SinkRegistry`] (the union of `nexus.toml` `[[sinks]]` and
//!     cloud-managed db sinks, rebuilt on `sink.config.changed`).
//!
//! A rule's explicit allow-list is **intersected** with the
//! configured set so a stale or not-yet-created sink id never
//! produces an undeliverable outbox row. An empty allow-list (or a
//! rule that is not currently loaded) routes to every configured
//! sink — matching the pre-M7 "deliver everywhere" default.

use std::collections::HashSet;
use std::sync::Arc;

use nexus_pipeline::SinkRouter;
use nexus_rules::RuleEvaluator;
use nexus_sinks::SinkRegistry;

/// See the module docs. Holds `Arc` clones of the same evaluator and
/// registry the rest of the engine uses, so it observes hot-reloads
/// of both rules and sinks without any extra plumbing.
pub struct EngineSinkRouter {
    evaluator: Arc<RuleEvaluator>,
    registry: Arc<SinkRegistry>,
}

impl EngineSinkRouter {
    pub fn new(evaluator: Arc<RuleEvaluator>, registry: Arc<SinkRegistry>) -> Self {
        Self {
            evaluator,
            registry,
        }
    }
}

impl SinkRouter for EngineSinkRouter {
    fn sinks_for(&self, rule_id: &str) -> Vec<String> {
        let configured: Vec<String> = self
            .registry
            .ids()
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        let mut result: Vec<String> = match self.evaluator.sinks_for(rule_id) {
            // Rule has an explicit allow-list — intersect with the
            // configured set (preserving the rule's order) so an id
            // that doesn't exist right now is silently dropped rather
            // than enqueued as a dead outbox row.
            Some(list) if !list.is_empty() => {
                let live: HashSet<&str> = configured.iter().map(String::as_str).collect();
                list.into_iter()
                    .filter(|s| live.contains(s.as_str()))
                    .collect()
            }
            // Empty allow-list, or rule not currently loaded — route
            // to every configured sink (the default).
            _ => configured,
        };
        // Always union the reserved (subsystem-owned) sinks. The
        // always-on `cloud:console` audit sink must receive EVERY alert
        // regardless of a rule's external-sink allow-list, so it is not
        // subject to the intersect above. Deduped in case a rule
        // explicitly names it.
        for reserved in self.registry.reserved_ids() {
            let id = reserved.to_string();
            if !result.contains(&id) {
                result.push(id);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::{
        RuleConfig, RuleDebounce, RuleGates, RulePredicate, RulesBackendKind, RulesConfig,
    };
    use nexus_sinks::{AlertSink, SinkError, SinkId};

    fn rule(id: &str, sinks: Vec<String>) -> RuleConfig {
        RuleConfig {
            id: id.into(),
            name: id.into(),
            predicate: RulePredicate {
                when: "true".into(),
                severity: "low".into(),
            },
            gates: RuleGates::default(),
            debounce: RuleDebounce {
                min_track_age_ms: 0,
                consecutive_frames: 1,
                cooldown_ms: 0,
            },
            enabled: true,
            sinks,
            verify: false,
        }
    }

    /// Minimal sink whose only job is to carry a [`SinkId`] so the
    /// registry has something to enumerate via `ids()`.
    struct StubSink(SinkId);

    #[async_trait::async_trait]
    impl AlertSink for StubSink {
        fn kind(&self) -> &'static str {
            "stub"
        }
        fn id(&self) -> &SinkId {
            &self.0
        }
        async fn deliver(&self, _event: &nexus_types::AlertEvent) -> Result<(), SinkError> {
            Ok(())
        }
    }

    fn registry_with(ids: &[&str]) -> Arc<SinkRegistry> {
        let reg = SinkRegistry::new();
        let sinks: Vec<Arc<dyn AlertSink>> = ids
            .iter()
            .map(|raw| {
                let id = SinkId::parse(raw).expect("valid sink id");
                Arc::new(StubSink(id)) as Arc<dyn AlertSink>
            })
            .collect();
        reg.replace(sinks);
        Arc::new(reg)
    }

    fn evaluator_with(rules: &[RuleConfig]) -> Arc<RuleEvaluator> {
        let cfg = RulesConfig {
            backend: RulesBackendKind::Cel,
            ..Default::default()
        };
        Arc::new(RuleEvaluator::new(&cfg, rules).expect("compile rules"))
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[test]
    fn empty_allow_list_routes_to_all_configured() {
        let evaluator = evaluator_with(&[rule("r1", vec![])]);
        let registry = registry_with(&["webhook:primary", "sureview:central"]);
        let router = EngineSinkRouter::new(evaluator, registry);
        assert_eq!(
            sorted(router.sinks_for("r1")),
            vec![
                "sureview:central".to_string(),
                "webhook:primary".to_string()
            ]
        );
    }

    #[test]
    fn explicit_list_is_intersected_with_configured() {
        let evaluator = evaluator_with(&[rule(
            "r1",
            vec!["webhook:primary".into(), "sureview:central".into()],
        )]);
        // sureview:central is NOT configured → must be dropped.
        let registry = registry_with(&["webhook:primary"]);
        let router = EngineSinkRouter::new(evaluator, registry);
        assert_eq!(router.sinks_for("r1"), vec!["webhook:primary".to_string()]);
    }

    #[test]
    fn explicit_list_routes_only_to_named_sinks() {
        let evaluator = evaluator_with(&[rule("r1", vec!["webhook:primary".into()])]);
        let registry = registry_with(&["webhook:primary", "sureview:central"]);
        let router = EngineSinkRouter::new(evaluator, registry);
        assert_eq!(router.sinks_for("r1"), vec!["webhook:primary".to_string()]);
    }

    #[test]
    fn unknown_rule_routes_to_all_configured() {
        let evaluator = evaluator_with(&[rule("r1", vec![])]);
        let registry = registry_with(&["webhook:primary"]);
        let router = EngineSinkRouter::new(evaluator, registry);
        assert_eq!(
            router.sinks_for("does-not-exist"),
            vec!["webhook:primary".to_string()]
        );
    }

    #[test]
    fn no_configured_sinks_routes_to_nothing() {
        let evaluator = evaluator_with(&[rule("r1", vec!["webhook:primary".into()])]);
        let registry = registry_with(&[]);
        let router = EngineSinkRouter::new(evaluator, registry);
        assert!(router.sinks_for("r1").is_empty());
    }
}
