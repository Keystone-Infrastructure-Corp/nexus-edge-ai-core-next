//! SPEC-069 Phase 1 — turns the raw decode-capacity + decode-health counters
//! already collected elsewhere (`system_metrics::DecodeCapacity`,
//! `nexus_pipeline::DecodeHealth`, `nexus_pipeline::AnalysisStreamStatus`)
//! into one human string an operator can act on without reading five
//! separate numbers spread across two endpoints.
//!
//! Precedence (checked in order, first match wins):
//! 1. no capacity telemetry at all              -> `Unknown`
//! 2. analysis substream fell back              -> `SubstreamUnavailable`
//! 3. decoder is losing compressed input         -> `Over` ("losing frames")
//! 4. decoder is padding output with duplicates  -> `Near`/`Over` ("padded")
//! 5. plain threshold on the binding engine's %  -> `Healthy`/`Near`/`Over`
//!
//! Step 3 takes precedence over step 4 (the "losing_frames > padded"
//! rule): a decoder that is corrupting/dropping compressed access units is
//! in worse shape than one that is merely re-serving stale frames to keep a
//! downstream `videorate` fed, even if both symptoms are present at once.

use nexus_pipeline::{AnalysisStreamStatus, DecodeHealth};

use crate::system_metrics::{DecodeCapacity, OVERSUBSCRIBED_PCT};

/// Below this the binding fixed-function engine has real headroom.
/// `[NEAR_PCT, OVERSUBSCRIBED_PCT)` is "worth an amber", and
/// `>= OVERSUBSCRIBED_PCT` matches the exact threshold that also drives
/// `decode_oversubscribed` on the cloud tunnel (`cloud_tunnel::edge_health`),
/// so the two surfaces never disagree about what "oversubscribed" means.
/// Chosen as a ruling — SPEC-069 does not specify a numeric threshold.
pub const NEAR_PCT: f32 = 70.0;

/// Duplicate-frame ratio (per mille of `sampled_frames`) above which
/// padding alone is treated as `Over` rather than `Near`. 500‰ means "more
/// than half of what the appsink received was a re-served duplicate" —
/// at that point the camera is not decoding new pictures fast enough to
/// call it a mild pressure signal. Ruling, not a spec'd number.
pub const HEAVY_PADDING_PER_MILLE: u64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeVerdict {
    /// No `DecodeCapacity` sample exists yet (e.g. GPU telemetry backend
    /// unavailable, or the very first poll before any sample lands).
    Unknown,
    Healthy,
    Near,
    Over,
    /// The camera's analysis substream was configured but fell back to
    /// mainstream decode. Reported ahead of the plain capacity thresholds
    /// because it is a knowable, actionable root cause in its own right —
    /// a healthy GPU percentage on the wire would otherwise mask it.
    SubstreamUnavailable,
}

impl DecodeVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            DecodeVerdict::Unknown => "unknown",
            DecodeVerdict::Healthy => "healthy",
            DecodeVerdict::Near => "near",
            DecodeVerdict::Over => "over",
            DecodeVerdict::SubstreamUnavailable => "substream_unavailable",
        }
    }
}

/// Pure function: given the capacity sample, decode-health counters, and
/// (optionally) this camera's analysis-stream status, returns the verdict
/// and an operator-facing summary string. The binding engine's name is
/// always named in the summary when a capacity sample exists, per the
/// P2 binding amendment ("name the binding engine in the verdict string").
pub fn compute_decode_verdict(
    capacity: Option<&DecodeCapacity>,
    health: &DecodeHealth,
    analysis: Option<&AnalysisStreamStatus>,
) -> (DecodeVerdict, String) {
    let Some(cap) = capacity else {
        return (
            DecodeVerdict::Unknown,
            "no decode-capacity telemetry available for this core".to_string(),
        );
    };

    if let Some(a) = analysis {
        if a.state == "unavailable" {
            let reason = a.reason.as_deref().unwrap_or("unknown");
            return (
                DecodeVerdict::SubstreamUnavailable,
                format!(
                    "analysis substream unavailable ({reason}); analysis has fallen back to \
                     mainstream decode, so {} at {:.1}% now also carries analysis load",
                    cap.binding_engine, cap.binding_engine_pct
                ),
            );
        }
    }

    if health.decoder_input_drops > 0 {
        return (
            DecodeVerdict::Over,
            format!(
                "{} at {:.1}% is losing compressed input ({} access units dropped ahead of the \
                 decoder) — this outranks output padding, the picture is being damaged to keep up",
                cap.binding_engine, cap.binding_engine_pct, health.decoder_input_drops
            ),
        );
    }

    if health.duplicate_frames > 0 {
        let per_mille = health.duplicate_per_mille();
        if per_mille >= HEAVY_PADDING_PER_MILLE {
            return (
                DecodeVerdict::Over,
                format!(
                    "{} at {:.1}% is padding most output with duplicates ({per_mille}\u{2030} of \
                     sampled frames) — decode throughput is not keeping pace",
                    cap.binding_engine, cap.binding_engine_pct
                ),
            );
        }
        return (
            DecodeVerdict::Near,
            format!(
                "{} at {:.1}% is padding some output with duplicates ({per_mille}\u{2030} of \
                 sampled frames)",
                cap.binding_engine, cap.binding_engine_pct
            ),
        );
    }

    if cap.binding_engine_pct >= OVERSUBSCRIBED_PCT {
        (
            DecodeVerdict::Over,
            format!(
                "{} is oversubscribed at {:.1}%",
                cap.binding_engine, cap.binding_engine_pct
            ),
        )
    } else if cap.binding_engine_pct >= NEAR_PCT {
        (
            DecodeVerdict::Near,
            format!(
                "{} is approaching capacity at {:.1}%",
                cap.binding_engine, cap.binding_engine_pct
            ),
        )
    } else {
        (
            DecodeVerdict::Healthy,
            format!(
                "{} has headroom at {:.1}%",
                cap.binding_engine, cap.binding_engine_pct
            ),
        )
    }
}

/// Rough ceiling estimate: "if this engine is at `binding_engine_pct`% while
/// producing `observed_fps`, what fps would 100% correspond to?" Returns
/// `None` when the engine has never been observed doing meaningful work
/// (`binding_engine_pct <= 0.0`) — extrapolating a ceiling from a
/// near-zero divisor would fabricate a number, not calibrate one. Also
/// `None` when `observed_fps <= 0.0` (e.g. the camera's rate window
/// hasn't seen two output events yet): reporting `Some(0.0)` right after a
/// busy decoder starts would read as "this engine has no ceiling", which
/// is the opposite of what a genuinely busy engine means.
pub fn estimate_decode_ceiling_fps(binding_engine_pct: f32, observed_fps: f32) -> Option<f32> {
    if binding_engine_pct <= 0.0 || observed_fps <= 0.0 {
        return None;
    }
    Some(observed_fps * (100.0 / binding_engine_pct))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(engine: &str, pct: f32) -> DecodeCapacity {
        DecodeCapacity {
            binding_engine: engine.to_string(),
            binding_engine_pct: pct,
            oversubscribed: pct >= OVERSUBSCRIBED_PCT,
        }
    }

    fn healthy_stream() -> AnalysisStreamStatus {
        AnalysisStreamStatus {
            mode: "substream".to_string(),
            state: "active".to_string(),
            reason: None,
            width: 640,
            height: 360,
            fps: 10.0,
        }
    }

    #[test]
    fn no_capacity_sample_is_unknown() {
        let (v, msg) = compute_decode_verdict(None, &DecodeHealth::default(), None);
        assert_eq!(v, DecodeVerdict::Unknown);
        assert!(msg.contains("no decode-capacity telemetry"));
    }

    #[test]
    fn low_utilization_is_healthy_and_names_the_binding_engine() {
        let c = cap("video-decode", 12.0);
        let (v, msg) = compute_decode_verdict(Some(&c), &DecodeHealth::default(), None);
        assert_eq!(v, DecodeVerdict::Healthy);
        assert!(msg.contains("video-decode"));
    }

    #[test]
    fn near_threshold_is_near_not_healthy() {
        let c = cap("video-decode", NEAR_PCT);
        let (v, _) = compute_decode_verdict(Some(&c), &DecodeHealth::default(), None);
        assert_eq!(v, DecodeVerdict::Near);
    }

    #[test]
    fn just_below_near_threshold_is_still_healthy() {
        let c = cap("video-decode", NEAR_PCT - 0.1);
        let (v, _) = compute_decode_verdict(Some(&c), &DecodeHealth::default(), None);
        assert_eq!(v, DecodeVerdict::Healthy);
    }

    #[test]
    fn oversubscribed_threshold_is_over() {
        let c = cap("video-decode", OVERSUBSCRIBED_PCT);
        let (v, msg) = compute_decode_verdict(Some(&c), &DecodeHealth::default(), None);
        assert_eq!(v, DecodeVerdict::Over);
        assert!(msg.contains("oversubscribed"));
    }

    /// The binding amendment's whole point: the verdict names whichever
    /// engine is saturating, not decode specifically.
    #[test]
    fn binding_engine_can_be_a_non_decode_engine() {
        let c = cap("video-enhance", 99.1);
        let (v, msg) = compute_decode_verdict(Some(&c), &DecodeHealth::default(), None);
        assert_eq!(v, DecodeVerdict::Over);
        assert!(msg.contains("video-enhance"));
        assert!(!msg.contains("video-decode"));
    }

    #[test]
    fn substream_unavailable_takes_precedence_over_a_healthy_gpu() {
        let c = cap("video-decode", 5.0);
        let unavailable = AnalysisStreamStatus {
            mode: "mainstream".to_string(),
            state: "unavailable".to_string(),
            reason: Some("no_frames".to_string()),
            width: 0,
            height: 0,
            fps: 0.0,
        };
        let (v, msg) =
            compute_decode_verdict(Some(&c), &DecodeHealth::default(), Some(&unavailable));
        assert_eq!(v, DecodeVerdict::SubstreamUnavailable);
        assert!(msg.contains("no_frames"));
    }

    /// The top of the precedence ladder, pinned against the rung directly
    /// below it. `substream_unavailable_takes_precedence_over_a_healthy_gpu`
    /// passes a `DecodeHealth::default()` (no drops), so on its own it only
    /// proves the analysis check outranks the plain capacity thresholds —
    /// moving the analysis block below the input-drop block would leave it
    /// green. A fallen-back substream is the actionable root cause even when
    /// the decoder is also shedding input, so it must still win here.
    #[test]
    fn substream_unavailable_outranks_input_drops() {
        let c = cap("video-decode", 95.0);
        let unavailable = AnalysisStreamStatus {
            mode: "mainstream".to_string(),
            state: "unavailable".to_string(),
            reason: Some("no_frames".to_string()),
            width: 0,
            height: 0,
            fps: 0.0,
        };
        let health = DecodeHealth {
            decoder_input_drops: 7,
            decoder_output_frames: 100,
            sampled_frames: 100,
            duplicate_frames: 80,
            ..Default::default()
        };
        let (v, msg) = compute_decode_verdict(Some(&c), &health, Some(&unavailable));
        assert_eq!(v, DecodeVerdict::SubstreamUnavailable);
        assert!(msg.contains("no_frames"));
        assert!(!msg.contains("losing compressed input"));
    }

    #[test]
    fn active_substream_does_not_trigger_substream_unavailable() {
        let c = cap("video-decode", 5.0);
        let active = healthy_stream();
        let (v, _) = compute_decode_verdict(Some(&c), &DecodeHealth::default(), Some(&active));
        assert_eq!(v, DecodeVerdict::Healthy);
    }

    /// The `losing_frames > padded` precedence rule: both symptoms present
    /// at once must report the input-drop verdict, not the padding one.
    #[test]
    fn losing_frames_outranks_padding_when_both_are_present() {
        let c = cap("video-decode", 5.0);
        let health = DecodeHealth {
            decoder_input_drops: 3,
            decoder_output_frames: 100,
            sampled_frames: 100,
            duplicate_frames: 80,
            ..Default::default()
        };
        let (v, msg) = compute_decode_verdict(Some(&c), &health, None);
        assert_eq!(v, DecodeVerdict::Over);
        assert!(msg.contains("losing compressed input"));
        assert!(!msg.contains("padding most") && !msg.contains("padding some"));
    }

    #[test]
    fn heavy_padding_alone_is_over() {
        let c = cap("video-decode", 5.0);
        let health = DecodeHealth {
            decoder_input_drops: 0,
            decoder_output_frames: 100,
            sampled_frames: 100,
            duplicate_frames: 60, // 600 per mille >= HEAVY_PADDING_PER_MILLE
            ..Default::default()
        };
        let (v, msg) = compute_decode_verdict(Some(&c), &health, None);
        assert_eq!(v, DecodeVerdict::Over);
        assert!(msg.contains("padding most output"));
    }

    #[test]
    fn light_padding_alone_is_near() {
        let c = cap("video-decode", 5.0);
        let health = DecodeHealth {
            decoder_input_drops: 0,
            decoder_output_frames: 100,
            sampled_frames: 100,
            duplicate_frames: 10, // 100 per mille < HEAVY_PADDING_PER_MILLE
            ..Default::default()
        };
        let (v, msg) = compute_decode_verdict(Some(&c), &health, None);
        assert_eq!(v, DecodeVerdict::Near);
        assert!(msg.contains("padding some output"));
    }

    #[test]
    fn ceiling_estimate_scales_observed_fps_to_full_utilization() {
        // 25% utilization producing 5 fps -> ceiling ~20 fps.
        let ceiling = estimate_decode_ceiling_fps(25.0, 5.0).unwrap();
        assert!((ceiling - 20.0).abs() < 0.01, "ceiling was {ceiling}");
    }

    #[test]
    fn ceiling_estimate_is_none_when_never_saturated() {
        assert_eq!(estimate_decode_ceiling_fps(0.0, 5.0), None);
    }

    #[test]
    fn ceiling_estimate_is_none_for_a_negative_utilization_reading() {
        // Defensive: a malformed/negative sysfs reading must not fabricate
        // a ceiling either.
        assert_eq!(estimate_decode_ceiling_fps(-1.0, 5.0), None);
    }

    #[test]
    fn ceiling_estimate_is_none_when_observed_fps_has_not_landed_yet() {
        // A genuinely busy engine with observed_fps still at 0.0 (e.g. the
        // rate window hasn't seen two output events yet) must not report
        // Some(0.0) — that would read backwards as "no ceiling".
        assert_eq!(estimate_decode_ceiling_fps(80.0, 0.0), None);
    }
}
