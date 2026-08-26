//! SPEC-040 — Edge label-space reporting (edge-side computation).
//!
//! The edge reports the label space its **resolved, baked** model actually
//! carries so the cloud stops inferring capability from hand-synced
//! mirrors (ADR-054, ADR-055). This module computes that
//! [`LabelSpaceReport`] from the resolved model and decides *when* to send
//! it: on connect, and again on any model change (OTA swap, detector-kind
//! change, or prompt-subset change that alters the baked set). It is
//! **idempotent** — an identical resolved space produces no new report.
//!
//! What crosses the wire is an additive `v=1` message whose kind and
//! payload live in the cloud repo's `proto/v1.json` (R3: the wire protocol
//! is owned by the cloud repo and vendored into core). This module owns
//! only the edge-side payload computation and the send-decision; the wire
//! kind and the cloud ingest handler are the cross-repo half of the spec.

use nexus_types::{CameraId, LabelSpaceReport};

use crate::coco_labels::closed_vocab_domain_labels;

/// The resolved model facts the edge knows once a graph is loaded for a
/// camera: which graph, at which ladder rung, and the open-vocab terms
/// **baked into that graph** (not the configured prompt subset).
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model_id: String,
    pub ladder_rung: String,
    /// Open-vocab terms baked at export. May be empty for a pure
    /// closed-vocab (COCO) model.
    pub baked_open_vocab: Vec<String>,
    /// Whether this graph emits the closed-vocab COCO→domain labels.
    pub emits_closed_vocab: bool,
}

/// Compute the per-camera [`LabelSpaceReport`] for a resolved model. The
/// vocab lists are sorted and de-duplicated so the report is stable —
/// prerequisite for idempotence.
#[must_use]
pub fn compute_report(camera_id: CameraId, model: &ResolvedModel) -> LabelSpaceReport {
    let mut baked_open_vocab = model.baked_open_vocab.clone();
    baked_open_vocab.sort();
    baked_open_vocab.dedup();

    let closed_vocab = if model.emits_closed_vocab {
        let mut v: Vec<String> = closed_vocab_domain_labels()
            .into_iter()
            .map(str::to_owned)
            .collect();
        v.sort();
        v.dedup();
        v
    } else {
        Vec::new()
    };

    LabelSpaceReport {
        camera_id,
        model_id: model.model_id.clone(),
        ladder_rung: model.ladder_rung.clone(),
        baked_open_vocab,
        closed_vocab,
    }
}

/// Tracks the last label space reported per camera and decides whether a
/// fresh report is warranted. Returns `Some(report)` on the first
/// observation for a camera (the on-connect report) and on any subsequent
/// change to the resolved space; returns `None` when the space is
/// unchanged (idempotent).
#[derive(Debug, Default)]
pub struct LabelSpaceReporter {
    last: std::collections::HashMap<CameraId, LabelSpaceReport>,
}

impl LabelSpaceReporter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer the current resolved model for a camera. Emits a report on
    /// connect and on change; suppresses an identical resolved space.
    pub fn on_resolved(
        &mut self,
        camera_id: CameraId,
        model: &ResolvedModel,
    ) -> Option<LabelSpaceReport> {
        let report = compute_report(camera_id, model);
        match self.last.get(&camera_id) {
            Some(prev) if *prev == report => None,
            _ => {
                self.last.insert(camera_id, report.clone());
                Some(report)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_vocab_model() -> ResolvedModel {
        ResolvedModel {
            model_id: "yoloe-v1".into(),
            ladder_rung: "1024x576".into(),
            baked_open_vocab: vec!["bolt cutter".into(), "angle grinder".into()],
            emits_closed_vocab: true,
        }
    }

    #[test]
    fn report_carries_resolved_model_rung_and_both_vocabs() {
        let r = compute_report(3, &open_vocab_model());
        assert_eq!(r.camera_id, 3);
        assert_eq!(r.model_id, "yoloe-v1");
        assert_eq!(r.ladder_rung, "1024x576");
        // Baked open vocab is sorted+deduped.
        assert_eq!(r.baked_open_vocab, vec!["angle grinder", "bolt cutter"]);
        // Closed vocab is the domain-label set, sorted.
        assert!(r.closed_vocab.contains(&"person".to_string()));
        assert!(r.closed_vocab.contains(&"vehicle.car".to_string()));
        let mut sorted = r.closed_vocab.clone();
        sorted.sort();
        assert_eq!(r.closed_vocab, sorted, "closed vocab is sorted");
    }

    #[test]
    fn report_uses_baked_terms_not_a_configured_prompt_subset() {
        // The report reflects only what the model resolves to; a caller
        // cannot smuggle an unbaked prompt in because the input is the
        // resolved model, not a live prompt subset.
        let mut m = open_vocab_model();
        m.baked_open_vocab = vec!["balaclava".into()];
        let r = compute_report(1, &m);
        assert_eq!(r.baked_open_vocab, vec!["balaclava"]);
    }

    #[test]
    fn closed_vocab_only_model_reports_no_open_terms() {
        let m = ResolvedModel {
            model_id: "yolo26n".into(),
            ladder_rung: "512x288".into(),
            baked_open_vocab: vec![],
            emits_closed_vocab: true,
        };
        let r = compute_report(1, &m);
        assert!(r.baked_open_vocab.is_empty());
        assert!(!r.closed_vocab.is_empty());
    }

    #[test]
    fn reporter_emits_on_connect() {
        let mut rep = LabelSpaceReporter::new();
        assert!(
            rep.on_resolved(1, &open_vocab_model()).is_some(),
            "on-connect report"
        );
    }

    #[test]
    fn reporter_is_idempotent_for_an_identical_space() {
        let mut rep = LabelSpaceReporter::new();
        assert!(rep.on_resolved(1, &open_vocab_model()).is_some());
        assert!(
            rep.on_resolved(1, &open_vocab_model()).is_none(),
            "replaying the same space produces no churn"
        );
    }

    #[test]
    fn reporter_emits_again_on_a_model_change_without_reconnect() {
        let mut rep = LabelSpaceReporter::new();
        assert!(rep.on_resolved(1, &open_vocab_model()).is_some());
        // OTA swap to a different rung — a second report on the same camera.
        let mut swapped = open_vocab_model();
        swapped.ladder_rung = "1536x864".into();
        assert!(
            rep.on_resolved(1, &swapped).is_some(),
            "model change re-reports"
        );
    }

    #[test]
    fn reporter_emits_on_a_prompt_subset_change_that_alters_the_baked_set() {
        let mut rep = LabelSpaceReporter::new();
        assert!(rep.on_resolved(1, &open_vocab_model()).is_some());
        let mut grown = open_vocab_model();
        grown.baked_open_vocab.push("pry bar".into());
        assert!(
            rep.on_resolved(1, &grown).is_some(),
            "baked-set change re-reports"
        );
    }
}
