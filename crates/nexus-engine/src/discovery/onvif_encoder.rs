//! ONVIF video-encoder configuration client (`ver10/media`).
//!
//! Phase 7.6.2. Reads the camera's video-encoder configurations
//! ([`get_video_encoder_configurations`]), the option ranges the
//! operator UI clamps to ([`get_video_encoder_configuration_options`]),
//! and writes a modified configuration back
//! ([`set_video_encoder_configuration`]) — bitrate, frame rate,
//! GOP length, resolution and codec.
//!
//! The recommended operator flow is read-modify-write: fetch the
//! current configuration, change one field, send the whole thing
//! back (ONVIF's `VideoEncoderConfiguration` has required members,
//! so a partial write is rejected by the camera). Request builders
//! and response parsers are pure for fixture-based unit tests.
//!
//! The public operations are consumed by the operator
//! admin-passthrough routes added in Phase 7.6.3
//! (`crate::device_control`).

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use super::onvif_soap::{local_name, xml_escape, MEDIA1};

/// One video-encoder configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VideoEncoderConfig {
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_count: Option<u32>,
    /// `H264`, `H265`, `JPEG`, …
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_rate_limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding_interval: Option<u32>,
    /// kbps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gov_length: Option<u32>,
    /// H.264/H.265 profile (`Baseline` / `Main` / `High`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// An integer `[min, max]` range.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct IntRange {
    pub min: i32,
    pub max: i32,
}

/// A float `[min, max]` range.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct FloatRange {
    pub min: f32,
    pub max: f32,
}

/// The encoder option ranges (`GetVideoEncoderConfigurationOptions`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VideoEncoderOptions {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resolutions: Vec<(u32, u32)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality_range: Option<FloatRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_rate_range: Option<IntRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gov_length_range: Option<IntRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_range: Option<IntRange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub profiles_supported: Vec<String>,
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// List the camera's video-encoder configurations.
pub async fn get_video_encoder_configurations(
    endpoint: &str,
    username: &str,
    password: &str,
) -> Result<Vec<VideoEncoderConfig>, String> {
    let resp = MEDIA1
        .call(
            endpoint,
            username,
            password,
            "GetVideoEncoderConfigurations",
            "<trt:GetVideoEncoderConfigurations/>",
        )
        .await?;
    Ok(parse_configs(&resp))
}

/// Read the option ranges for an encoder configuration / profile.
pub async fn get_video_encoder_configuration_options(
    endpoint: &str,
    username: &str,
    password: &str,
    config_token: Option<&str>,
    profile_token: Option<&str>,
) -> Result<VideoEncoderOptions, String> {
    let mut body = String::from("<trt:GetVideoEncoderConfigurationOptions>");
    if let Some(t) = config_token {
        body.push_str(&format!(
            "<trt:ConfigurationToken>{}</trt:ConfigurationToken>",
            xml_escape(t)
        ));
    }
    if let Some(t) = profile_token {
        body.push_str(&format!(
            "<trt:ProfileToken>{}</trt:ProfileToken>",
            xml_escape(t)
        ));
    }
    body.push_str("</trt:GetVideoEncoderConfigurationOptions>");
    let resp = MEDIA1
        .call(
            endpoint,
            username,
            password,
            "GetVideoEncoderConfigurationOptions",
            &body,
        )
        .await?;
    Ok(parse_options(&resp))
}

/// Write a (read-modify-write) video-encoder configuration.
pub async fn set_video_encoder_configuration(
    endpoint: &str,
    username: &str,
    password: &str,
    config: &VideoEncoderConfig,
    force_persistence: bool,
) -> Result<(), String> {
    let body = set_config_body(config, force_persistence);
    MEDIA1
        .call(
            endpoint,
            username,
            password,
            "SetVideoEncoderConfiguration",
            &body,
        )
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Request-body builder (pure)
// ---------------------------------------------------------------------------

fn set_config_body(c: &VideoEncoderConfig, force_persistence: bool) -> String {
    let name = c
        .name
        .as_ref()
        .map(|n| format!("<tt:Name>{}</tt:Name>", xml_escape(n)))
        .unwrap_or_default();
    let use_count = c
        .use_count
        .map(|u| format!("<tt:UseCount>{u}</tt:UseCount>"))
        .unwrap_or_default();
    let encoding = c
        .encoding
        .as_ref()
        .map(|e| format!("<tt:Encoding>{}</tt:Encoding>", xml_escape(e)))
        .unwrap_or_default();
    let resolution = match (c.width, c.height) {
        (Some(w), Some(h)) => format!(
            "<tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution>"
        ),
        _ => String::new(),
    };
    let quality = c
        .quality
        .map(|q| format!("<tt:Quality>{q}</tt:Quality>"))
        .unwrap_or_default();
    let rate_control = if c.frame_rate_limit.is_some()
        || c.encoding_interval.is_some()
        || c.bitrate_limit.is_some()
    {
        format!(
            "<tt:RateControl>{}{}{}</tt:RateControl>",
            c.frame_rate_limit
                .map(|v| format!("<tt:FrameRateLimit>{v}</tt:FrameRateLimit>"))
                .unwrap_or_default(),
            c.encoding_interval
                .map(|v| format!("<tt:EncodingInterval>{v}</tt:EncodingInterval>"))
                .unwrap_or_default(),
            c.bitrate_limit
                .map(|v| format!("<tt:BitrateLimit>{v}</tt:BitrateLimit>"))
                .unwrap_or_default(),
        )
    } else {
        String::new()
    };
    // Codec-specific block keyed off the declared encoding.
    let codec = if c.gov_length.is_some() || c.profile.is_some() {
        let container = match c.encoding.as_deref() {
            Some("H265") => "H265",
            _ => "H264",
        };
        let profile_tag = if container == "H265" {
            "H265Profile"
        } else {
            "H264Profile"
        };
        format!(
            "<tt:{container}>{}{}</tt:{container}>",
            c.gov_length
                .map(|v| format!("<tt:GovLength>{v}</tt:GovLength>"))
                .unwrap_or_default(),
            c.profile
                .as_ref()
                .map(|p| format!("<tt:{profile_tag}>{}</tt:{profile_tag}>", xml_escape(p)))
                .unwrap_or_default(),
        )
    } else {
        String::new()
    };
    format!(
        "<trt:SetVideoEncoderConfiguration><trt:Configuration token=\"{}\">{}{}{}{}{}{}{}</trt:Configuration><trt:ForcePersistence>{}</trt:ForcePersistence></trt:SetVideoEncoderConfiguration>",
        xml_escape(&c.token),
        name,
        use_count,
        encoding,
        resolution,
        quality,
        rate_control,
        codec,
        force_persistence,
    )
}

// ---------------------------------------------------------------------------
// Response parsers (pure)
// ---------------------------------------------------------------------------

fn attr_str(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key.as_bytes())
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

const CONFIG_SECTIONS: &[&str] = &[
    "Resolution",
    "RateControl",
    "H264",
    "H265",
    "Mpeg4",
    "MPEG4",
];

fn nearest(stack: &[String], sections: &[&'static str]) -> Option<&'static str> {
    stack
        .iter()
        .rev()
        .find_map(|s| sections.iter().copied().find(|sec| *sec == s.as_str()))
}

fn parse_configs(body: &str) -> Vec<VideoEncoderConfig> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut out = Vec::new();
    let mut cur: Option<VideoEncoderConfig> = None;
    let mut capture: Option<String> = None;
    let mut acc = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(&e.name());
                if n == "Configurations" || n == "Configuration" {
                    cur = Some(VideoEncoderConfig {
                        token: attr_str(&e, "token").unwrap_or_default(),
                        ..VideoEncoderConfig::default()
                    });
                }
                stack.push(n.clone());
                capture = Some(n);
                acc.clear();
            }
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let n = local_name(&e.name());
                if capture.as_deref() == Some(n.as_str()) {
                    let v = acc.trim().to_string();
                    if !v.is_empty() {
                        if let Some(c) = cur.as_mut() {
                            let section =
                                nearest(&stack[..stack.len().saturating_sub(1)], CONFIG_SECTIONS);
                            apply_config_leaf(c, section, &n, &v);
                        }
                    }
                }
                capture = None;
                stack.pop();
                if n == "Configurations" || n == "Configuration" {
                    if let Some(c) = cur.take() {
                        if !c.token.is_empty() {
                            out.push(c);
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn apply_config_leaf(c: &mut VideoEncoderConfig, section: Option<&str>, leaf: &str, v: &str) {
    match section {
        None => match leaf {
            "Name" => c.name = Some(v.to_string()),
            "UseCount" => c.use_count = v.parse().ok(),
            "Encoding" => c.encoding = Some(v.to_string()),
            "Quality" => c.quality = v.parse().ok(),
            _ => {}
        },
        Some("Resolution") => match leaf {
            "Width" => c.width = v.parse().ok(),
            "Height" => c.height = v.parse().ok(),
            _ => {}
        },
        Some("RateControl") => match leaf {
            "FrameRateLimit" => c.frame_rate_limit = v.parse().ok(),
            "EncodingInterval" => c.encoding_interval = v.parse().ok(),
            "BitrateLimit" => c.bitrate_limit = v.parse().ok(),
            _ => {}
        },
        Some("H264") | Some("H265") => match leaf {
            "GovLength" => c.gov_length = v.parse().ok(),
            "H264Profile" | "H265Profile" => c.profile = Some(v.to_string()),
            _ => {}
        },
        _ => {}
    }
}

const RANGE_SECTIONS: &[&str] = &[
    "QualityRange",
    "FrameRateRange",
    "GovLengthRange",
    "BitrateRange",
    "EncodingIntervalRange",
    "ResolutionsAvailable",
];

fn parse_options(body: &str) -> VideoEncoderOptions {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut out = VideoEncoderOptions::default();
    let mut capture: Option<String> = None;
    let mut acc = String::new();
    let mut res_w: Option<u32> = None;
    let mut res_h: Option<u32> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = local_name(&e.name());
                if n == "ResolutionsAvailable" {
                    res_w = None;
                    res_h = None;
                }
                stack.push(n.clone());
                capture = Some(n);
                acc.clear();
            }
            Ok(Event::Text(t)) if capture.is_some() => {
                if let Ok(s) = t.unescape() {
                    acc.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let n = local_name(&e.name());
                if capture.as_deref() == Some(n.as_str()) {
                    let v = acc.trim().to_string();
                    if !v.is_empty() {
                        let section =
                            nearest(&stack[..stack.len().saturating_sub(1)], RANGE_SECTIONS);
                        apply_option_leaf(&mut out, section, &n, &v, &mut res_w, &mut res_h);
                    }
                }
                capture = None;
                stack.pop();
                if n == "ResolutionsAvailable" {
                    if let (Some(w), Some(h)) = (res_w.take(), res_h.take()) {
                        out.resolutions.push((w, h));
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn apply_option_leaf(
    out: &mut VideoEncoderOptions,
    section: Option<&str>,
    leaf: &str,
    v: &str,
    res_w: &mut Option<u32>,
    res_h: &mut Option<u32>,
) {
    match section {
        Some("ResolutionsAvailable") => match leaf {
            "Width" => *res_w = v.parse().ok(),
            "Height" => *res_h = v.parse().ok(),
            _ => {}
        },
        Some("QualityRange") => {
            if let Ok(f) = v.parse::<f32>() {
                let r = out.quality_range.get_or_insert_with(FloatRange::default);
                apply_float_bound(r, leaf, f);
            }
        }
        Some("FrameRateRange") => apply_int(
            out.frame_rate_range.get_or_insert_with(IntRange::default),
            leaf,
            v,
        ),
        Some("GovLengthRange") => apply_int(
            out.gov_length_range.get_or_insert_with(IntRange::default),
            leaf,
            v,
        ),
        Some("BitrateRange") => apply_int(
            out.bitrate_range.get_or_insert_with(IntRange::default),
            leaf,
            v,
        ),
        _ => {
            // Top-level (no range section) profile lists.
            if leaf == "H264ProfilesSupported" || leaf == "H265ProfilesSupported" {
                out.profiles_supported.push(v.to_string());
            }
        }
    }
}

fn apply_float_bound(r: &mut FloatRange, bound: &str, val: f32) {
    match bound {
        "Min" => r.min = val,
        "Max" => r.max = val,
        _ => {}
    }
}

fn apply_int(r: &mut IntRange, bound: &str, v: &str) {
    if let Ok(val) = v.parse::<i32>() {
        match bound {
            "Min" => r.min = val,
            "Max" => r.max = val,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(inner: &str) -> String {
        format!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:trt="http://www.onvif.org/ver10/media/wsdl"><s:Body>{inner}</s:Body></s:Envelope>"#
        )
    }

    fn assert_well_formed(xml: &str) {
        let mut r = Reader::from_str(xml);
        let mut buf = Vec::new();
        loop {
            match r.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Err(e) => panic!("invalid xml: {e}\n{xml}"),
                _ => {}
            }
        }
    }

    #[test]
    fn parses_video_encoder_configurations() {
        let body = wrap(
            r#"<trt:GetVideoEncoderConfigurationsResponse>
                <trt:Configurations token="VideoEncoderToken_1">
                    <tt:Name>VideoEncoder_1</tt:Name>
                    <tt:UseCount>1</tt:UseCount>
                    <tt:Encoding>H264</tt:Encoding>
                    <tt:Resolution><tt:Width>1920</tt:Width><tt:Height>1080</tt:Height></tt:Resolution>
                    <tt:Quality>5</tt:Quality>
                    <tt:RateControl><tt:FrameRateLimit>25</tt:FrameRateLimit><tt:EncodingInterval>1</tt:EncodingInterval><tt:BitrateLimit>4096</tt:BitrateLimit></tt:RateControl>
                    <tt:H264><tt:GovLength>50</tt:GovLength><tt:H264Profile>High</tt:H264Profile></tt:H264>
                </trt:Configurations>
            </trt:GetVideoEncoderConfigurationsResponse>"#,
        );
        let cfgs = parse_configs(&body);
        assert_eq!(cfgs.len(), 1);
        let c = &cfgs[0];
        assert_eq!(c.token, "VideoEncoderToken_1");
        assert_eq!(c.name.as_deref(), Some("VideoEncoder_1"));
        assert_eq!(c.encoding.as_deref(), Some("H264"));
        assert_eq!(c.width, Some(1920));
        assert_eq!(c.height, Some(1080));
        assert_eq!(c.quality, Some(5.0));
        assert_eq!(c.frame_rate_limit, Some(25));
        assert_eq!(c.bitrate_limit, Some(4096));
        assert_eq!(c.gov_length, Some(50));
        assert_eq!(c.profile.as_deref(), Some("High"));
    }

    #[test]
    fn parses_encoder_options() {
        let body = wrap(
            r#"<trt:GetVideoEncoderConfigurationOptionsResponse><trt:Options>
                <tt:QualityRange><tt:Min>1</tt:Min><tt:Max>6</tt:Max></tt:QualityRange>
                <tt:H264>
                    <tt:ResolutionsAvailable><tt:Width>1920</tt:Width><tt:Height>1080</tt:Height></tt:ResolutionsAvailable>
                    <tt:ResolutionsAvailable><tt:Width>1280</tt:Width><tt:Height>720</tt:Height></tt:ResolutionsAvailable>
                    <tt:GovLengthRange><tt:Min>1</tt:Min><tt:Max>400</tt:Max></tt:GovLengthRange>
                    <tt:FrameRateRange><tt:Min>1</tt:Min><tt:Max>30</tt:Max></tt:FrameRateRange>
                    <tt:H264ProfilesSupported>Baseline</tt:H264ProfilesSupported>
                    <tt:H264ProfilesSupported>High</tt:H264ProfilesSupported>
                </tt:H264>
                <tt:Extension><tt:H264><tt:BitrateRange><tt:Min>32</tt:Min><tt:Max>16384</tt:Max></tt:BitrateRange></tt:H264></tt:Extension>
            </trt:Options></trt:GetVideoEncoderConfigurationOptionsResponse>"#,
        );
        let o = parse_options(&body);
        assert_eq!(o.quality_range, Some(FloatRange { min: 1.0, max: 6.0 }));
        assert_eq!(o.resolutions, vec![(1920, 1080), (1280, 720)]);
        assert_eq!(o.gov_length_range, Some(IntRange { min: 1, max: 400 }));
        assert_eq!(o.frame_rate_range, Some(IntRange { min: 1, max: 30 }));
        assert_eq!(
            o.bitrate_range,
            Some(IntRange {
                min: 32,
                max: 16384
            })
        );
        assert_eq!(o.profiles_supported, vec!["Baseline", "High"]);
    }

    #[test]
    fn set_config_body_round_trips_modified_fields() {
        let c = VideoEncoderConfig {
            token: "VideoEncoderToken_1".into(),
            name: Some("VideoEncoder_1".into()),
            use_count: Some(1),
            encoding: Some("H264".into()),
            width: Some(1280),
            height: Some(720),
            quality: Some(4.0),
            frame_rate_limit: Some(15),
            encoding_interval: Some(1),
            bitrate_limit: Some(2048),
            gov_length: Some(30),
            profile: Some("Main".into()),
        };
        let body = set_config_body(&c, true);
        assert_well_formed(&wrap(&body));
        assert!(body.contains("token=\"VideoEncoderToken_1\""), "{body}");
        assert!(body.contains("<tt:Width>1280</tt:Width>"), "{body}");
        assert!(
            body.contains("<tt:BitrateLimit>2048</tt:BitrateLimit>"),
            "{body}"
        );
        assert!(body.contains("<tt:H264><tt:GovLength>30</tt:GovLength><tt:H264Profile>Main</tt:H264Profile></tt:H264>"), "{body}");
        assert!(
            body.contains("<trt:ForcePersistence>true</trt:ForcePersistence>"),
            "{body}"
        );
    }

    #[test]
    fn set_config_body_uses_h265_profile_tag_for_h265() {
        let c = VideoEncoderConfig {
            token: "T".into(),
            encoding: Some("H265".into()),
            gov_length: Some(60),
            profile: Some("Main".into()),
            ..VideoEncoderConfig::default()
        };
        let body = set_config_body(&c, false);
        assert!(body.contains("<tt:H265><tt:GovLength>60</tt:GovLength><tt:H265Profile>Main</tt:H265Profile></tt:H265>"), "{body}");
    }
}
