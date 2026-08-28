//! Which probed ONVIF profile should analysis decode?
//!
//! One pure function, shared by every caller that has to answer it:
//! camera discovery (both the ONVIF and CIDR tabs), the bulk reprobe
//! endpoint, and — mirrored in TypeScript — the cloud bulk-add wizard.
//! SPEC-069 makes the single implementation a requirement rather than a
//! preference: two rankings in two places is the console drift its UX
//! principles exist to prevent.
//!
//! The rules and the reasoning behind each are in
//! `SPEC-069 § Selection policy (what autodetect actually picks)`.

use super::onvif_media::MediaStream;
use nexus_types::CodecKind;

/// Why no profile was chosen. Surfaced to the operator verbatim —
/// "this camera has no substream" and "this camera was not looked at"
/// are different answers and the UI has to be able to tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoAnalysisStream {
    /// The camera reported no profiles at all.
    NoProfiles,
    /// Every profile is the main stream.
    OnlyMain,
    /// Candidates existed but all were JPEG/MJPEG — the decode chains
    /// are H.26x.
    JpegOnly,
    /// Candidates existed but none shared the main stream's aspect
    /// ratio within tolerance. Selecting one would silently shift every
    /// zone the operator drew (invariant I6).
    AspectMismatch,
    /// A profile's resolution was missing or unparseable, leaving
    /// nothing rankable.
    NoUsableResolution,
}

/// The chosen analysis profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisPick {
    pub stream: MediaStream,
    /// The pick is smaller than the detector input. Still a large
    /// decode win, but small or distant objects may be missed and the
    /// operator is told so.
    pub below_detector_input: bool,
}

/// Operator-facing wording for each way the policy can decline. Lives
/// here so discovery and the reprobe endpoint say the same thing about
/// the same camera.
#[must_use]
pub fn describe(e: NoAnalysisStream) -> &'static str {
    match e {
        NoAnalysisStream::NoProfiles => "The camera reported no stream profiles.",
        NoAnalysisStream::OnlyMain => "This camera offers no second stream.",
        NoAnalysisStream::JpegOnly => {
            "The only other stream is JPEG, which this appliance cannot decode for analysis."
        }
        NoAnalysisStream::AspectMismatch => {
            "The other stream has a different aspect ratio. Using it would shift your zones \
             and clip overlays."
        }
        NoAnalysisStream::NoUsableResolution => {
            "The camera did not report a usable resolution for its other streams."
        }
    }
}

/// Maximum fractional difference between the two streams' display
/// aspect ratios. Sensor modes that are "16:9" only approximately are
/// common — 2688×1520 is 0.53% off — and excluding them sends healthy
/// cameras down a manual-override path they do not need. The failure
/// I6 actually guards against is 16:9 against 4:3, a 33% gap.
const ASPECT_TOLERANCE: f64 = 0.01;

/// Parse ONVIF's `"WIDTHxHEIGHT"` resolution form.
fn parse_resolution(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// Is this profile the camera's main stream?
///
/// Compared on host, port and path only: the configured URL carries
/// `user:pass@` userinfo and the ONVIF-reported URI does not, so a
/// string comparison would never match and every camera would offer
/// its own main stream as an analysis candidate.
fn is_same_stream(main_url: &url::Url, uri: &str) -> bool {
    let Ok(probed) = url::Url::parse(uri) else {
        return false;
    };
    main_url.host_str() == probed.host_str()
        && main_url.port_or_known_default() == probed.port_or_known_default()
        && main_url.path() == probed.path()
}

fn is_jpeg(s: &MediaStream) -> bool {
    s.codec
        .as_deref()
        .is_some_and(|c| c.eq_ignore_ascii_case("JPEG") || c.eq_ignore_ascii_case("MJPEG"))
}

/// Pick the analysis profile for a camera, or say why there is none.
///
/// * `main_url` — the camera's configured RTSP URL, used only to
///   exclude the main stream from its own candidate list.
/// * `main_resolution` — the main stream's `(width, height)`, which
///   sets the aspect ratio every candidate is measured against.
/// * `main_codec` — used as a tie-break, never as a filter: an H.265
///   main stream with an H.264 substream is common and must work.
/// * `supervisor_pixels` — the resolved supervisor frame's pixel count.
///   Analysis never benefits from decoding more pixels than the
///   detector consumes.
pub fn pick_analysis_stream(
    profiles: &[MediaStream],
    main_url: &url::Url,
    main_resolution: (u32, u32),
    main_codec: Option<CodecKind>,
    supervisor_pixels: u64,
) -> Result<AnalysisPick, NoAnalysisStream> {
    if profiles.is_empty() {
        return Err(NoAnalysisStream::NoProfiles);
    }

    let candidates: Vec<&MediaStream> = profiles
        .iter()
        .filter(|s| !is_same_stream(main_url, &s.uri))
        .collect();
    if candidates.is_empty() {
        return Err(NoAnalysisStream::OnlyMain);
    }

    let candidates: Vec<&MediaStream> = candidates.into_iter().filter(|s| !is_jpeg(s)).collect();
    if candidates.is_empty() {
        return Err(NoAnalysisStream::JpegOnly);
    }

    let main_dar = f64::from(main_resolution.0) / f64::from(main_resolution.1);
    let mut sized: Vec<(&MediaStream, u64)> = Vec::new();
    let mut saw_aspect_mismatch = false;
    for s in candidates {
        let Some((w, h)) = s.resolution.as_deref().and_then(parse_resolution) else {
            continue;
        };
        if (f64::from(w) / f64::from(h) / main_dar - 1.0).abs() > ASPECT_TOLERANCE {
            saw_aspect_mismatch = true;
            continue;
        }
        sized.push((s, u64::from(w) * u64::from(h)));
    }
    if sized.is_empty() {
        return Err(if saw_aspect_mismatch {
            NoAnalysisStream::AspectMismatch
        } else {
            NoAnalysisStream::NoUsableResolution
        });
    }

    // Prefer the smallest profile that still feeds the detector fully;
    // fall back to the largest of the too-small ones, flagged.
    let below_detector_input = sized.iter().all(|(_, px)| *px < supervisor_pixels);
    let best = if below_detector_input {
        sized
            .iter()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| tie_break(a.0, b.0, main_codec)))
    } else {
        sized
            .iter()
            .filter(|(_, px)| *px >= supervisor_pixels)
            .min_by(|a, b| a.1.cmp(&b.1).then_with(|| tie_break(b.0, a.0, main_codec)))
    }
    .expect("sized is non-empty");

    Ok(AnalysisPick {
        stream: best.0.clone(),
        below_detector_input,
    })
}

/// Order two equally-sized profiles: the one matching the main
/// stream's codec sorts *greater*, then token order so the result is
/// deterministic across probes.
fn tie_break(
    a: &MediaStream,
    b: &MediaStream,
    main_codec: Option<CodecKind>,
) -> std::cmp::Ordering {
    let matches = |s: &MediaStream| main_codec.is_some() && s.codec_kind == main_codec;
    matches(a)
        .cmp(&matches(b))
        .then_with(|| b.token.cmp(&a.token))
}

/// What discovery should commit for a camera it has just probed.
///
/// Recording is unconditionally the largest profile — that is the
/// BUG-073 fix, and it holds whether or not the operator wants
/// substream analysis. The analysis pick is then made against it by the
/// same policy the reprobe endpoint uses.
#[derive(Debug, Clone)]
pub struct DiscoveryRecommendation {
    /// The profile to record from. `None` only when no profile reports
    /// a usable resolution.
    pub main: Option<MediaStream>,
    /// The profile to analyse, or why there is none.
    pub analysis: Result<AnalysisPick, NoAnalysisStream>,
}

/// Recommend both profiles from a freshly probed camera.
#[must_use]
pub fn recommend_for_discovery(
    profiles: &[MediaStream],
    supervisor_pixels: u64,
) -> DiscoveryRecommendation {
    let main = profiles
        .iter()
        .filter_map(|s| {
            s.resolution
                .as_deref()
                .and_then(parse_resolution)
                .map(|(w, h)| (s, u64::from(w) * u64::from(h), (w, h)))
        })
        // Ties resolve on token so two profiles at the same resolution
        // don't pick differently between probes.
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.token.cmp(&a.0.token)));
    let Some((main, _, main_res)) = main else {
        return DiscoveryRecommendation {
            main: None,
            analysis: Err(if profiles.is_empty() {
                NoAnalysisStream::NoProfiles
            } else {
                NoAnalysisStream::NoUsableResolution
            }),
        };
    };
    let analysis = match url::Url::parse(&main.uri) {
        Ok(main_url) => pick_analysis_stream(
            profiles,
            &main_url,
            main_res,
            main.codec_kind,
            supervisor_pixels,
        ),
        Err(_) => Err(NoAnalysisStream::NoUsableResolution),
    };
    DiscoveryRecommendation {
        main: Some(main.clone()),
        analysis,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(token: &str, uri: &str, codec: &str, res: &str) -> MediaStream {
        MediaStream {
            token: token.into(),
            name: token.into(),
            uri: uri.into(),
            codec: Some(codec.into()),
            codec_kind: match codec {
                "H264" => Some(CodecKind::H264),
                "H265" => Some(CodecKind::H265),
                _ => None,
            },
            resolution: Some(res.into()),
        }
    }

    fn main_url() -> url::Url {
        url::Url::parse("rtsp://admin:secret@10.0.0.5:554/Streaming/Channels/101").unwrap()
    }

    const SUPERVISOR_512X288: u64 = 512 * 288;

    #[test]
    fn the_main_stream_is_never_offered_as_its_own_analysis_stream() {
        // Same host and path, no userinfo — exactly what ONVIF returns.
        let profiles = [stream(
            "main",
            "rtsp://10.0.0.5:554/Streaming/Channels/101",
            "H265",
            "3840x2160",
        )];
        assert_eq!(
            pick_analysis_stream(
                &profiles,
                &main_url(),
                (3840, 2160),
                Some(CodecKind::H265),
                SUPERVISOR_512X288
            ),
            Err(NoAnalysisStream::OnlyMain)
        );
    }

    #[test]
    fn the_substream_is_picked_on_a_typical_hikvision_pair() {
        let profiles = [
            stream(
                "main",
                "rtsp://10.0.0.5:554/Streaming/Channels/101",
                "H265",
                "3840x2160",
            ),
            stream(
                "sub",
                "rtsp://10.0.0.5:554/Streaming/Channels/102",
                "H264",
                "640x360",
            ),
        ];
        let pick = pick_analysis_stream(
            &profiles,
            &main_url(),
            (3840, 2160),
            Some(CodecKind::H265),
            SUPERVISOR_512X288,
        )
        .unwrap();
        assert_eq!(pick.stream.token, "sub");
        assert!(!pick.below_detector_input);
    }

    #[test]
    fn an_approximately_16_9_sensor_mode_is_within_tolerance() {
        // 2688x1520 is DAR 1.7684 against the substream's 1.7778 —
        // 0.53% apart. Read as "differs", this healthy pair would be
        // excluded.
        let profiles = [stream(
            "sub",
            "rtsp://10.0.0.5:554/Streaming/Channels/102",
            "H264",
            "640x360",
        )];
        let pick = pick_analysis_stream(
            &profiles,
            &main_url(),
            (2688, 1520),
            Some(CodecKind::H265),
            SUPERVISOR_512X288,
        )
        .unwrap();
        assert_eq!(pick.stream.token, "sub");
    }

    #[test]
    fn a_4_3_substream_against_a_16_9_main_is_excluded() {
        let profiles = [stream(
            "sub",
            "rtsp://10.0.0.5:554/Streaming/Channels/102",
            "H264",
            "640x480",
        )];
        assert_eq!(
            pick_analysis_stream(
                &profiles,
                &main_url(),
                (1920, 1080),
                Some(CodecKind::H264),
                SUPERVISOR_512X288
            ),
            Err(NoAnalysisStream::AspectMismatch)
        );
    }

    #[test]
    fn jpeg_profiles_are_excluded_because_the_decode_chains_are_h26x() {
        let profiles = [stream(
            "sub",
            "rtsp://10.0.0.5:554/Streaming/Channels/102",
            "JPEG",
            "640x360",
        )];
        assert_eq!(
            pick_analysis_stream(
                &profiles,
                &main_url(),
                (1920, 1080),
                Some(CodecKind::H264),
                SUPERVISOR_512X288
            ),
            Err(NoAnalysisStream::JpegOnly)
        );
    }

    #[test]
    fn the_smallest_profile_at_or_above_the_detector_input_wins() {
        let profiles = [
            stream("a", "rtsp://10.0.0.5:554/s/1280", "H264", "1280x720"),
            stream("b", "rtsp://10.0.0.5:554/s/640", "H264", "640x360"),
            stream("c", "rtsp://10.0.0.5:554/s/1920", "H264", "1920x1080"),
        ];
        let pick = pick_analysis_stream(
            &profiles,
            &main_url(),
            (1920, 1080),
            Some(CodecKind::H264),
            SUPERVISOR_512X288,
        )
        .unwrap();
        assert_eq!(pick.stream.token, "b");
        assert!(!pick.below_detector_input);
    }

    #[test]
    fn every_profile_below_the_detector_input_takes_the_largest_and_says_so() {
        let profiles = [
            stream("tiny", "rtsp://10.0.0.5:554/s/320", "H264", "320x180"),
            stream("small", "rtsp://10.0.0.5:554/s/480", "H264", "480x270"),
        ];
        let pick = pick_analysis_stream(
            &profiles,
            &main_url(),
            (1920, 1080),
            Some(CodecKind::H264),
            SUPERVISOR_512X288,
        )
        .unwrap();
        assert_eq!(pick.stream.token, "small");
        assert!(pick.below_detector_input);
    }

    #[test]
    fn equal_sized_profiles_tie_break_on_the_main_streams_codec() {
        let profiles = [
            stream("h264", "rtsp://10.0.0.5:554/s/a", "H264", "640x360"),
            stream("h265", "rtsp://10.0.0.5:554/s/b", "H265", "640x360"),
        ];
        let pick = pick_analysis_stream(
            &profiles,
            &main_url(),
            (1920, 1080),
            Some(CodecKind::H265),
            SUPERVISOR_512X288,
        )
        .unwrap();
        assert_eq!(pick.stream.token, "h265");
    }

    #[test]
    fn a_camera_that_reports_nothing_is_distinguishable_from_one_with_no_substream() {
        assert_eq!(
            pick_analysis_stream(
                &[],
                &main_url(),
                (1920, 1080),
                Some(CodecKind::H264),
                SUPERVISOR_512X288
            ),
            Err(NoAnalysisStream::NoProfiles)
        );
    }

    /// BUG-073: discovery used to commit whichever profile the picker
    /// liked as the camera's single URL, so a typical camera recorded
    /// its evidence at 640×360. Recording is now unconditionally the
    /// largest profile, regardless of what analysis wants.
    #[test]
    fn discovery_records_the_largest_profile_and_analyses_the_small_one() {
        let profiles = [
            stream(
                "sub",
                "rtsp://10.0.0.5:554/Streaming/Channels/102",
                "H264",
                "640x360",
            ),
            stream(
                "main",
                "rtsp://10.0.0.5:554/Streaming/Channels/101",
                "H265",
                "3840x2160",
            ),
        ];
        let r = recommend_for_discovery(&profiles, SUPERVISOR_512X288);
        assert_eq!(r.main.as_ref().unwrap().token, "main");
        assert_eq!(r.analysis.unwrap().stream.token, "sub");
    }

    /// A single-stream camera still commits successfully — recording on
    /// its only profile, analysis declining with a reason. It is a
    /// caveat on a good row, never a failed one.
    #[test]
    fn a_single_stream_camera_still_yields_a_main_profile() {
        let profiles = [stream(
            "only",
            "rtsp://10.0.0.5:554/Streaming/Channels/101",
            "H264",
            "1920x1080",
        )];
        let r = recommend_for_discovery(&profiles, SUPERVISOR_512X288);
        assert_eq!(r.main.as_ref().unwrap().token, "only");
        assert_eq!(r.analysis, Err(NoAnalysisStream::OnlyMain));
    }
}
