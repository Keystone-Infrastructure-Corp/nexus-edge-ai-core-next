//! Bulk re-probe of camera substreams (SPEC-069 Phase 2).
//!
//! Answers "which of these cameras could analyse a substream, and what
//! would change?" for a fleet that was commissioned before
//! `analysis_url` existed. It **proposes only** — nothing here writes a
//! camera. Applying a proposal writes through `upsert_camera_tx` with an
//! audit row committed in the same transaction, exactly as the camera
//! editor does, so a bulk change to what a site analyses is as traceable
//! as a single-camera edit.
//!
//! The probe runs on the appliance because that is where the
//! credentials are: RTSP userinfo is edge-resident (REPO_BOUNDARY R5b),
//! so the cloud cannot authenticate an ONVIF call against a camera that
//! already exists.

use nexus_config::CameraConfig;
use serde::{Deserialize, Serialize};

use crate::discovery::analysis_pick::{describe, pick_analysis_stream};

/// What a reprobe concluded for one camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReprobeOutcome {
    /// A usable analysis stream was found and differs from the current
    /// value. The only outcome an operator can apply.
    Set,
    /// The camera already carries exactly this `analysis_url`.
    Unchanged,
    /// Probed fine; no profile survives the selection policy.
    NoSubstream,
    /// The ONVIF probe failed — offline, refused, or no endpoint
    /// configured.
    Unreachable,
}

/// One row of a reprobe proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReprobeProposal {
    pub camera_id: i64,
    pub camera_name: String,
    pub outcome: ReprobeOutcome,
    /// Credential-redacted current value, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Credential-redacted proposed value, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed: Option<String>,
    /// Why there is no proposal, in words an operator can act on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The pick is smaller than the detector input — still applicable,
    /// but the operator is told.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub below_detector_input: bool,
}

/// Copy `main`'s userinfo onto a bare ONVIF-reported stream URI.
///
/// ONVIF returns stream URIs without credentials; the appliance already
/// holds the camera's, so it can complete the URL without anything
/// crossing the tunnel.
pub fn with_credentials_from(main: &url::Url, uri: &str) -> Option<url::Url> {
    let mut out = url::Url::parse(uri).ok()?;
    if main.username().is_empty() {
        return Some(out);
    }
    out.set_username(main.username()).ok()?;
    out.set_password(main.password()).ok()?;
    Some(out)
}

/// Build the proposal for one camera from its probed profiles.
///
/// Pure — `profiles` is whatever the ONVIF probe returned (or `Err` if
/// it failed), so the whole decision is testable without a camera.
///
/// Returns the resolved URL separately from the proposal: the URL
/// carries the camera's credentials and must never leave the
/// appliance (REPO_BOUNDARY R5b), so only the proposal is serialised.
/// Apply re-derives the URL here rather than accepting one back.
pub fn propose_for_camera(
    cam: &CameraConfig,
    profiles: Result<Vec<crate::discovery::onvif_media::MediaStream>, String>,
    supervisor_pixels: u64,
    redact: impl Fn(&str) -> String,
) -> (ReprobeProposal, Option<url::Url>) {
    let current = cam.ingest.analysis_url.as_ref().map(|u| redact(u.as_str()));
    let base = ReprobeProposal {
        camera_id: cam.id,
        camera_name: cam.name.clone(),
        outcome: ReprobeOutcome::Unreachable,
        current: current.clone(),
        proposed: None,
        reason: None,
        below_detector_input: false,
    };

    let profiles = match profiles {
        Ok(p) => p,
        Err(e) => {
            return (
                ReprobeProposal {
                    reason: Some(format!(
                        "Could not reach the camera over ONVIF: {}",
                        redact(&e)
                    )),
                    ..base
                },
                None,
            )
        }
    };

    let main_res = profiles
        .iter()
        .find(|p| {
            url::Url::parse(&p.uri).is_ok_and(|u| {
                u.host_str() == cam.ingest.url.host_str() && u.path() == cam.ingest.url.path()
            })
        })
        .and_then(|p| p.resolution.as_deref())
        .and_then(parse_wxh);
    let Some(main_res) = main_res else {
        return (
            ReprobeProposal {
                outcome: ReprobeOutcome::NoSubstream,
                reason: Some(
                    "Could not identify this camera's main stream among its ONVIF profiles, so \
                     there is nothing to compare a substream against."
                        .into(),
                ),
                ..base
            },
            None,
        );
    };

    match pick_analysis_stream(
        &profiles,
        &cam.ingest.url,
        main_res,
        cam.ingest.codec,
        supervisor_pixels,
    ) {
        Err(e) => (
            ReprobeProposal {
                outcome: ReprobeOutcome::NoSubstream,
                reason: Some(describe(e).into()),
                ..base
            },
            None,
        ),
        Ok(pick) => {
            let Some(full) = with_credentials_from(&cam.ingest.url, &pick.stream.uri) else {
                return (
                    ReprobeProposal {
                        outcome: ReprobeOutcome::NoSubstream,
                        reason: Some("The camera reported an unparseable stream URL.".into()),
                        ..base
                    },
                    None,
                );
            };
            if cam.ingest.analysis_url.as_ref() == Some(&full) {
                return (
                    ReprobeProposal {
                        outcome: ReprobeOutcome::Unchanged,
                        proposed: Some(redact(full.as_str())),
                        ..base
                    },
                    None,
                );
            }
            (
                ReprobeProposal {
                    outcome: ReprobeOutcome::Set,
                    proposed: Some(redact(full.as_str())),
                    below_detector_input: pick.below_detector_input,
                    ..base
                },
                Some(full),
            )
        }
    }
}

fn parse_wxh(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    // A zero height would make the main stream's aspect ratio infinite,
    // which fails every candidate and tells the operator to check their
    // aspect ratios when the real fault is a malformed probe response.
    (w > 0 && h > 0).then_some((w, h))
}

/// Pixel count of the supervisor frame this camera's analysis will run
/// at, which is the floor a substream has to clear.
///
/// Uses the camera's own overrides when it has them and the default
/// preset's 512 px otherwise. That default is *not* read from
/// `inference.model.input_width`, which the admin API does not carry —
/// so a deployment that raised its global detector width could see a
/// proposal ranked against 512 rather than its true frame. The effect
/// is confined to which of two substreams is preferred and to the
/// `below_detector_input` advisory; the reconciler still builds the
/// session at the camera's real supervisor size.
pub fn supervisor_pixels_for(cam: &CameraConfig) -> u64 {
    const DEFAULT_DETECTOR_WIDTH: u32 = 512;
    let det_w = cam
        .detector
        .model_override
        .as_ref()
        .map(|m| m.input_width)
        .unwrap_or(DEFAULT_DETECTOR_WIDTH);
    let sup_input = cam.behavior.supervisor_width.unwrap_or(det_w).max(det_w);
    let (w, h) = nexus_pipeline::supervisor_frame_for(sup_input);
    u64::from(w) * u64::from(h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::onvif_media::MediaStream;
    use nexus_config::{CameraBehavior, CameraDetector, CameraIngest, CameraOnvif, CameraTalkDown};
    use url::Url;

    fn redact(s: &str) -> String {
        s.replace("secret", "<redacted>")
    }

    fn cam(analysis: Option<&str>) -> CameraConfig {
        CameraConfig {
            id: 1,
            name: "lot-1".into(),
            ingest: CameraIngest {
                url: Url::parse("rtsp://admin:secret@10.0.0.5:554/Streaming/Channels/101").unwrap(),
                analysis_url: analysis.map(|a| Url::parse(a).unwrap()),
                enabled: true,
                max_fps: 15,
                codec: Some(nexus_types::CodecKind::H265),
            },
            detector: CameraDetector {
                prompts: vec![],
                visual_prompts: vec![],
                model_override: None,
            },
            behavior: CameraBehavior::default(),
            onvif: CameraOnvif::default(),
            talk_down: CameraTalkDown::default(),
            zones: vec![],
        }
    }

    fn stream(uri: &str, codec: &str, res: &str) -> MediaStream {
        MediaStream {
            token: uri.into(),
            name: uri.into(),
            uri: uri.into(),
            codec: Some(codec.into()),
            codec_kind: match codec {
                "H264" => Some(nexus_types::CodecKind::H264),
                "H265" => Some(nexus_types::CodecKind::H265),
                _ => None,
            },
            resolution: Some(res.into()),
        }
    }

    fn pair() -> Vec<MediaStream> {
        vec![
            stream(
                "rtsp://10.0.0.5:554/Streaming/Channels/101",
                "H265",
                "3840x2160",
            ),
            stream(
                "rtsp://10.0.0.5:554/Streaming/Channels/102",
                "H264",
                "640x360",
            ),
        ]
    }

    const SUP: u64 = 512 * 288;

    #[test]
    fn a_camera_with_a_substream_proposes_it_with_credentials_carried_over() {
        let (p, apply) = propose_for_camera(&cam(None), Ok(pair()), SUP, redact);
        assert_eq!(p.outcome, ReprobeOutcome::Set);
        // The resolved URL must be usable — bare ONVIF URIs have no
        // credentials and would fail to authenticate — but it stays on
        // the appliance.
        assert_eq!(
            apply.as_ref().map(url::Url::as_str),
            Some("rtsp://admin:secret@10.0.0.5:554/Streaming/Channels/102")
        );
        assert!(!p.below_detector_input);
    }

    /// REPO_BOUNDARY R5b. The proposal is what crosses the tunnel, so
    /// **no field on it** may carry a credential — not the current
    /// value, not the proposed one, and not the failure reason. An
    /// earlier revision returned the usable URL in a third field beside
    /// the two redacted ones, which redacted nothing in practice.
    #[test]
    fn no_field_that_crosses_the_tunnel_carries_a_credential() {
        let (p, _) = propose_for_camera(&cam(None), Ok(pair()), SUP, redact);
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            !json.contains("secret"),
            "the reprobe proposal leaked a camera credential: {json}"
        );
    }

    /// The probe's own error string is passed through to the operator,
    /// so it goes through the redactor too — an ONVIF stack that echoes
    /// the URL it failed on would otherwise leak past the fields that
    /// are scrubbed.
    #[test]
    fn the_failure_reason_is_redacted_too() {
        let (p, _) = propose_for_camera(
            &cam(None),
            Err("connect to rtsp://admin:secret@10.0.0.5 failed".into()),
            SUP,
            redact,
        );
        assert!(!p.reason.unwrap().contains("secret"));
    }

    /// Re-running a reprobe over an already-converted fleet must be a
    /// no-op, not 53 rows offering to set what is already set.
    #[test]
    fn a_camera_already_on_its_substream_reports_unchanged() {
        let c = cam(Some(
            "rtsp://admin:secret@10.0.0.5:554/Streaming/Channels/102",
        ));
        let (p, apply) = propose_for_camera(&c, Ok(pair()), SUP, redact);
        assert_eq!(p.outcome, ReprobeOutcome::Unchanged);
        assert!(apply.is_none(), "an unchanged row is not applicable");
    }

    /// An offline camera is an informational row, not a failed batch —
    /// a mixed fleet is expected to produce these.
    #[test]
    fn an_unreachable_camera_says_so_rather_than_failing_the_batch() {
        let (p, apply) =
            propose_for_camera(&cam(None), Err("connection refused".into()), SUP, redact);
        assert_eq!(p.outcome, ReprobeOutcome::Unreachable);
        assert!(p.reason.unwrap().contains("connection refused"));
        assert!(apply.is_none());
    }

    /// A single-stream camera is a normal answer and must be
    /// distinguishable from one that could not be reached.
    #[test]
    fn a_camera_with_only_a_main_stream_reports_no_substream() {
        let only_main = vec![stream(
            "rtsp://10.0.0.5:554/Streaming/Channels/101",
            "H265",
            "3840x2160",
        )];
        let (p, _) = propose_for_camera(&cam(None), Ok(only_main), SUP, redact);
        assert_eq!(p.outcome, ReprobeOutcome::NoSubstream);
        assert!(p.reason.unwrap().contains("no second stream"));
    }

    /// The aspect-ratio refusal has to reach the operator in words,
    /// because "no substream" would be a lie — there is one, and it is
    /// rejected for a reason they can act on.
    #[test]
    fn an_aspect_mismatch_explains_itself() {
        let mismatched = vec![
            stream(
                "rtsp://10.0.0.5:554/Streaming/Channels/101",
                "H265",
                "1920x1080",
            ),
            stream(
                "rtsp://10.0.0.5:554/Streaming/Channels/102",
                "H264",
                "640x480",
            ),
        ];
        let (p, _) = propose_for_camera(&cam(None), Ok(mismatched), SUP, redact);
        assert_eq!(p.outcome, ReprobeOutcome::NoSubstream);
        assert!(p.reason.unwrap().contains("shift your zones"));
    }
}
